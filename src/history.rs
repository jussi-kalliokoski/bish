// Command history: persisted to ~/.bish_history (plain text, one entry per
// file line -- a stored entry's own newlines/backslashes are escaped so a
// multi-line history entry, e.g. a whole recalled for-loop, round-trips
// through the file as a single line rather than fragmenting on reload).
// editor.rs is the only consumer of the search methods, driving fish-style
// up/down: prefix-filtered rather than plain chronological recall.
//
// The filename is parameterized (see History::load) so command mode can
// keep a second, independent instance (its own file, own entries) without
// colliding with the normal shell's history.
//
// A genuine persistent (immutable, structurally-shared) singly-linked
// list, not a shared Vec with a per-session floor index: each entry is a
// Node pointing at the entry before it, and a History value is just an
// Rc handle onto its own newest Node (`tail`). Forking a session (`window
// new`/split -- see History::fork) is an O(1) Rc clone of that pointer,
// no copying, and from that moment the parent and child diverge --
// record() only ever extends *this* History's own tail with a fresh
// Node, so a command typed in one session's pane never becomes visible
// to a sibling's, or to the parent's, own Up/Down browsing or `!`
// expansion. This was the explicit reason for choosing a linked list
// over a flat array in the first place: unlike a shared array + index
// (which was tried here first, and looked observationally equivalent
// for the single-branch case, but wasn't -- a lower-bound index into one
// growing array can't stop a *later* sibling's entry from becoming
// visible once appended), a persistent list's divergence is structural,
// not just a numeric floor. Every session still shares the same on-disk
// FILE (interleaved across every process, same as before) via each
// History's own `path` -- forking clones that too (a cheap PathBuf
// clone), so every fork still appends to the identical file, just with
// an independently-diverging in-memory view.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

// `cwd` lands ahead of its consumer -- the suggestions engine
// (bishedit::suggestion) wires it in as its own, later stage -- same
// "build the seam, wire it in later" pattern already used elsewhere in
// this codebase (manpages.rs, lexer.rs's SpannedItem).
#[allow(dead_code)]
struct Node {
    entry: String,
    // The directory this entry was recorded from -- None for anything
    // loaded from the history file (load() never has a cwd to attach;
    // persistence for this field is deliberately deferred, see
    // HistoryEntry's own doc comment on prev for why the same "we
    // genuinely don't know" gate matters for sequence tracking too).
    cwd: Option<PathBuf>,
    prev: Option<Rc<Node>>,
}

// A directory-aware, borrowed view of one entry -- see History::entries.
// Unused until the suggestions engine's own stage wires it in.
#[allow(dead_code)]
pub struct HistoryEntry<'a> {
    pub text: &'a str,
    pub cwd: Option<&'a Path>,
    // The entry run immediately before this one, and ONLY when both it
    // and this one were recorded live in this session's own chain -- see
    // entries()'s own doc comment for why disk-loaded entries never get
    // one, even though the underlying Node chain technically links them.
    pub prev: Option<&'a str>,
}

// Clone is a plain, cheap Rc-clone of `tail` (identical to fork() below)
// -- kept as a real derive, not just fork(), so a caller that needs a
// standalone snapshot without the "new session" framing (e.g. repl.rs
// taking one to pass into a call that also needs a live mutable borrow
// of the session it came from) can just call .clone().
#[derive(Clone)]
pub struct History {
    path: Option<PathBuf>,
    tail: Option<Rc<Node>>,
}

impl History {
    // `filename` is a bare filename under $HOME, e.g. ".bish_history" for
    // the normal shell history or ".bish_cmd_history" for command mode's.
    pub fn load(filename: &str) -> History {
        let path = history_path(filename);
        let mut tail: Option<Rc<Node>> = None;
        if let Some(p) = &path {
            if let Ok(content) = std::fs::read_to_string(p) {
                for line in content.lines() {
                    tail = Some(Rc::new(Node { entry: unescape(line), cwd: None, prev: tail.take() }));
                }
            }
        }
        History { path, tail }
    }

    // Cheap, no-copy fork for a newly created session (`window new`/
    // split_focused_pane): the child starts out able to see everything
    // the parent could, via a plain Rc clone of the parent's current
    // tail (O(1) regardless of how much history exists), but from this
    // point on each side's own record() only ever extends its *own*
    // tail -- see this struct's own doc comment for why that's the
    // point of using a persistent list here at all.
    pub fn fork(&self) -> History {
        self.clone()
    }

