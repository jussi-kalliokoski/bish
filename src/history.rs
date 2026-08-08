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
// One History is shared (by plain reference, not Rc<RefCell<_>> -- nothing
// inside a running Shell ever touches history, only repl.rs's own outer
// loop does) across every session under a root: every session's commands
// land in the same growing `entries`/file, interleaved in the order they
// actually happened. What's per-session is a `boundary` index (that
// session's `entries.len()` at the moment it was created), threaded
// through search_backward/search_forward as a floor: a session only ever
// browses its own commands plus whatever's been recorded (by any session)
// from its creation point forward, never anything older. This gets the
// same observable behavior as a hand-rolled persistent linked list with
// per-session snapshot pointers, just via a plain shared Vec + an index --
// old entries are never copied or re-walked, only sliced.

use std::io::Write;
use std::path::PathBuf;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
}

impl History {
    // `filename` is a bare filename under $HOME, e.g. ".bish_history" for
    // the normal shell history or ".bish_cmd_history" for command mode's.
    pub fn load(filename: &str) -> History {
        let path = history_path(filename);
        let mut entries = Vec::new();
        if let Some(p) = &path {
            if let Ok(content) = std::fs::read_to_string(p) {
                entries.extend(content.lines().map(unescape));
            }
        }
        History { path, entries }
    }

    // Records a submitted line (regardless of whether it later succeeds or
    // fails -- bash and fish both do this). Skipped if blank or identical
    // to the immediately preceding entry; this is a lighter touch than
    // fish's full re-dedup-and-move-to-front, but keeps the common
    // "pressed enter on the same command twice" case from cluttering
    // history, which is what this is mainly for.
    pub fn record(&mut self, entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        if self.entries.last().map(|s| s.as_str()) == Some(entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if let Some(p) = &self.path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", escape(entry));
            }
        }
    }

    // Nearest entry at or before index `from` (exclusive; None means
    // "start from the newest"), never older than `boundary`, whose text
    // starts with `prefix`, searching toward older entries. Used for Up.
    pub fn search_backward(&self, prefix: &str, from: Option<usize>, boundary: usize) -> Option<(usize, &str)> {
        let end = from.unwrap_or(self.entries.len());
        self.entries[boundary..end]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, e)| e.starts_with(prefix))
            .map(|(i, e)| (boundary + i, e.as_str()))
    }

    // Nearest entry after index `from` whose text starts with `prefix`,
    // searching toward newer entries. Used for Down. No `boundary` needed
    // here -- searching forward from `from` can never land before it.
    pub fn search_forward(&self, prefix: &str, from: usize) -> Option<(usize, &str)> {
        self.entries
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, e)| e.starts_with(prefix))
            .map(|(i, e)| (i, e.as_str()))
    }

    // `!n`: absolute, 1-based (matches how bash numbers history events).
    // None if out of range or older than `boundary` -- a session can't
    // `!`-reference an event from before it existed any more than it can
    // browse one with Up (see this struct's own doc comment).
    fn entry_by_number(&self, n: usize, boundary: usize) -> Option<&str> {
        let idx = n.checked_sub(1)?;
        if idx < boundary {
            return None;
        }
        self.entries.get(idx).map(|s| s.as_str())
    }

    // `!-n`: n events back from the most recent (1 = most recent, same
    // as `!!`).
    fn entry_back(&self, n: usize, boundary: usize) -> Option<&str> {
        if n == 0 {
            return None;
        }
        let idx = self.entries.len().checked_sub(n)?;
        if idx < boundary {
            return None;
        }
        self.entries.get(idx).map(|s| s.as_str())
    }

    // `!/prefix` (bish's own spelling of bash's plain `!prefix`, freeing
    // up a bare `!word` to mean "run in a child shell" instead -- see
    // expand's own doc comment): most recent entry starting with it.
    fn find_starting_with(&self, prefix: &str, boundary: usize) -> Option<&str> {
        self.entries[boundary..].iter().rev().find(|e| e.starts_with(prefix)).map(|s| s.as_str())
    }

    // `!?text`: most recent entry containing it anywhere.
    fn find_containing(&self, substr: &str, boundary: usize) -> Option<&str> {
        self.entries[boundary..].iter().rev().find(|e| e.contains(substr)).map(|s| s.as_str())
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

fn resolve(kind: &Designator, history: &History, boundary: usize) -> Option<String> {
    match kind {
        Designator::Bang => history.entry_back(1, boundary).map(str::to_string),
        Designator::LastArg => history.entry_back(1, boundary).and_then(|s| s.split_whitespace().next_back()).map(str::to_string),
        Designator::Back(n) => history.entry_back(*n, boundary).map(str::to_string),
        Designator::Number(n) => history.entry_by_number(*n, boundary).map(str::to_string),
        Designator::Contains(s) => history.find_containing(s, boundary).map(str::to_string),
        Designator::StartsWith(s) => history.find_starting_with(s, boundary).map(str::to_string),
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
// below) applied to one freshly-typed line, scoped to `boundary` the
// same way Up/Down browsing already is (History's own doc comment).
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
pub fn expand(line: &str, history: &History, boundary: usize) -> Result<Expansion, String> {
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
                let resolved = resolve(&kind, history, boundary).ok_or_else(|| format!("bish: !{}: event not found", event_text))?;
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
