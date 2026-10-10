// Editor-side git integration (`:git ...` in bishedit command mode -- see
// repl.rs's own run_command_mode `"git"` arm). Bish has zero external
// dependencies and no git object-database implementation of its own, so
// every one of these shells out to the user's real `git` executable --
// exactly the "let a real external tool do the heavy lifting" choice this
// project already makes elsewhere (e.g. `mise`/`nvm`-style shell
// activation scripts), just invoked directly by the editor here instead of
// something the user's own shell config chooses to run. `available()` is
// what makes every other function in here optional rather than a hard
// dependency: a missing `git` on $PATH just means these features quietly
// don't work, not a broken build/editor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// Every `git` this module runs, built in one place so that what it
// prints is decided here and not by the machine bish happens to be on.
//
// bish parses this output -- a `--format` with 0x1f separators, a
// porcelain status, a line-porcelain blame -- and a developer's own
// configuration can change all of it. `log.showSignature = true`, which
// anyone who signs their commits may well have, puts gpg's verification
// lines in front of the commit `log` was asked to print, straight into
// the first field bish splits out. `color.ui = always` writes escape
// codes through output meant for a machine. `log.date` re-renders the
// `%ad` the `:git show` header asks for. `core.quotepath` decides
// whether a non-ASCII path arrives escaped.
//
// Three environment variables go the other way: `GIT_DIR`,
// `GIT_WORK_TREE` and `GIT_INDEX_FILE` override the directory the
// command runs in, and every process a git hook starts inherits them --
// so a prompt drawn from a shell inside a hook, or `:git log` in an
// editor opened from one, would answer about that repository instead of
// the one on screen.
//
// What is *not* overridden is the rest: an alias, an `includeIf`, a
// `safe.directory`, a mailmap are all the repository's own business and
// bish asks git the same questions a terminal would.
//
// Except for the config keys whose values are *commands*. bish runs git
// by itself -- on every prompt, when Ctrl+T opens, when a buffer wants a
// blame gutter -- so a repository's own config becomes code execution on
// nothing more than `cd`, which is the bug fish shipped as
// CVE-2022-20001 and powerlevel10k shipped twice. Demonstrated here
// before this list existed: `core.fsmonitor = "touch /tmp/x; false"` in
// a repository's `.git/config` ran on entering the directory, and again
// from the opener, and again from the blame gutter.
//
// `safe.directory` is git's own answer and does not cover this: it
// refuses a repository owned by *another* user, and the way this arrives
// is an archive you extracted yourself. A person typing `git status`
// chose to run git there; a prompt did not.
//
// So every key below is one git would hand to a shell, and `-c` beats
// the repository's config file. What this cannot reach is a driver the
// repository *names* itself -- `filter.<whatever>.clean` selected by a
// tracked `.gitattributes`, and `diff.<whatever>.textconv` the same way.
// Config subsections do not glob, so there is no `-c` to write in
// advance, and the one blunt instrument that works -- pointing
// `GIT_ATTR_SOURCE` at an empty tree, which was measured to stop it --
// also throws away `text=auto` and every other eol rule, so a clean
// repository would start reporting itself dirty. That half is still
// open and is written down here rather than left to be rediscovered.
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
            // Every one of these is a command git would run. None of
            // them is wanted by anything bish asks for: it reads, it
            // never fetches, and it never pages.
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

