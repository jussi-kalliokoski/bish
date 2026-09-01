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
use std::path::Path;
use std::process::{Command, Stdio};

// Checked fresh on every `:git` invocation rather than cached once at
// startup -- a subprocess spawn is cheap and this only runs when a user
// actually types a `:git` command, not on every keystroke -- so installing
// or removing `git` mid-session is picked up immediately.
pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
pub fn head_status(dir: &Path) -> Option<HeadStatus> {
    let output = Command::new("git").arg("status").arg("--porcelain=v2").arg("--branch").current_dir(dir).stdin(Stdio::null()).output().ok()?;
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
    let output = Command::new("git").arg("rev-parse").arg("--short").arg("HEAD").current_dir(dir).stdin(Stdio::null()).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
pub fn blame(path: &Path, rev: Option<&str>) -> Result<Vec<BlameLine>, String> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().ok_or_else(|| "no filename".to_string())?;
    let mut command = Command::new("git");
    command.arg("blame").arg("--line-porcelain");
    if let Some(rev) = rev {
        command.arg(rev);
    }
    // `--` before the path, always: without it a revision and a filename
    // are told apart by guesswork, and a branch and a file can share a
    // name.
    let output = command.arg("--").arg(filename).current_dir(dir).stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
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

    let in_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !in_repo.status.success() {
        return Err(first_stderr_line(&in_repo.stderr, "not a git repository"));
    }
    if let Some(rev) = rev {
        let resolved = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .arg(rev)
            .current_dir(dir)
            .stdin(Stdio::null())
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
    let output = Command::new("git")
        .arg("show")
        .arg(&spec)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        // Both real failures are already ruled out above, so what's left
        // is "that path isn't in there".
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
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
        unsafe extern "C" {
            fn tzset();
        }
        unsafe {
            std::env::set_var("TZ", "UTC");
            tzset();
        }
        assert_eq!(format_unix_date(0), "1970-01-01");
        // 1700000000 is a widely-cited round Unix timestamp: 2023-11-14
        // 22:13:20 UTC.
        assert_eq!(format_unix_date(1_700_000_000), "2023-11-14");
    }
}