    // Records a submitted line (regardless of whether it later succeeds or
    // fails -- bash and fish both do this). Skipped if blank or identical
    // to the immediately preceding entry; this is a lighter touch than
    // fish's full re-dedup-and-move-to-front, but keeps the common
    // "pressed enter on the same command twice" case from cluttering
    // history, which is what this is mainly for.
    //
    // `cwd`: the directory this command is about to run in, for the
    // suggestions engine's directory/sequence heuristic (see
    // HistoryEntry) -- None for callers with no meaningful shell context
    // (command mode's own, entirely separate history instance, which
    // isn't used for shell-command suggestions at all).
    pub fn record(&mut self, entry: &str, cwd: Option<&Path>) {
        if entry.trim().is_empty() {
            return;
        }
        if self.tail.as_ref().map(|n| n.entry.as_str()) == Some(entry) {
            return;
        }
        self.tail = Some(Rc::new(Node { entry: entry.to_string(), cwd: cwd.map(Path::to_path_buf), prev: self.tail.take() }));
        if let Some(p) = &self.path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", escape(entry));
            }
        }
    }

    // Walks this History's own chain into a plain, oldest-first Vec --
    // the same shape the old flat-array design already assumed, so
    // every lookup below can stay simple index math instead of juggling
    // `.prev` pointers directly. Re-walked fresh on every call rather
    // than cached: history sizes here (hundreds to low thousands of
    // entries) make that unmeasurable, and it sidesteps any risk of a
    // stale cache after record() extends the chain.
    fn to_vec(&self) -> Vec<&str> {
        let mut v = Vec::new();
        let mut cur = &self.tail;
        while let Some(node) = cur {
            v.push(node.entry.as_str());
            cur = &node.prev;
        }
        v.reverse();
        v
    }

    // A directory-aware, oldest-first view for the suggestions engine
    // (bishedit::suggestion), which needs more than to_vec()'s bare
    // strings: which directory an entry ran in, and what immediately
    // preceded it. Re-walked fresh every call, same "no caching, cheap
    // enough" reasoning as to_vec() above. Unused until that engine's
    // own stage wires it in.
    #[allow(dead_code)]
    pub fn entries(&self) -> Vec<HistoryEntry<'_>> {
        let mut v = Vec::new();
        let mut cur = &self.tail;
        while let Some(node) = cur {
            // `prev` is Some only when *both* this entry and the one
            // before it were recorded live in this session's own chain
            // (cwd.is_some() on both sides) -- a disk-loaded node's own
            // `.prev` is just the previous *line in a file every bish
            // process appends to concurrently*, not "the command that
            // actually ran before this one," so it must not leak into
            // the sequence heuristic as if it were.
            let prev = match (&node.cwd, &node.prev) {
                (Some(_), Some(p)) if p.cwd.is_some() => Some(p.entry.as_str()),
                _ => None,
            };
            v.push(HistoryEntry { text: node.entry.as_str(), cwd: node.cwd.as_deref(), prev });
            cur = &node.prev;
        }
        v.reverse();
        v
    }

    // Nearest entry at or before index `from` (exclusive; None means
    // "start from the newest") whose text starts with `prefix`,
    // searching toward older entries. Used for Up.
    pub fn search_backward(&self, prefix: &str, from: Option<usize>) -> Option<(usize, String)> {
        let v = self.to_vec();
        let end = from.unwrap_or(v.len());
        v[..end].iter().enumerate().rev().find(|(_, e)| e.starts_with(prefix)).map(|(i, e)| (i, e.to_string()))
    }

    // Nearest entry after index `from` whose text starts with `prefix`,
    // searching toward newer entries. Used for Down.
    pub fn search_forward(&self, prefix: &str, from: usize) -> Option<(usize, String)> {
        let v = self.to_vec();
        v.iter().enumerate().skip(from + 1).find(|(_, e)| e.starts_with(prefix)).map(|(i, e)| (i, e.to_string()))
    }

    // `!n`: absolute, 1-based (matches how bash numbers history events).
    fn entry_by_number(&self, n: usize) -> Option<String> {
        let idx = n.checked_sub(1)?;
        self.to_vec().get(idx).map(|s| s.to_string())
    }

    // `!-n`: n events back from the most recent (1 = most recent, same
    // as `!!`).
    fn entry_back(&self, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        let v = self.to_vec();
        let idx = v.len().checked_sub(n)?;
        v.get(idx).map(|s| s.to_string())
    }

    // `!/prefix` (bish's own spelling of bash's plain `!prefix`, freeing
    // up a bare `!word` to mean "run in a child shell" instead -- see
    // expand's own doc comment): most recent entry starting with it.
    fn find_starting_with(&self, prefix: &str) -> Option<String> {
        self.to_vec().iter().rev().find(|e| e.starts_with(prefix)).map(|s| s.to_string())
    }

    // `!?text`: most recent entry containing it anywhere.
    fn find_containing(&self, substr: &str) -> Option<String> {
        self.to_vec().iter().rev().find(|e| e.contains(substr)).map(|s| s.to_string())
    }
}