// Checked fresh on every `:git` invocation rather than cached once at
// startup -- a subprocess spawn is cheap and this only runs when a user
// actually types a `:git` command, not on every keystroke -- so installing
// or removing `git` mid-session is picked up immediately.
pub fn available() -> bool {
    command(Path::new(".")).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

// One command prompt's worth of "where does this repo's HEAD point, and
// is the working tree dirty" (prompt.rs's own git segment). `branch` is
// the checked-out branch name, or (rare -- detached HEAD) the short
// commit hash instead, matching what a human glancing at `git status`
// would call it either way. `dirty` is true iff there's anything beyond
// a clean `git status` (staged, unstaged, or untracked).
pub struct HeadStatus {
    pub branch: String,
    pub dirty: bool,
}

// One `git status --porcelain=v2 --branch` call covers both branch name
// (the `# branch.head` line) and dirty (whether any non-`#` line
// follows it) at once, rather than two separate git invocations per
// prompt render -- still a real subprocess spawn on every new prompt
// line though (prompt::render's own call site), same accepted cost
// real bash-prompt git plugins (starship, oh-my-zsh's git-prompt, ...)
// all have; a config knob to disable it, or caching against some
// "did HEAD/the index change" signal, is a reasonable follow-up if it
// ever shows up as noticeable latency in practice, not attempted here.
// `None` covers "git not installed" and "not inside a repo" alike --
// prompt.rs's own caller treats both the same (no segment shown), so
// there's no need to tell them apart.
pub fn head_status(dir: &Path, trusted: bool) -> Option<HeadStatus> {
    // In a directory you have not vouched for, bish runs no git in the
    // working tree at all -- not even to ask whether it is dirty.
    // Deciding dirtiness is what runs a repository's `clean` filter (see
    // `command`), so the branch is read out of `.git/HEAD` directly
    // instead, with no subprocess for any repo-configured driver to hook.
    // The cost is honest and visible: an untrusted repo shows its branch
    // and no dirty marker, until `::bish trust` says more is wanted.
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
            // "(detached)" while HEAD isn't on any branch -- fall back to
            // the short commit hash below instead, same as `git status`
            // itself would tell a human ("HEAD detached at <sha>").
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

/// The branch name from `.git/HEAD`, read as a plain file rather than
/// asked of git -- the untrusted `head_status` path, where the point is
/// that no git process touches the working tree.
///
/// `None` when `dir` is not in a repository, so the prompt shows no git
/// segment there, exactly as it does when `git status` returns nothing.
/// A detached HEAD comes back as the short hash, matching what the
/// trusted path's `short_head` would have shown.
///
/// It reimplements only the two steps git would take to answer this one
/// question: find the git directory, read HEAD. HEAD is always a loose
/// file (git never packs it), so there is no ref database to consult --
/// a `ref: refs/heads/NAME` line or a bare object id, and nothing else.
fn branch_from_head(dir: &Path) -> Option<String> {
    let git_dir = find_git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        // `ref: refs/heads/main` -> `main`; any other ref target shows
        // its last component, which is what a human reads off it too.
        Some(target) => target.rsplit('/').next().map(str::to_string).filter(|s| !s.is_empty()),
        // Detached: a bare object id. Abbreviated to git's own default
        // length, so it reads the same as the trusted path's answer.
        None if head.chars().all(|c| c.is_ascii_hexdigit()) && head.len() >= 7 => Some(head[..head.len().min(8)].to_string()),
        None => None,
    }
}

/// The repository's git directory for `dir`, by walking up and looking
/// for `.git` -- the same search git does, reduced to what reading HEAD
/// needs. `.git` is usually a directory; in a linked worktree or a
/// submodule it is a file holding `gitdir: <path>`, which may be
/// relative to the `.git` file's own directory.
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

// One buffer line's worth of `git blame` info -- deliberately minimal
// (just enough for a gutter cell, not every field real `git blame`
// reports): `short_commit` is the first 8 hex digits (matching `git`'s own
// default abbreviation length), `date` is the commit's author-time
// formatted as `YYYY-MM-DD` (see format_unix_date below for why no
// external date/time crate is needed for this). An uncommitted working-
// tree line comes back with `short_commit` "00000000" and author "Not
// Committed Yet" -- real `git blame --line-porcelain`'s own convention for
// that case, not something this parses specially.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameLine {
    pub short_commit: String,
    pub author: String,
    pub date: String,
}

