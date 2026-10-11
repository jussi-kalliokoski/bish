// Man-page mining: a command's flags (with their descriptions), its
// subcommands and its NAME line, from the page's roff source via
// `crate::roff`. Used by highlighting, completion and `K` hover.
//
// Highlighting runs on every keystroke, so `query` never blocks: it
// returns cached data or `Pending` and parses the page on a background
// thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub struct ManPageData {
    pub flags: Vec<String>,
    pub subcommands: Vec<String>,
    // The NAME section's one line (`ls - list directory contents`), as
    // `whatis` shows it; `K` hover's fallback. `None` when the page has no
    // NAME section.
    pub name_section: Option<String>,
    // Each flag's description, keyed by the spelling `flags` uses (from
    // `extract_flag_token`), for `K` hover on a flag. A flag with no entry
    // had no description to isolate, or was never a flag on its own (the
    // `-la` in `ls -la`).
    pub flag_descriptions: HashMap<String, String>,
}

pub enum ManStatus {
    // No page, or nothing useful in it. Cached for the life of the process.
    Missing,
    // A fetch is in flight. Skip this command for now; the next redraw
    // queries again.
    Pending,
    Ready(Arc<ManPageData>),
}

enum CacheEntry {
    Pending,
    Ready(Arc<ManPageData>),
    Missing,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

// Never blocks. One lock acquisition both checks for an entry and, if
// there's none, inserts Pending and spawns the fetch, so two queries for
// the same command can't both spawn.
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

/// `query`, parsing on this thread so the answer is never `Pending`. For
/// a caller with nothing else to do until it arrives, like
/// `bish tool lsp-server` answering a hover. Never from the editor, whose
/// redraw would stutter.
///
/// Shares `query`'s cache, and waits for a fetch already in flight rather
/// than duplicating it.
pub fn query_blocking(command: &str) -> ManStatus {
    loop {
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = cache.lock().unwrap();
        match guard.get(command) {
            Some(CacheEntry::Ready(data)) => return ManStatus::Ready(Arc::clone(data)),
            Some(CacheEntry::Missing) => return ManStatus::Missing,
            // Another thread's fetch. Released first, since that fetch
            // needs the lock to finish.
            Some(CacheEntry::Pending) => {
                drop(guard);
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            None => {
                guard.insert(command.to_string(), CacheEntry::Pending);
            }
        }
        drop(guard);
        fetch_and_store(command.to_string());
    }
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

/// A command's man page as raw roff, for rendering rather than mining.
/// Synchronous: it answers a keypress and costs a file read and a
/// decompress, not the parse `query` uses a thread for. `None` when there's
/// no page.
pub fn source_for(command: &str) -> Option<String> {
    read_page(&find_page(command)?)
}

fn fetch_and_parse(command: &str) -> Option<ManPageData> {
    let source = read_page(&find_page(command)?)?;
    Some(extract(&crate::roff::parse(&source), command))
}

// Where man pages live: `$MANPATH` if set, otherwise the conventional
// roots. No `manpath` subprocess, no config parsing.
fn man_roots() -> Vec<std::path::PathBuf> {
    if let Ok(path) = std::env::var("MANPATH")
        && !path.trim().is_empty()
    {
        return path.split(':').filter(|p| !p.is_empty()).map(std::path::PathBuf::from).collect();
    }
    ["/usr/share/man", "/usr/local/share/man", "/usr/local/man", "/usr/man"].iter().map(std::path::PathBuf::from).collect()
}

// A bare name is a command first, then an admin command, then the rest,
// as `man` orders sections.
const SECTION_ORDER: [&str; 8] = ["1", "8", "6", "5", "7", "3", "2", "4"];

fn find_page(command: &str) -> Option<std::path::PathBuf> {
    // A name with a slash, a leading dash or `..` is never a page name.
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
            // Suffixed sections (`perl.1p`, `foo.3ssl`) need a scan, done
            // only when the direct paths missed.
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

// The page's text, gunzipped if needed, following a one-line `.so`
// redirect such as `bunzip2.1` (`.so man1/bzip2.1`). Followed once:
// refusing to loop matters more than a redirect to a redirect.
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

// Far above any real page, and tighter than `gunzip`'s default: pages
// are read in the background as names appear in the line, so a crafted
// `.gz` on MANPATH mustn't choose its own memory cost.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

fn read_text(path: &std::path::Path) -> Option<String> {
    if path.extension().is_some_and(|e| e == "gz") {
        let (_, bytes) = crate::archive::gunzip_within(path, MAX_PAGE_BYTES).ok()?;
        return Some(crate::encoding::decode(&bytes).text);
    }
    // Plenty of pages are still latin-1.
    Some(crate::encoding::decode(&std::fs::read(path).ok()?).text)
}

// Everything this module wants from a parsed page. Visible to the tests,
// which drive it from roff source.
pub(crate) fn extract(doc: &crate::roff::Document, command: &str) -> ManPageData {
    let mut tags = Vec::new();
    collect_tagged(&doc.blocks, &mut tags);

    let mut flags = Vec::new();
    let mut flag_descriptions = HashMap::new();
    for (tag, body) in &tags {
        // `-a, --all` and `-b [WHEN]`: split the tag and keep the pieces that
        // are flags.
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
    crate::sort::sort(&mut flags);
    flags.dedup();

    let subcommands = collect_subcommands(doc, &tags, command);
    let name_section = name_line(doc);
    ManPageData { flags, subcommands, name_section, flag_descriptions }
}

// Every tagged paragraph, at any depth, as (tag, body text).
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

// Two conventions: git, apt and ip give each subcommand its own page and
// cross-reference it as `git-add(1)`; docker and systemctl list them as
// tagged paragraphs in a COMMANDS section.
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
    crate::sort::sort(&mut subs);
    subs.dedup();
    subs.retain(|s| s != command);
    subs
}

// The NAME section's line: `ls - list directory contents`.
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

// A flag from one piece of a tag, or `None` for prose that starts with a
// dash.
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
        assert_eq!(data.flag_descriptions.get("-a").map(String::as_str), Some("do not ignore entries starting with ."));
        // Both spellings on one tag share its description.
        assert_eq!(data.flag_descriptions.get("--all"), data.flag_descriptions.get("-a"));
        assert_eq!(data.flag_descriptions.get("-c").map(String::as_str), Some("with \u{2d}lt: sort by, and show, ctime"));
    }

    #[test]
    fn the_name_section_is_the_summary_line() {
        let data = extract(&crate::roff::parse(LS_STYLE), "ls");
        assert_eq!(data.name_section.as_deref(), Some("ls - list directory contents"));
    }

    // A dash opening prose isn't an option, but `--`, the documented
    // end-of-options marker, is.
    #[test]
    fn prose_that_merely_starts_with_a_dash_is_not_a_flag() {
        let page = ".SH DESCRIPTION\n.TP\n\\- a bullet point, not an option\ntext\n.TP\n\\-\\-\nend of options marker\n";
        let data = extract(&crate::roff::parse(page), "tool");
        assert_eq!(data.flags, vec!["--"], "the bullet is not a flag; `--` is");
    }

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

    // End to end, against this machine's pages; skipped if there are none.
    #[test]
    fn reads_a_real_page_from_this_machine_with_no_subprocess() {
        let Some(path) = find_page("ls") else { return };
        assert!(path.exists(), "{}", path.display());
        let source = read_page(&path).expect("a readable page");
        let data = extract(&crate::roff::parse(&source), "ls");
        assert!(data.name_section.as_deref().is_some_and(|n| n.contains("ls")), "NAME was {:?}", data.name_section);
        assert!(data.flags.iter().any(|f| f == "-l"), "expected -l among {:?}", data.flags);
        assert!(data.flag_descriptions.contains_key("-l"), "expected a description for -l");
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
        // A command that can't exist: Pending, then Missing, with no panic
        // and no blocking.
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
