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
    // A recognized flag's own description text, keyed by exactly the
    // spelling `flags` itself uses (`extract_flag_token`'s own output --
    // no `=value`/bundled-short-flag normalization beyond what that
    // already does) -- `K`-hover's own "which flag, and what does it
    // do" lookup (docs.rs), once it's already found which command a
    // flag under the cursor belongs to. A flag with no entry here either
    // had no description this parser could isolate, or (for a bundled
    // short flag like the `-la` in `ls -la`) was never a `flags` entry
    // on its own to begin with.
    pub flag_descriptions: HashMap<String, String>,
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

// Finds and parses the page itself -- no `man` subprocess, no groff.
// This used to spawn `man` and scan its *rendered* output, which cost
// roughly 250ms per lookup and meant recovering structure by measuring
// indentation. Now the page's own source is read (gunzipped with
// crate::archive if it needs it) and run through crate::roff, so a
// flag and its description are paired because the page's own `.TP`
// paired them, not because they happened to line up in a column.
fn fetch_and_parse(command: &str) -> Option<ManPageData> {
    let source = read_page(&find_page(command)?)?;
    Some(extract(&crate::roff::parse(&source), command))
}

// Where man pages live: `$MANPATH` when it is set, otherwise the
// conventional roots. No `manpath` subprocess and no config file parsing
// -- the point of this change is to stop shelling out.
fn man_roots() -> Vec<std::path::PathBuf> {
    if let Ok(path) = std::env::var("MANPATH")
        && !path.trim().is_empty()
    {
        return path.split(':').filter(|p| !p.is_empty()).map(std::path::PathBuf::from).collect();
    }
    ["/usr/share/man", "/usr/local/share/man", "/usr/local/man", "/usr/man"]
        .iter()
        .map(std::path::PathBuf::from)
        .collect()
}

// Sections in the order a user means them: a bare name is a command
// first, an admin command next, and only then the library and file
// formats -- the same precedence `man` itself uses.
const SECTION_ORDER: [&str; 8] = ["1", "8", "6", "5", "7", "3", "2", "4"];

fn find_page(command: &str) -> Option<std::path::PathBuf> {
    // A command with a slash or a leading dash is not a page name, and
    // must never be turned into one.
    if command.is_empty() || command.contains('/') || command.starts_with('-') || command.contains("..") {
        return None;
    }
    let roots = man_roots();
    for section in SECTION_ORDER {
        for root in &roots {
            let dir = root.join(format!("man{section}"));
            for name in [format!("{command}.{section}"), format!("{command}.{section}.gz")] {
                let path = dir.join(&name);
                if path.is_file() {
                    return Some(path);
                }
            }
            // Pages with a suffixed section (`perl.1p`, `foo.3ssl`) need
            // a scan, which only runs when the direct paths missed.
            if let Ok(entries) = std::fs::read_dir(&dir) {
                let prefix = format!("{command}.{section}");
                for entry in entries.flatten() {
                    let file = entry.file_name();
                    let file = file.to_string_lossy();
                    if file.starts_with(&prefix) {
                        return Some(entry.path());
                    }
                }
            }
        }
    }
    None
}

// The page's text, gunzipped when it needs it, following the one-line
// `.so` redirect a page like `bunzip2.1` is (its whole content is
// `.so man1/bzip2.1`). Followed once: a redirect to a redirect is not a
// thing real pages do, and refusing to loop matters more than serving
// one that did.
fn read_page(path: &std::path::Path) -> Option<String> {
    let text = read_text(path)?;
    let redirect = text.lines().find(|l| !l.trim().is_empty() && !l.starts_with(".\\\""))?;
    let Some(target) = redirect.trim().strip_prefix(".so ") else { return Some(text) };
    let target = target.trim();
    if target.is_empty() || target.contains("..") || target.starts_with('/') {
        return Some(text);
    }
    for root in man_roots() {
        let candidate = root.join(target);
        for path in [candidate.clone(), candidate.with_extension("gz")] {
            if path.is_file()
                && let Some(followed) = read_text(&path)
            {
                return Some(followed);
            }
        }
    }
    Some(text)
}

