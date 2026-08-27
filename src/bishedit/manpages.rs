// Hand-rolled man-page mining for context-aware highlighting -- fish has a
// similar built-in man-page parser it uses to drive completions; this is
// the same idea, scoped (for now) to extracting a command's own flags and
// its immediate subcommands, used by highlight.rs to bold-highlight
// recognized unquoted arguments. No regex crate, no `col` dependency --
// pure hand-written line scanning, matching this codebase's existing
// hand-rolled lexer/parser style.
//
// Fetching a man page is genuinely slow (~250-280ms measured for `man
// git`/`man ls` in this environment -- spawning `man`, groff rendering,
// etc. isn't free), and highlight.rs's caller (editor.rs::redraw) runs on
// every keystroke, so this can never block: `query()` always returns
// immediately, either with cached data or a `Pending` marker, and kicks off
// a background thread (this module's own first-ever use of
// std::thread/Mutex/Arc in this codebase -- see `query`'s own doc comment
// for why the single-lock-acquisition design avoids a duplicate-spawn
// race).
//
// Not yet wired into highlight.rs -- lands ahead of its consumer, same
// "build the seam, wire it in later" pattern as several other modules in
// this crate (vt100.rs, pty.rs before the M9 compositor; highlight.rs
// itself before editor.rs::redraw wired it in).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub struct ManPageData {
    pub flags: Vec<String>,
    pub subcommands: Vec<String>,
    // The one-line summary from the page's own "NAME" section (e.g. "ls
    // - list directory contents"), the same text `whatis`/`apropos`
    // show -- debugger.rs's own `K` hover uses this as its "show a
    // snippet from the manpage" fallback for an identifier that isn't a
    // known variable/function. `None` when the page has no such section
    // (rare, but not every page follows the convention) rather than
    // guessing at some other line.
    pub name_section: Option<String>,
}

pub enum ManStatus {
    // No man page found for this command (or it failed to parse into
    // anything useful) -- cached permanently, never retried within this
    // process's lifetime (simplest possible negative-caching policy).
    Missing,
    // A background fetch is in flight; nothing to show yet. The caller
    // should just skip flag/subcommand highlighting for this command on
    // this redraw -- the very next redraw (next keystroke) will re-query
    // and pick up whatever landed by then.
    Pending,
    Ready(Arc<ManPageData>),
}

