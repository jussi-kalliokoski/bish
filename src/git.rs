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
        let msg = String::from_utf8_lossy(&output.stderr);
        let msg = msg.lines().next().unwrap_or("git blame failed").trim();
        return Err(msg.to_string());
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