// Recognized `!`-designator, and how much of the text right after the
// `!` it consumes -- see find_designator.
enum Designator<'a> {
    Bang,
    LastArg,
    Back(usize),
    Number(usize),
    Contains(&'a str),
    StartsWith(&'a str),
}

// Tries to parse a `!`-designator starting right after the `!` itself
// (`rest` is everything from there on). Returns the designator and how
// many bytes of `rest` it consumes, or None if `rest` doesn't start with
// any recognized form -- the `!` is then left alone (general scan) or,
// if it was the line's own leading character, triggers the child-shell
// fallback (see expand).
fn find_designator(rest: &str) -> Option<(usize, Designator<'_>)> {
    let mut chars = rest.chars();
    match chars.next()? {
        '!' => Some((1, Designator::Bang)),
        '$' => Some((1, Designator::LastArg)),
        '-' => {
            let digits: String = rest[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                None
            } else {
                Some((1 + digits.len(), Designator::Back(digits.parse().ok()?)))
            }
        }
        c if c.is_ascii_digit() => {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            Some((digits.len(), Designator::Number(digits.parse().ok()?)))
        }
        '?' => {
            let body = &rest[1..];
            // `!?text?` (closed) or `!?text` up to the next blank/end.
            if let Some(end) = body.find('?') {
                Some((1 + end + 1, Designator::Contains(&body[..end])))
            } else {
                let end = body.find(char::is_whitespace).unwrap_or(body.len());
                if end == 0 {
                    None
                } else {
                    Some((1 + end, Designator::Contains(&body[..end])))
                }
            }
        }
        '/' => {
            let body = &rest[1..];
            let end = body.find(char::is_whitespace).unwrap_or(body.len());
            if end == 0 {
                None
            } else {
                Some((1 + end, Designator::StartsWith(&body[..end])))
            }
        }
        _ => None,
    }
}

fn resolve(kind: &Designator, history: &History) -> Option<String> {
    match kind {
        Designator::Bang => history.entry_back(1),
        Designator::LastArg => history.entry_back(1).and_then(|s| s.split_whitespace().next_back().map(str::to_string)),
        Designator::Back(n) => history.entry_back(*n),
        Designator::Number(n) => history.entry_by_number(*n),
        Designator::Contains(s) => history.find_containing(s),
        Designator::StartsWith(s) => history.find_starting_with(s),
    }
}

// A freshly-typed line, after history expansion. Substituted is the
// common case -- feed it into the lexer/parser exactly like the
// original line. UnrecognizedBang is the "otherwise" case from expand's
// own doc comment: the line started with a `!` that wasn't any
// recognized designator, so the rest of it is meant to run in a child
// shell instead -- callers decide exactly how (see expand's doc comment
// for why that differs between the normal prompt and command mode).
pub enum Expansion {
    Substituted(String),
    UnrecognizedBang(String),
}

// bash-style history expansion (`!!`, `!-n`, `!n`, `!$`, plus bish's own
// `!?text` and `!/text` in place of bash's plain `!text`, freeing up a
// bare `!word` at the start of a line to mean something else -- see
// below) applied to one freshly-typed line, against `history`'s own
// scope (whatever that particular session's chain can see -- see
// History's own doc comment).
//
// If the line starts with `!` and what immediately follows isn't one of
// the five designators above, this doesn't error the way bash's own
// "event not found" would -- instead the rest of the line (after that
// leading `!`) is meant to run in a child shell, the same idea as vim's
// `:!cmd` or a plain `(cmd)` subshell. What "child shell" means
// concretely differs by caller: the normal shell prompt has no
// restriction on what it can wrap, so `(rest)` (a real subshell) is the
// natural fit; command mode explicitly disallows subshells (see
// command_mode_violation), so it prepends `command ` instead --
// consistent with command mode's own existing "builtins only, `command
// NAME` is the escape hatch for externals" model. Both are the caller's
// job, not this function's -- it just reports which case happened.
//
// Every other `!` in the line (not just a leading one) is still
// scanned and substituted normally -- `rm !$` doesn't require the `!` to
// be the first character.
//
// Returns Err with a bash-style "event not found" message if a
// recognized designator couldn't actually be resolved (no matching
// history entry) -- the whole line is meant to be abandoned in that
// case, nothing runs, matching bash's own behavior.
pub fn expand(line: &str, history: &History) -> Result<Expansion, String> {
    if !line.contains('!') {
        return Ok(Expansion::Substituted(line.to_string()));
    }

    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix('!') {
        if find_designator(rest).is_none() {
            return Ok(Expansion::UnrecognizedBang(rest.to_string()));
        }
    }

    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let ch = line[i..].chars().next().unwrap();
        if ch != '!' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let rest = &line[i + 1..];
        match find_designator(rest) {
            Some((consumed, kind)) => {
                let event_text = &rest[..consumed];
                let resolved = resolve(&kind, history).ok_or_else(|| format!("bish: !{}: event not found", event_text))?;
                out.push_str(&resolved);
                i += 1 + consumed;
            }
            None => {
                out.push('!');
                i += 1;
            }
        }
    }
    Ok(Expansion::Substituted(out))
}

