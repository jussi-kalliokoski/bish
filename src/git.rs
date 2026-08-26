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
pub fn blame(path: &Path) -> Result<Vec<BlameLine>, String> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let filename = path.file_name().ok_or_else(|| "no filename".to_string())?;
    let output = Command::new("git")
        .arg("blame")
        .arg("--line-porcelain")
        .arg(filename)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        return Err(first_stderr_line(&output.stderr, "git blame failed"));
    }
    parse_line_porcelain(&String::from_utf8_lossy(&output.stdout))
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

// Runs a unified diff against `path`'s tracked content -- `git diff -U0`
// against HEAD/the index for a tracked file, or `git diff --no-index`
// against `/dev/null` for one git doesn't know about yet (so a brand new,
// not-yet-added file still shows every line as freshly Added, matching
// real diff-gutter plugins' own "new file" convention, rather than
// showing nothing just because there's no commit to diff against). Like
// `blame`, this only ever reflects `path`'s content *on disk* -- there's
// no live-buffer-vs-HEAD diffing here, so a dirty, unsaved buffer would
// show a diff that doesn't match what's on screen; the caller
// (fileeditor::toggle_git_diff) is what actually refuses that case, same
// as it does for blame.
// Explicitly checks "is this even inside a git repository" up front
// (`git rev-parse --is-inside-work-tree`) rather than letting a plain
// `git diff` outside one fail on its own: unlike a tracked-file diff
// (which fails loudly by itself when there's no repo at all), the
// untracked/`--no-index` branch below would otherwise *succeed* even
// with no repository anywhere (that's the whole point of `--no-index`),
// silently masking exactly the case `blame`'s own "not a git repository"
// error surfaces -- this keeps the two features' error behavior
// consistent instead of diff quietly doing something blame refuses.
pub fn diff(path: &Path) -> Result<HashMap<usize, DiffMark>, String> {
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

    let tracked = Command::new("git")
        .arg("ls-files")
        .arg("--error-unmatch")
        .arg(filename)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    let output = if tracked {
        Command::new("git").args(["diff", "--no-color", "-U0", "--"]).arg(filename).current_dir(dir).stdin(Stdio::null()).output()
    } else {
        Command::new("git")
            .args(["diff", "--no-color", "-U0", "--no-index", "--", "/dev/null"])
            .arg(filename)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
    }
    .map_err(|e| format!("git: {e}"))?;

    // A plain tracked-file `git diff` (no --exit-code given) always exits
    // 0 regardless of whether there were any differences; `--no-index`
    // instead always uses 0 (identical)/1 (differences found) as its own
    // normal pair, differences or not, per its own --help. Either way,
    // only something beyond that small range is a real failure worth
    // surfacing (a corrupt repo, a permissions error, ...).
    if output.status.code().is_none_or(|c| !(0..=1).contains(&c)) {
        return Err(first_stderr_line(&output.stderr, "git diff failed"));
    }

    Ok(parse_unified_diff(&String::from_utf8_lossy(&output.stdout)))
}

fn first_stderr_line(stderr: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    text.lines().next().unwrap_or(fallback).trim().to_string()
}

fn parse_unified_diff(text: &str) -> HashMap<usize, DiffMark> {
    let mut marks = HashMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("@@ -") else { continue };
        let Some((old_count, new_start, new_count)) = parse_hunk_rest(rest) else { continue };
        if new_count == 0 {
            // See this function's own doc comment on `diff` for why
            // `new_start.saturating_sub(1)` is the right line in every
            // case (deletion at the very start of the file, in the
            // middle, or at the end).
            marks.insert(new_start.saturating_sub(1), DiffMark::Removed);
        } else {
            let mark = if old_count == 0 { DiffMark::Added } else { DiffMark::Changed };
            for i in 0..new_count {
                marks.insert(new_start - 1 + i, mark);
            }
        }
    }
    marks
}