fn read_text(path: &std::path::Path) -> Option<String> {
    if path.extension().is_some_and(|e| e == "gz") {
        let (_, bytes) = crate::archive::gunzip(path).ok()?;
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    std::fs::read_to_string(path).ok()
}

// Everything this module wants out of a parsed page. Public so the
// tests can drive it from roff source directly, which is what these
// pages actually are.
pub(crate) fn extract(doc: &crate::roff::Document, command: &str) -> ManPageData {
    let mut tags = Vec::new();
    collect_tagged(&doc.blocks, &mut tags);

    let mut flags = Vec::new();
    let mut flag_descriptions = HashMap::new();
    for (tag, body) in &tags {
        // `-a, --all` and `-b [WHEN]` alike: split the tag into pieces
        // and keep the ones that are really flags.
        for piece in tag.split([',', '|']) {
            let Some(token) = extract_flag_token(piece.trim()) else { continue };
            if !flags.contains(&token) {
                flags.push(token.clone());
            }
            if !body.trim().is_empty() {
                flag_descriptions.entry(token).or_insert_with(|| body.trim().to_string());
            }
        }
    }
    flags.sort();
    flags.dedup();

    let subcommands = collect_subcommands(doc, &tags, command);
    let name_section = name_line(doc);
    ManPageData { flags, subcommands, name_section, flag_descriptions }
}

// Every tagged paragraph in the page, at any depth, as (tag, body text).
fn collect_tagged(blocks: &[crate::roff::Block], out: &mut Vec<(String, String)>) {
    use crate::roff::Block;
    for block in blocks {
        match block {
            Block::Tagged { tag, blocks, .. } => {
                let mut body = String::new();
                collect_text(blocks, &mut body);
                out.push((crate::roff::text_of(tag), body));
                collect_tagged(blocks, out);
            }
            Block::Indented { blocks, .. } => collect_tagged(blocks, out),
            _ => {}
        }
    }
}

fn collect_text(blocks: &[crate::roff::Block], out: &mut String) {
    use crate::roff::Block;
    for block in blocks {
        match block {
            Block::Paragraph { content, .. } | Block::Heading { content, .. } => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&crate::roff::text_of(content));
            }
            Block::Literal { lines, .. } => {
                for line in lines {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(line.trim());
                }
            }
            Block::Tagged { tag, blocks, .. } => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&crate::roff::text_of(tag));
                collect_text(blocks, out);
            }
            Block::Indented { blocks, .. } => collect_text(blocks, out),
        }
    }
}

// Two conventions, because tools split into two camps. git/apt/ip give
// each subcommand its own page and cross-reference them as
// `git-add(1)`; docker/systemctl-style tools list them as tagged
// paragraphs in a COMMANDS section.
fn collect_subcommands(doc: &crate::roff::Document, tags: &[(String, String)], command: &str) -> Vec<String> {
    let mut subs = Vec::new();
    let prefix = format!("{command}-");
    let mut push = |name: &str| {
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            subs.push(name.to_string());
        }
    };
    for (tag, _) in tags {
        let tag = tag.trim();
        if let Some(rest) = tag.strip_prefix(&prefix)
            && let Some(open) = rest.find('(')
            && rest[open + 1..].starts_with(|c: char| c.is_ascii_digit())
        {
            push(&rest[..open]);
            continue;
        }
        // A COMMANDS-section tag is the subcommand itself.
        if !tag.starts_with('-') && !tag.contains(' ') {
            push(tag);
        }
    }
    for section in ["COMMANDS", "SUBCOMMANDS", "GIT COMMANDS"] {
        for block in doc.section(section) {
            if let crate::roff::Block::Tagged { tag, .. } = block {
                let tag = crate::roff::text_of(tag);
                let tag = tag.trim();
                if let Some(rest) = tag.strip_prefix(&prefix) {
                    push(rest.split('(').next().unwrap_or(rest));
                } else if !tag.starts_with('-') {
                    push(tag.split_whitespace().next().unwrap_or(tag));
                }
            }
        }
    }
    subs.sort();
    subs.dedup();
    subs.retain(|s| s != command);
    subs
}