fn history_path(filename: &str) -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(filename))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // `History { path: None, tail: None }` is a fully valid, filesystem-
    // free empty history -- record()'s own disk write is already a no-op
    // whenever `path` is None (see record()'s own body), so every test
    // here is deterministic with no temp files involved.
    fn empty() -> History {
        History { path: None, tail: None }
    }

    #[test]
    fn record_carries_the_cwd_it_was_given() {
        let mut h = empty();
        let cwd = PathBuf::from("/home/user/project");
        h.record("cargo test", Some(&cwd));
        let entries = h.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "cargo test");
        assert_eq!(entries[0].cwd, Some(cwd.as_path()));
    }

    // Simulates what load() produces (cwd: None throughout) without
    // touching the filesystem -- record(entry, None) yields the exact
    // same Node shape a disk-loaded line would.
    #[test]
    fn disk_loaded_shaped_chain_carries_no_cwd() {
        let mut h = empty();
        h.record("ls -la", None);
        h.record("git status", None);
        let entries = h.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| e.cwd.is_none()));
    }

    #[test]
    fn prev_is_some_only_between_two_live_recorded_neighbors() {
        let mut h = empty();
        let cwd = PathBuf::from("/tmp/proj");
        h.record("git status", Some(&cwd));
        h.record("git commit", Some(&cwd));
        let entries = h.entries();
        assert_eq!(entries[0].prev, None); // nothing before the first entry
        assert_eq!(entries[1].prev, Some("git status"));
    }

    #[test]
    fn prev_is_none_for_first_live_entry_after_a_disk_loaded_tail() {
        let mut h = empty();
        // A disk-loaded-shaped entry (no cwd), then the first entry
        // actually recorded live this session.
        h.record("some old command", None);
        let cwd = PathBuf::from("/tmp/proj");
        h.record("cargo build", Some(&cwd));
        let entries = h.entries();
        assert_eq!(entries[0].prev, None);
        // The live entry's own predecessor is the disk-loaded one, which
        // has no cwd -- must not be reported as a real sequence link.
        assert_eq!(entries[1].prev, None);
    }

    #[test]
    fn fork_diverges_with_cwds_intact() {
        let mut parent = empty();
        let cwd_a = PathBuf::from("/tmp/a");
        parent.record("echo parent", Some(&cwd_a));

        let mut child = parent.fork();
        let cwd_b = PathBuf::from("/tmp/b");
        child.record("echo child", Some(&cwd_b));

        let parent_entries = parent.entries();
        assert_eq!(parent_entries.len(), 1);
        assert_eq!(parent_entries[0].cwd, Some(cwd_a.as_path()));

        let child_entries = child.entries();
        assert_eq!(child_entries.len(), 2);
        assert_eq!(child_entries[0].cwd, Some(cwd_a.as_path()));
        assert_eq!(child_entries[1].cwd, Some(cwd_b.as_path()));
    }
}