// Runs `git blame --line-porcelain` against `path`, one BlameLine per line
// of the file in order. Always run from `path`'s own parent directory
// with just its filename as the argument (rather than passing `path`
// itself, possibly relative to bish's own cwd, straight through) -- keeps
// this correct regardless of where bish's own process cwd happens to be
// relative to the repo, the same way a real terminal `git blame` run from
// that file's own directory would resolve. `Err` covers both "git itself
// failed" (not a repo, file not tracked, path doesn't exist, ...) and a
// malformed/unexpected porcelain response.
pub fn blame(path: &Path, rev: Option<&str>, trusted: bool) -> Result<Vec<BlameLine>, String> {
    // `git blame` runs the repository's `clean` filter on the working
    // copy and its `textconv` on blobs (both measured), so it is gated
    // on the same `git` capability the prompt is: in an untrusted
    // directory there is no blame gutter until `::bish trust` grants it.
    if !trusted {
        return Err("blame: this directory is not trusted (`::bish trust` to allow)".to_string());
    }
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().ok_or_else(|| "no filename".to_string())?;
    let mut blame = command(dir);
    blame.arg("blame").arg("--line-porcelain");
    if let Some(rev) = rev {
        // Refused here rather than fenced off with
        // `--end-of-options`, which is how `log` and `show` do it and
        // does not work for this command: `git blame
        // --end-of-options <rev> -- <path>` stops treating the `--` as
        // the revision/path separator, so the path is read as a second
        // revision and the whole call fails with "bad revision". Tried,
        // measured, and the separator is the more important of the two
        // -- a branch and a file can share a name, which is what the
        // comment below is about.
        //
        // A revision is a word somebody typed at `:blame`, so the only
        // thing that needed closing is one shaped like an option.
        if rev.starts_with('-') {
            return Err(format!("bad revision '{rev}'"));
        }
        blame.arg(rev);
    }
    // `--` before the path, always: without it a revision and a filename
    // are told apart by guesswork, and a branch and a file can share a
    // name.
    let output = blame.arg("--").arg(filename).output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(first_stderr_line(&output.stderr, "git blame failed"));
    }
    parse_line_porcelain(&String::from_utf8_lossy(&output.stdout))
}

// The committed content of `path` at `rev` -- the *other* half of what
// blame and diff need, and the reason both work against a modified
// buffer at all: knowing what the file looked like there is what lets
// bish line those results up with what's actually on screen (see
// `align_to`), rather than assuming the buffer still matches whatever
// git was asked about.
//
// `rev` of `None` means the index -- `git show :path` is git's own
// spelling for it, and the right default because a plain `git diff`
// compares the worktree against the index too, not against HEAD.
//
// `Ok(None)` when the file simply isn't there at that revision (not yet
// added, or deleted since): a real answer, not a failure -- every line is
// then new, which is exactly what the caller should show.
//
// The two genuine failures -- no repository at all, and a revision that
// doesn't resolve -- are each checked with their own small `git` call
// first, rather than sorting them out of `git show`'s own error message
// afterwards. `git show` reports all three cases as one indistinguishable
// "fatal: ambiguous argument", so a typo'd revision would otherwise read
// as "the file isn't in it" and quietly show every line as new.
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
            // --quiet means git printed nothing of its own to relay.
            return Err(format!("unknown revision '{rev}'"));
        }
    }

    // `./` makes the path cwd-relative rather than repo-root-relative,
    // which is what lets this run from the file's own directory like
    // every other call here.
    let mut spec = std::ffi::OsString::from(rev.unwrap_or(""));
    spec.push(":./");
    spec.push(filename);
    let output = command(dir).arg("show").arg(&spec).output().map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        // Both real failures are already ruled out above, so what's left
        // is "that path isn't in there".
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

// One `git` call from `dir`, its stdout on success and the first line of
// what it said on failure.
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

/// What `rev` is, from anywhere inside the repository at `dir`: its
/// header, its parent, and the files it changed against that parent.
///
/// A merge is shown against its first parent -- one diff rather than
/// git's combined one, which has no single "before" to put beside
/// "after" -- and `merge` says so. Renames are followed (`-M`), so a
/// moved file is one entry rather than a deletion and an addition.
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