enum CacheEntry {
    Pending,
    Ready(Arc<ManPageData>),
    Missing,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

// The one entry point highlight.rs calls. Never blocks: does a *single*
// lock acquisition that both checks for an existing entry and, only if
// none exists, inserts Pending and spawns the fetch thread -- all before
// releasing the lock. This is what prevents a duplicate-spawn race: a
// second query() call for the same in-flight command (e.g. the next
// keystroke's redraw) takes the same lock, sees the Pending entry already
// there, and returns without ever reaching thread::spawn. There's no
// separate "check" then "insert" as two lock acquisitions, so there's no
// window for two threads to both decide "nothing here yet, I'll spawn."
pub fn query(command: &str) -> ManStatus {
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    match guard.get(command) {
        Some(CacheEntry::Ready(data)) => return ManStatus::Ready(Arc::clone(data)),
        Some(CacheEntry::Pending) => return ManStatus::Pending,
        Some(CacheEntry::Missing) => return ManStatus::Missing,
        None => {}
    }
    guard.insert(command.to_string(), CacheEntry::Pending);
    drop(guard); // release before spawning -- fetch_and_store re-locks itself
    let owned = command.to_string();
    std::thread::spawn(move || fetch_and_store(owned));
    ManStatus::Pending
}

fn fetch_and_store(command: String) {
    let result = fetch_and_parse(&command);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock().unwrap();
    guard.insert(
        command,
        match result {
            Some(data) => CacheEntry::Ready(Arc::new(data)),
            None => CacheEntry::Missing,
        },
    );
}

// Spawns `man <command>`, checking the real exit status (not just
// "stdout was empty") since a missing man page prints its complaint to
// stderr and produces nothing on stdout either way -- status is the only
// reliable signal. MANWIDTH is set wide so flag/description lines don't
// wrap mid-line, which would otherwise confuse the line-based parsers
// below (a wrapped continuation line looks identical to an unrelated
// prose line with no way to tell them apart here).
fn fetch_and_parse(command: &str) -> Option<ManPageData> {
    let output = std::process::Command::new("man").env("MANWIDTH", "1000").arg(command).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = strip_overstrike(&text);
    let flags = parse_flags(&text);
    let subcommands = parse_subcommands(&text, command);
    let name_section = parse_name_section(&text);
    Some(ManPageData { flags, subcommands, name_section })
}

// The first non-blank line after a bare "NAME" section header, trimmed --
// real man pages format this section as one line (e.g. "ls - list
// directory contents"). A page with no such section (or an unusually
// multi-line one, not attempted here) yields `None` rather than a guess.
fn parse_name_section(text: &str) -> Option<String> {
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "NAME" {
            continue;
        }
        for candidate in lines.by_ref() {
            let trimmed = candidate.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        return None;
    }
    None
}

// Defensive pass for `X\x08X`-style backspace-overstrike sequences some
// `man`/pager/groff configurations emit for bold/underline when not
// attached to a TTY (bold: the same char repeated around a backspace,
// e.g. "b\x08bo\x08o"; underline: an underscore before the backspace,
// e.g. "_\x08t"). This WSL environment's own `man` doesn't produce these
// (piped output measured clean), but that's environment-specific --
// stripping unconditionally is a no-op when there's nothing to strip, and
// correct when there is, without depending on the external `col` utility.
fn strip_overstrike(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if i + 2 < chars.len() && chars[i + 1] == '\u{8}' {
            // "X\x08Y" -- keep Y (the char actually meant to display;
            // for bold X==Y, for underline Y is the real char and X is
            // the '_' overstrike mark), drop the backspace and X.
            out.push(chars[i + 2]);
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

// Line-oriented flag scanner. Real man pages consistently indent flag
// entries at a shallow, fixed column (both the ls-style "-a, --all" and
// git-style "-C <path>" samples this was grounded against sit around
// column 7) with their description indented further or on a later line --
// so "starts with '-' after trimming, and the ORIGINAL line's indent is
// shallow" is a good proxy for "this is a flag line" without needing to
// track section context. Multiple spellings on one line ("-a, --all") are
// comma-separated.
fn parse_flags(text: &str) -> Vec<String> {
    let mut flags = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('-') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent > 8 {
            continue;
        }
        for piece in trimmed.split(',') {
            if let Some(flag) = extract_flag_token(piece.trim()) {
                flags.push(flag);
            }
        }
    }
    flags.sort();
    flags.dedup();
    flags
}

// Extracts just the flag spelling from a piece like "-a", "--author",
// "--block-size=SIZE", "-C <path>", or "-c     with -lt: sort by..." --
// stops at the first '='/' '/'[' (a value suffix, a placeholder, or an
// optional-value bracket) and validates what's left actually looks like a
// flag. Exact-string match only, per this feature's v1 scope -- no
// bundled short-flag decomposition (e.g. "-la" is never split into "-l"
// and "-a").
fn extract_flag_token(piece: &str) -> Option<String> {
    if !piece.starts_with('-') {
        return None;
    }
    let end = piece.find(|c: char| c == '=' || c == ' ' || c == '[').unwrap_or(piece.len());
    let token = &piece[..end];
    if token.len() < 2 || !token.chars().nth(1).is_some_and(|c| c == '-' || c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(token.to_string())
}

// Recognizes only "<command>-<subcommand>(<digit>)" lines (e.g.
// "git-add(1)") -- the naming convention git/apt/ip use for their own
// per-subcommand man pages, matched via a whole-trimmed-line check (not
// "appears somewhere in the line") so a prose sentence that merely
// *mentions* git-add(1) mid-paragraph doesn't get misread as a listing.
// The generic "COMMANDS section, bare lowercase words" fallback needed
// for docker/kubectl/systemctl-style tools (no per-subcommand pages) is
// deliberately not attempted -- out of scope for v1, see the plan.
fn parse_subcommands(text: &str, command: &str) -> Vec<String> {
    let prefix = format!("{}-", command);
    let mut subs = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(prefix.as_str()) else { continue };
        let Some(paren) = rest.find('(') else { continue };
        let (sub, tail) = rest.split_at(paren);
        if sub.is_empty() || sub.contains(char::is_whitespace) {
            continue;
        }
        let inner = &tail[1..];
        let Some(close) = inner.find(')') else { continue };
        if inner[..close].is_empty() || !inner[..close].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if inner[close + 1..].trim().is_empty() {
            subs.push(sub.to_string());
        }
    }
    subs.sort();
    subs.dedup();
    subs
}

#[cfg(test)]
mod tests {
    use super::*;

    const LS_EXCERPT: &str = "\
       -a, --all
              do not ignore entries starting with .
       -A, --almost-all
              do not list implied . and ..
       --author
              with -l, print the author of each file
       -b, --escape
              print C-style escapes for nongraphic characters
       --block-size=SIZE
              with -l, scale sizes by SIZE when printing them; e.g., '--block-size=M'; see SIZE format below
       -B, --ignore-backups
              do not list implied entries ending with ~
       -c     with -lt: sort by, and show, ctime (time of last modification of file  status  information);  with  -l:
              show ctime and sort by name; otherwise: sort by ctime, newest first
       -C     list entries by columns
";

    #[test]
    fn parse_flags_handles_ls_style_entries() {
        let flags = parse_flags(LS_EXCERPT);
        // Sorted ASCII order: all "--" long flags before any "-X" short
        // flag (second char '-' < any letter), uppercase short flags
        // before lowercase.
        assert_eq!(
            flags,
            vec![
                "--all", "--almost-all", "--author", "--block-size", "--escape", "--ignore-backups", "-A", "-B", "-C", "-a", "-b", "-c",
            ]
        );
    }

    const GIT_FLAGS_EXCERPT: &str = "\
       -C <path>
           Run as if git was started in <path> instead of the current working directory.
       -c <name>=<value>
           Pass a configuration parameter to the command.
       -p, --paginate
           Pipe all output into less (or if set, $PAGER) if standard output is a terminal.
";

    #[test]
    fn parse_flags_handles_git_style_placeholder_entries() {
        let flags = parse_flags(GIT_FLAGS_EXCERPT);
        // Sorted ASCII order: "--paginate" (second char '-') before any
        // "-X" short flag, then uppercase before lowercase.
        assert_eq!(flags, vec!["--paginate".to_string(), "-C".to_string(), "-c".to_string(), "-p".to_string()]);
    }

    #[test]
    fn parse_flags_ignores_prose_that_merely_starts_with_a_dash() {
        // A deeply-indented description continuation line beginning with
        // "-1" (e.g. explaining a negative-number option value) must not
        // be picked up as its own flag entry -- indent alone (> 8 cols)
        // already excludes this in real man-page formatting.
        let text = "       --block-size=SIZE\n                     a value of -1 means unlimited\n";
        let flags = parse_flags(text);
        assert_eq!(flags, vec!["--block-size".to_string()]);
    }

    const GIT_SUBCOMMANDS_EXCERPT: &str = "\
   Main porcelain commands
       git-add(1)
           Add file contents to the index.

       git-am(1)
           Apply a series of patches from a mailbox.

       This behaves the same as git-add(1) in most cases.
";

    #[test]
    fn parse_subcommands_recognizes_cmd_dash_sub_paren_n_lines() {
        let subs = parse_subcommands(GIT_SUBCOMMANDS_EXCERPT, "git");
        assert_eq!(subs, vec!["add".to_string(), "am".to_string()]);
    }

    #[test]
    fn parse_subcommands_rejects_mid_sentence_cross_references() {
        // "This behaves the same as git-add(1) in most cases." must not
        // contribute a duplicate/spurious entry beyond what the real
        // listing lines already produced above -- covered by the exact
        // dedup'd result in the previous test; this test isolates just
        // the rejection with no real listing lines present at all.
        let text = "       This behaves the same as git-add(1) in most cases.\n";
        let subs = parse_subcommands(text, "git");
        assert_eq!(subs, Vec::<String>::new());
    }

    #[test]
    fn parse_subcommands_ignores_a_different_commands_prefix() {
        let subs = parse_subcommands(GIT_SUBCOMMANDS_EXCERPT, "apt");
        assert_eq!(subs, Vec::<String>::new());
    }

    #[test]
    fn strip_overstrike_collapses_bold_and_underline_sequences() {
        // Bold: char repeated around a backspace. Underline: '_' then
        // backspace then the real char.
        let bold = "b\u{8}bo\u{8}ol\u{8}ld\u{8}d";
        assert_eq!(strip_overstrike(bold), "bold");
        let underline = "_\u{8}t_\u{8}e_\u{8}x_\u{8}t";
        assert_eq!(strip_overstrike(underline), "text");
    }

    #[test]
    fn strip_overstrike_is_a_no_op_on_plain_text() {
        let plain = "NAME\n       ls - list directory contents\n";
        assert_eq!(strip_overstrike(plain), plain);
    }

    #[test]
    fn query_returns_pending_then_eventually_ready_or_missing() {
        // A command guaranteed not to exist -- confirms the full
        // Pending -> (background thread) -> Missing pipeline runs without
        // panicking and without blocking the calling thread.
        let bogus = "bish-test-definitely-not-a-real-command-xyz";
        match query(bogus) {
            ManStatus::Pending | ManStatus::Missing => {}
            ManStatus::Ready(_) => panic!("unexpectedly found a real man page for {bogus}"),
        }
        // Poll briefly for the background fetch to land -- generous
        // upper bound since this genuinely spawns a real `man` process.
        for _ in 0..50 {
            if matches!(query(bogus), ManStatus::Missing) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("expected {bogus} to resolve to Missing within the poll window");
    }
}