// Parses everything after a hunk header's own literal "@@ -" prefix --
// `<old-start>[,<old-count>] +<new-start>[,<new-count>] @@`, optionally
// followed by a trailing function-context hint some hunks carry (e.g.
// "@@ ... @@ fn foo()") -- `split_once(" @@")` already stops before that,
// so it's simply discarded rather than parsed. A count is implicitly 1
// when its own ",<count>" part is omitted -- unified diff's own
// convention for a single-line range. Returns `(old_count, new_start,
// new_count)`: `old_start` itself is never needed by any caller here,
// only the two counts (to classify the hunk) and `new_start` (to know
// which buffer lines it actually touches).
fn parse_hunk_rest(rest: &str) -> Option<(usize, usize, usize)> {
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let (_, old_count) = parse_range(old_part)?;
    let (new_start, new_count) = parse_range(new_part)?;
    Some((old_count, new_start, new_count))
}

fn parse_range(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

// The glibc/BSD `struct tm` layout, same as exec.rs's own CTm (kept as a
// separate, local copy rather than shared -- this file's own small raw
// libc FFI need doesn't justify a cross-module dependency for it, matching
// how e.g. exec.rs's tty_basename/stdin_is_tty already each keep their own
// tiny FFI declarations rather than sharing one). Unlike exec.rs's
// local_time_now (always "right now"), this converts an arbitrary
// caller-given Unix timestamp -- git blame's own author-time -- so it
// calls localtime_r directly with that value instead of calling time()
// first.
#[repr(C)]
struct CTm {
    tm_sec: i32,
    tm_min: i32,
    tm_hour: i32,
    tm_mday: i32,
    tm_mon: i32,
    tm_year: i32,
    tm_wday: i32,
    tm_yday: i32,
    tm_isdst: i32,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

fn format_unix_date(epoch_secs: i64) -> String {
    unsafe extern "C" {
        fn localtime_r(t: *const i64, result: *mut CTm) -> *mut CTm;
    }
    let mut tm = CTm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe { localtime_r(&epoch_secs as *const i64, &mut tm as *mut CTm) };
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every one of these hunk shapes was checked against a real `git
    // diff --no-color -U0` run first (see this session's own git-history
    // for the exact repro commands) -- parse_unified_diff's own doc
    // comment on `diff` explains why `new_start.saturating_sub(1)` is the
    // right attachment line for a pure deletion in each of the three
    // positions (start/middle/end of file) tested here.
    #[test]
    fn parse_unified_diff_marks_a_pure_addition() {
        let text = "@@ -2,0 +3,2 @@ two\n+NEWA\n+NEWB\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&2), Some(&DiffMark::Added));
        assert_eq!(marks.get(&3), Some(&DiffMark::Added));
        assert_eq!(marks.len(), 2);
    }

    #[test]
    fn parse_unified_diff_marks_a_changed_line() {
        let text = "@@ -3 +3 @@ two\n-three\n+CHANGED\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&2), Some(&DiffMark::Changed));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn parse_unified_diff_marks_a_pure_deletion_at_the_start_on_line_zero() {
        let text = "@@ -1 +0,0 @@\n-one\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&0), Some(&DiffMark::Removed));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn parse_unified_diff_marks_a_pure_deletion_in_the_middle_on_the_preceding_line() {
        let text = "@@ -3 +2,0 @@ two\n-three\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&1), Some(&DiffMark::Removed));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn parse_unified_diff_marks_a_pure_deletion_at_the_end_on_the_last_line() {
        let text = "@@ -5 +4,0 @@ four\n-five\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&3), Some(&DiffMark::Removed));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn parse_unified_diff_ignores_non_hunk_lines_and_handles_multiple_hunks() {
        let text = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+b\n@@ -5,0 +6,1 @@\n+c\n";
        let marks = parse_unified_diff(text);
        assert_eq!(marks.get(&0), Some(&DiffMark::Changed));
        assert_eq!(marks.get(&5), Some(&DiffMark::Added));
        assert_eq!(marks.len(), 2);
    }

    #[test]
    fn parse_unified_diff_of_empty_text_is_empty() {
        assert!(parse_unified_diff("").is_empty());
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