/// Every file git tracks at or below `dir`, as paths relative to `dir`.
///
/// Relative to `dir` rather than to the top level because this is a list
/// to put in front of somebody: a path they can read, and the one they
/// would have typed from where they are. `git ls-files` already answers
/// that way when run from a subdirectory.
///
/// Nothing here consults `.gitignore`: an ignored file is by definition
/// not tracked, so the question never comes up.
pub fn tracked_files(dir: &Path) -> Result<Vec<String>, String> {
    // -z because a tracked filename may contain a newline, and git's
    // default quoting of one would hand back a path that no longer opens.
    Ok(git(dir, &["ls-files", "-z"])?.split(|b| *b == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect())
}

/// The directories those files are in, nearest the top first.
///
/// Derived from the file list rather than asked for, because git does not
/// track directories at all -- a directory exists exactly as long as
/// something in it does, which makes the parents of the tracked files the
/// honest answer to "which directories does git know about".
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
    // Absolute, so it names the same file from the top level as from
    // wherever the buffer is; git takes a path inside the work tree
    // either way.
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

// `git log` in the format `log` asks for: each commit ended by 0x1e, its
// fields split by 0x1f -- two bytes no hash, date, name or subject has
// any use for, which a tab or a newline could not promise.
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

// `git diff-tree --name-status -z`: a status, then one path, or two for a
// rename or copy, every field ended by a NUL. The NULs are what let a
// path hold a tab or a newline without being misread.
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

// Lines up per-line results computed against `old` with the buffer's own
// `new` lines, by diffing the two: entry `i` of the result is whatever
// `old`-side line buffer line `i` came from, or `None` for a line that
// isn't in `old` at all.
//
// This is what makes `:git blame` work on a modified buffer and against
// an arbitrary revision at once -- the two are the same problem. Blame
// describes some committed version of the file; the buffer on screen is
// something else (edited since, or simply a later revision), and without
// this the two are lined up by line number, which is wrong the moment
// anything above shifted.
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

// `--line-porcelain` repeats every commit's full metadata for every line
// it covers (unlike plain `--porcelain`, which omits it for a repeat
// commit already shown) -- picked specifically so this parse never needs
// to carry state forward from an earlier record, just read one self-
// contained group at a time: a header line (`<sha> <orig-line>
// <final-line> [<count>]`), then `key value...` lines until the literal
// file content line (always exactly one tab followed by that line's
// text, even when the line itself is empty), which ends the group.
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
            // Every other key (author-mail/committer*/summary/previous/
            // filename/boundary) is real git blame output too, just not
            // needed for this gutter's own minimal display -- skipped.
        }
        result.push(BlameLine { short_commit: sha.chars().take(8).collect(), author, date: format_unix_date(author_time) });
    }
    Ok(result)
}

// `:git diff`'s own per-line marker -- which kind of change (relative to
// this file's tracked state) a given 0-indexed buffer line falls under.
// `Removed` doesn't mark a line that itself changed (there isn't one --
// the old lines are just gone) but the single nearest surviving line the
// deletion sits next to, matching real diff-gutter plugins' own
// convention (see `diff`'s own doc comment for exactly which line that
// is and why).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffMark {
    Added,
    Changed,
    Removed,
}

// Every diff gutter in the editor, computed here rather than by parsing
// `git diff`'s own text output: a DiffMark keyed by 0-indexed *new*-side
// line, built from `crate::diff::diff`'s own edit script. Both callers
// (`fileeditor::toggle_buffer_diff`'s "buffer vs. what's on disk" and
// `toggle_git_diff`'s "buffer vs. some revision") hand it two plain
// slices of lines, which is why neither needs a git repository for the
// diffing itself and why an unsaved buffer diffs correctly -- the old
// side is just whatever the caller fetched, and the new side is the
// buffer as it stands.
//
// A Delete immediately followed by an Insert (no Equal between them --
// `crate::diff::diff`'s own coalescing already merges any run of same-
// kind steps, so this is the only way to see both back to back) is one
// "changed" hunk, same as a unified diff's own old_count>0/new_count>0
// hunk; an unpaired Delete marks the single nearest surviving new-side
// line (wherever the very next op's own `b` picks back up, or the very
// end of the file if this Delete is the last op) as Removed -- the same
// attachment line real `git diff -U0` picks, verified against it in
// marks_from_diff_places_a_deletion_at_the_same_line_git_itself_does.
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