// The NAME section's own one line -- `ls - list directory contents`.
fn name_line(doc: &crate::roff::Document) -> Option<String> {
    for block in doc.section("NAME") {
        if let crate::roff::Block::Paragraph { content, .. } = block {
            let text = crate::roff::text_of(content);
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    None
}

// One flag out of a piece of a tag, or `None` when the piece is prose
// that merely starts with a dash.
pub(crate) fn extract_flag_token(piece: &str) -> Option<String> {
    if !piece.starts_with('-') {
        return None;
    }
    let end = piece.find(['=', ' ', '[']).unwrap_or(piece.len());
    let token = &piece[..end];
    if token.len() < 2 || !token.chars().nth(1).is_some_and(|c| c == '-' || c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Written as roff, because that is what a man page is. The old
    // tests here were excerpts of `man`'s *rendered* output, which is
    // what this module used to parse; keeping them would test a
    // pipeline that no longer exists.
    const LS_STYLE: &str = r#".TH LS 1 "March 2024" "GNU coreutils" "User Commands"
.SH NAME
ls \- list directory contents
.SH DESCRIPTION
.TP
\fB\-a\fR, \fB\-\-all\fR
do not ignore entries starting with .
.TP
\fB\-\-block\-size\fR=\fISIZE\fR
with \fB\-l\fR, scale sizes by SIZE when printing them
.TP
\fB\-c\fR
with \fB\-lt\fR: sort by, and show, ctime
"#;

    #[test]
    fn flags_come_from_the_pages_own_tagged_paragraphs() {
        let data = extract(&crate::roff::parse(LS_STYLE), "ls");
        assert_eq!(data.flags, vec!["--all", "--block-size", "-a", "-c"]);
    }

    #[test]
    fn a_flags_description_is_the_body_its_tag_paired_it_with() {
        let data = extract(&crate::roff::parse(LS_STYLE), "ls");
        assert_eq!(
            data.flag_descriptions.get("-a").map(String::as_str),
            Some("do not ignore entries starting with .")
        );
        // Both spellings on one tag get the same description, because
        // the page gave them one description.
        assert_eq!(data.flag_descriptions.get("--all"), data.flag_descriptions.get("-a"));
        assert_eq!(
            data.flag_descriptions.get("-c").map(String::as_str),
            Some("with \u{2d}lt: sort by, and show, ctime")
        );
    }

    #[test]
    fn the_name_section_is_the_summary_line() {
        let data = extract(&crate::roff::parse(LS_STYLE), "ls");
        assert_eq!(data.name_section.as_deref(), Some("ls - list directory contents"));
    }

    // A dash that opens prose is not an option -- but `--`, which real
    // pages document as the end-of-options marker, is one.
    #[test]
    fn prose_that_merely_starts_with_a_dash_is_not_a_flag() {
        let page = ".SH DESCRIPTION\n.TP\n\\- a bullet point, not an option\ntext\n.TP\n\\-\\-\nend of options marker\n";
        let data = extract(&crate::roff::parse(page), "tool");
        assert_eq!(data.flags, vec!["--"], "the bullet is not a flag; `--` is");
    }

    // git/apt/ip give each subcommand its own page and cross-reference
    // it by name.
    #[test]
    fn subcommands_are_read_from_cross_referenced_page_names() {
        let page = ".TH GIT 1\n.SH GIT COMMANDS\n.TP\ngit-add(1)\nAdd file contents to the index\n.TP\ngit-commit(1)\nRecord changes\n";
        let data = extract(&crate::roff::parse(page), "git");
        assert_eq!(data.subcommands, vec!["add", "commit"]);
    }

    #[test]
    fn a_different_commands_cross_references_are_not_borrowed() {
        let page = ".TH GIT 1\n.SH SEE ALSO\n.TP\nsvn-add(1)\nsomething else entirely\n";
        let data = extract(&crate::roff::parse(page), "git");
        assert!(data.subcommands.is_empty(), "{:?}", data.subcommands);
    }

    #[test]
    fn a_commands_section_lists_its_own_tags_as_subcommands() {
        let page = ".TH TOOL 1\n.SH COMMANDS\n.TP\nbuild\nBuild the project\n.TP\ntest\nRun the tests\n";
        let data = extract(&crate::roff::parse(page), "tool");
        assert_eq!(data.subcommands, vec!["build", "test"]);
    }

    #[test]
    fn extract_flag_token_stops_at_a_value_or_a_bracket() {
        assert_eq!(extract_flag_token("--block-size=SIZE").as_deref(), Some("--block-size"));
        assert_eq!(extract_flag_token("--color[=WHEN]").as_deref(), Some("--color"));
        assert_eq!(extract_flag_token("-w COLS").as_deref(), Some("-w"));
        assert_eq!(extract_flag_token("not a flag"), None);
        assert_eq!(extract_flag_token("-"), None);
    }

    // The end-to-end path, against whatever this machine actually has.
    // Skipped where there are no man pages at all.
    #[test]
    fn reads_a_real_page_from_this_machine_with_no_subprocess() {
        let Some(path) = find_page("ls") else { return };
        assert!(path.exists(), "{}", path.display());
        let source = read_page(&path).expect("a readable page");
        let data = extract(&crate::roff::parse(&source), "ls");
        assert!(
            data.name_section.as_deref().is_some_and(|n| n.contains("ls")),
            "NAME was {:?}",
            data.name_section
        );
        assert!(data.flags.iter().any(|f| f == "-l"), "expected -l among {:?}", data.flags);
        assert!(
            data.flag_descriptions.contains_key("-l"),
            "expected a description for -l"
        );
    }

    #[test]
    fn a_page_name_that_is_a_path_is_never_looked_up() {
        assert!(find_page("../../etc/passwd").is_none());
        assert!(find_page("/etc/passwd").is_none());
        assert!(find_page("").is_none());
        assert!(find_page("-rf").is_none());
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
        for _ in 0..50 {
            if matches!(query(bogus), ManStatus::Missing) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("expected {bogus} to resolve to Missing within the poll window");
    }
}