// git blame's author-time, as a plain date. This used to carry its own
// copy of `struct tm` and its own `localtime_r` declaration, with a
// comment explaining that one small FFI need did not justify a
// cross-module dependency. That was true while the only other copy was
// a private function sixteen thousand lines into exec.rs; `time.rs`
// exists now, so it isn't.
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

    // The prompt runs git in whatever directory you entered, so a
    // repository's own config must not be able to run a command. This is
    // the vector fish shipped as CVE-2022-20001; it was live here until
    // `command` started overriding the keys whose values git executes.
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
        // `false` afterwards so git falls back to its ordinary scan: the
        // point is whether the command ran at all, not what it answered.
        run(&["config", "core.fsmonitor", &format!("touch {}; false", marker.display())]);

        // Every automatic query, not just the prompt's: the opener asks
        // for the file list and a buffer asks for blame.
        let status = head_status(&dir, true);
        let files = tracked_files(&dir);
        let blamed = blame(&dir.join("a.txt"), None, true);

        let ran = marker.exists();
        std::fs::remove_dir_all(&dir).ok();
        assert!(!ran, "a repository's `core.fsmonitor` was executed by bish asking about it");
        // ...and the answers still arrived, which is the half that makes
        // this a fix rather than a removal.
        assert_eq!(status.map(|s| s.branch), Some("main".to_string()));
        assert_eq!(files.unwrap(), ["a.txt"]);
        assert_eq!(blamed.unwrap().len(), 1);
    }

    // `:blame <rev>` passes a typed word to `git blame`, where one
    // beginning with a dash would be an option rather than a revision.
    // `--end-of-options` is how the other commands here fence that off
    // and cannot be used for this one: it makes `blame` stop treating
    // `--` as the revision/path separator, so the path becomes a second
    // revision and the call fails outright.
    // The trust gate: in an untrusted directory bish runs no git in the
    // working tree, so a repository's own `clean` filter never executes
    // on a prompt draw. The branch still comes back -- read out of
    // .git/HEAD -- and `dirty` is always false, because deciding
    // dirtiness is the part that would have run the filter.
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
        // Dirty the tree so a trusted status would have something to
        // filter, and bust the stat cache the same way.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();

        let status = head_status(&dir, false).expect("branch comes from HEAD");
        assert_eq!(status.branch, "main");
        assert!(!status.dirty, "an untrusted repo reports no dirtiness");
        let ran = marker.exists();

        // Blame is gated too: no gutter in an untrusted repo.
        let blamed = blame(&dir.join("a.txt"), None, false);

        std::fs::remove_dir_all(&dir).ok();
        assert!(!ran, "an untrusted directory ran git in its working tree");
        assert!(blamed.is_err(), "blame must refuse an untrusted directory");
    }

    // The HEAD reader behind the untrusted path, on the forms it has to
    // read without git's help.
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

        // On a branch, read from a subdirectory: finds the git dir by
        // walking up.
        assert_eq!(branch_from_head(&dir.join("sub")).as_deref(), Some("main"));
        // Not a repository at all: nothing, so the prompt shows no
        // git segment.
        assert_eq!(branch_from_head(std::path::Path::new("/")), None);
        // Detached: the short hash, as the trusted path would show.
        run(&["checkout", "-q", "--detach"]);
        let detached = branch_from_head(&dir).expect("detached still has a HEAD");
        assert_eq!(detached.len(), 8);
        assert!(detached.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::remove_dir_all(&dir).ok();
    }

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
        // And the ordinary pair still work, which is what the refusal
        // must not cost: a named revision, and none at all.
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
        // Not added, so not tracked, and nothing to do with .gitignore.
        std::fs::write(dir.join("src/scratch.rs"), "x\n").unwrap();
        run(&["add", "README.md", "src/repl.rs", "src/platform/unix.rs"]);
        run(&["commit", "-q", "-m", "initial"]);

        let files = tracked_files(&dir).unwrap();
        assert_eq!(files, ["README.md", "src/platform/unix.rs", "src/repl.rs"]);
        assert_eq!(tracked_directories(&files), ["src", "src/platform"], "every parent, and the top level is not one of them");
        // From a subdirectory: the paths somebody standing there would
        // type, and only what is at or below them.
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

    // bish parses what git prints, and a repository carries
    // configuration of its own that changes it. `-c` on the command line
    // outranks every config file, which is what `command` relies on.
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
        // Each of these is a real setting someone has: a date format
        // that rewrites the `%ad` the header asks for, colour in output
        // meant for a machine, and signature lines in front of a commit.
        crate::gittest::run(dir, &["config", "log.date", "raw"]);
        crate::gittest::run(dir, &["config", "color.ui", "always"]);
        crate::gittest::run(dir, &["config", "log.showSignature", "true"]);

        let entries = log(dir, None, None).unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].subject, "only commit", "a colour escape would land in the subject");
        assert_eq!(entries[0].date.len(), "2026-10-06".len(), "`--date=short` decides the date, not the repository: {:?}", entries[0].date);

        let commit = show(dir, "HEAD").unwrap();
        let date = commit.header.iter().find(|l| l.starts_with("Date:")).expect("a Date line").clone();
        // `log.date = raw` renders the date as seconds-since-the-epoch
        // and an offset, so a letter in the rendered part -- past the
        // "Date:" label, which has letters of its own -- is proof the
        // default format survived.
        let rendered = date.trim_start_matches("Date:").trim().to_string();
        assert!(rendered.chars().any(|c| c.is_ascii_alphabetic()), "the header date is still git's default format: {rendered:?}");
        assert!(!commit.header.iter().any(|l| l.contains('\u{1b}')), "no escape codes anywhere in it: {:?}", commit.header);
    }

    // A git hook exports `GIT_DIR`, and every process started from one
    // inherits it -- a shell, an editor opened from that shell, a prompt
    // drawn in it. It overrides the directory a `git` command runs in, so
    // without `command` dropping it, `:git log` in an editor opened from
    // a hook would list the hook's repository.
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

        // SAFETY: every `git` this suite runs -- bish's own `command`
        // here and `gittest`'s fixtures -- drops these three, which is
        // the thing being tested, so no concurrent test can be derailed
        // by them. Restored immediately either way.
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

    // The three deletion-attachment points (start/middle/end of file),
    // each checked against a real `git diff --no-color -U0` run first
    // rather than assumed -- this is the only part of a diff gutter
    // where "which line does a *removal* belong to" has a non-obvious
    // answer, and matching git's own choice is what makes the markers
    // read the way anyone used to a diff gutter expects.
    #[test]
    fn marks_from_diff_places_a_deletion_at_the_same_line_git_itself_does() {
        // Middle: [a,b,c,d] -> [a,d] -- real `git diff -U0` attaches
        // this to new-side line 0 ("a"), the line right before the gap.
        assert_eq!(marks_from_diff(&["a", "b", "c", "d"], &["a", "d"]), HashMap::from([(0, DiffMark::Removed)]));
        // End: [a,b,c,d] -> [a,b,c] -- attaches to the new last line.
        assert_eq!(marks_from_diff(&["a", "b", "c", "d"], &["a", "b", "c"]), HashMap::from([(2, DiffMark::Removed)]));
        // Start: [a,b,c,d] -> [b,c,d] -- no line precedes the gap, so
        // this attaches to the new first line instead.
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
        // format_unix_date reads the process's own local timezone via
        // localtime_r, so this pins TZ to UTC first (and calls tzset() so
        // glibc actually notices the change) rather than depending on
        // whatever the test-running machine happens to be configured
        // with -- same reasoning this project's own verification always
        // runs the full suite with --test-threads=1 (a process-wide env
        // var like TZ isn't safe to mutate from a test that might run
        // concurrently with another one reading it).
        unsafe { std::env::set_var("TZ", "UTC") };
        crate::platform::reload_timezone();
        assert_eq!(format_unix_date(0), "1970-01-01");
        // 1700000000 is a widely-cited round Unix timestamp: 2023-11-14
        // 22:13:20 UTC.
        assert_eq!(format_unix_date(1_700_000_000), "2023-11-14");
    }
}
