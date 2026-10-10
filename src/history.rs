// Command history, persisted to ~/.bish_history one entry per line: an
// entry's newlines and backslashes are escaped, so a recalled multi-line
// loop stays one line. Each line carries its metadata inline, as a
// runnable shell command (see "The on-disk record format" below).
// editor.rs drives prefix-filtered Up/Down through the search methods.
//
// The file keeps at most `history_size` entries (the bishopt, 10,000 by
// default), oldest dropped first, which bounds memory too, since `load`
// stops there and `to_vec` walks the chain on every keypress. It's
// trimmed at load and, for a session that never restarts, by `record`
// once the file reaches twice the bound, both through `compact` under
// an flock. The filename is a parameter, so command mode keeps its own.
//
// The entries are a persistent linked list: each `Node` points at the one
// before, and a `History` is an `Rc` to its newest node. Forking a
// session (`window new`, a split) clones that pointer, and from then on
// each side's `record` extends only its own tail, so one pane's commands
// never show in a sibling's Up/Down or `!` expansion. A shared array with
// a per-session index can't do that: a sibling's later entries would
// still appear. All forks append to the same file.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct Node {
    entry: String,
    // The directory this entry was recorded from, persisted as `-d`.
    cwd: Option<PathBuf>,
    // When this entry was recorded, for `history` under `HISTTIMEFORMAT`.
    // `None` for a legacy line with no `-t`, which shows no time.
    time: Option<i64>,
    // This entry's on-disk id, so a later entry can name it as parent. In
    // memory the chain has real pointers.
    id: u64,
    // The command this one ran after, unlike `prev`, which is the previous
    // line in a file every bish process appends to. Resolved through `-p`;
    // keeping them apart is why a parent id is written.
    parent: Option<Rc<Node>>,
    // Recorded in this process, not loaded. A session's first command has
    // no true predecessor (what's before it in the file is another
    // window's), so `record` takes a parent only from a live node.
    live: bool,
    prev: Option<Rc<Node>>,
}

// A directory-aware view of one entry; see `History::entries`.
pub struct HistoryEntry<'a> {
    pub text: &'a str,
    pub cwd: Option<&'a Path>,
    /// When it was recorded, if known.
    pub time: Option<i64>,
    // The entry run just before this one, recovered from disk through the
    // parent id, so `Confidence::Legacy` means written before this format.
    pub prev: Option<&'a str>,
}

// `Clone` is the same cheap `Rc` clone as `fork`, for a caller wanting a
// snapshot without starting a session.
#[derive(Clone)]
pub struct History {
    path: Option<PathBuf>,
    tail: Option<Rc<Node>>,
    // The most entries the file keeps: the `history_size` bishopt, passed
    // in so this file has no opinion on configuration. Also the in-memory
    // bound, since `load` stops at it.
    limit: usize,
    // Entries appended by this process since the last compaction; the check
    // in `record` is against `limit + appended`, so a long-lived session
    // still compacts.
    appended: usize,
}

/// Takes group and other permissions off an existing history file, only
/// when it has them, to avoid a syscall per append. Failure is ignored:
/// a filesystem that can't represent the mode is still worth appending
/// to.
fn tighten_history_permissions(path: &Path) {
    crate::platform::restrict_to_owner(path);
}

impl History {
    // `filename` is a bare name under $HOME: `.bish_history`, or
    // `.bish_cmd_history` for command mode.
    pub fn load(filename: &str, limit: usize) -> History {
        History::load_at(history_path(filename), limit)
    }

    // `load` with a path in hand, so tests can use a temp file rather than
    // change $HOME.
    fn load_at(path: Option<PathBuf>, limit: usize) -> History {
        let mut tail: Option<Rc<Node>> = None;
        if let Some(p) = &path {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().collect();
                let records: Vec<Record> = lines.iter().map(|l| parse_record(l)).collect();
                // Trimmed before parents are resolved, so an entry whose
                // parent was trimmed gets `None`. Truncating a file of a
                // forest orphans branches, so that's ordinary.
                let start = records.len().saturating_sub(limit);
                let mut by_id: std::collections::HashMap<u64, Rc<Node>> = std::collections::HashMap::new();
                for record in &records[start..] {
                    // On a duplicate id the last wins; a collision costs
                    // one entry the wrong predecessor.
                    let parent = record.parent.and_then(|p| by_id.get(&p).cloned());
                    let node = Rc::new(Node {
                        entry: record.entry.clone(),
                        cwd: record.cwd.clone(),
                        time: record.time,
                        id: record.id.unwrap_or(0),
                        parent,
                        live: false,
                        prev: tail.take(),
                    });
                    if let Some(id) = record.id {
                        by_id.insert(id, Rc::clone(&node));
                    }
                    tail = Some(node);
                }
                // Over the bound at open: rewrite now, with the content in
                // hand.
                if records.len() > limit {
                    compact(p, limit);
                }
            }
        }
        History { path, tail, limit, appended: 0 }
    }

    // An O(1) fork for a new session (`window new`, a split): the child
    // sees everything the parent does, then each side's `record` extends
    // only its own tail.
    pub fn fork(&self) -> History {
        self.clone()
    }

    // Records a submitted line, whether or not it succeeds, as bash does.
    // Skipped when blank or the same as the previous entry, so pressing
    // Enter on one command twice doesn't clutter history. `cwd` is where the
    // command runs, for the suggestion engine (see `HistoryEntry`); `None`
    // for command mode's history.
    pub fn record(&mut self, entry: &str, cwd: Option<&Path>) {
        if entry.trim().is_empty() {
            return;
        }
        if self.tail.as_ref().map(|n| n.entry.as_str()) == Some(entry) {
            return;
        }
        // Only a live node is a true predecessor; a loaded tail is just the
        // previous line in a shared file.
        let parent = self.tail.as_ref().filter(|n| n.live).map(Rc::clone);
        let id = fresh_id();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        self.tail = Some(Rc::new(Node {
            entry: entry.to_string(),
            cwd: cwd.map(Path::to_path_buf),
            time: Some(now.max(0)),
            id,
            parent: parent.clone(),
            live: true,
            prev: self.tail.take(),
        }));

        // Created 0600, not per the umask: this is every command typed at a
        // prompt, passwords and tokens included. bash creates its history
        // 0600 too. `mode` applies only to a new file, so an existing one
        // is tightened below.
        if let Some(p) = &self.path
            && let Ok(mut f) = crate::platform::create_private_file(p, true)
        {
            tighten_history_permissions(p);
            // One `write_all` of one line: a single write to an O_APPEND
            // file is atomic on the inode, so concurrent sessions
            // interleave whole entries. Every split forks a `History`
            // appending here, and a detached daemon makes concurrent
            // writers ordinary.
            let mut line = format_record(id, parent.map(|n| n.id), cwd, entry);
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
        self.appended += 1;
        self.maybe_compact();
    }

    // Compaction for a session that never restarts, hysteretic: the file
    // may reach twice the bound first, so a long session pays once per
    // `limit` commands. Skipped unless this process appended enough to be
    // responsible.
    fn maybe_compact(&mut self) {
        if self.limit == 0 || self.appended < self.limit {
            return;
        }
        self.appended = 0;
        let Some(path) = self.path.clone() else { return };
        let Ok(content) = std::fs::read_to_string(&path) else { return };
        if content.lines().count() <= self.limit * 2 {
            return;
        }
        compact(&path, self.limit);
    }

    // This chain as an oldest-first `Vec`, so lookups are index math. Walked
    // on every call rather than cached: at these sizes it's unmeasurable,
    // and nothing can go stale.
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

    // A directory-aware, oldest-first view for the suggestion engine
    // (bishedit::suggestion): each entry's directory, and what ran just
    // before it (`prev`, from the node's resolved parent). Walked on every
    // call, like `to_vec`.
    pub fn entries(&self) -> Vec<HistoryEntry<'_>> {
        let mut v = Vec::new();
        let mut cur = &self.tail;
        while let Some(node) = cur {
            v.push(HistoryEntry {
                text: node.entry.as_str(),
                cwd: node.cwd.as_deref(),
                time: node.time,
                prev: node.parent.as_ref().map(|p| p.entry.as_str()),
            });
            cur = &node.prev;
        }
        v.reverse();
        v
    }

    // The nearest entry before index `from` (`None` starts at the newest)
    // starting with `prefix`, searching older. Used for Up.
    pub fn search_backward(&self, prefix: &str, from: Option<usize>) -> Option<(usize, String)> {
        let v = self.to_vec();
        let end = from.unwrap_or(v.len());
        v[..end].iter().enumerate().rev().find(|(_, e)| e.starts_with(prefix)).map(|(i, e)| (i, e.to_string()))
    }

    // The nearest entry after index `from` starting with `prefix`,
    // searching newer. Used for Down.
    pub fn search_forward(&self, prefix: &str, from: usize) -> Option<(usize, String)> {
        let v = self.to_vec();
        v.iter().enumerate().skip(from + 1).find(|(_, e)| e.starts_with(prefix)).map(|(i, e)| (i, e.to_string()))
    }

    // `!n`: absolute, 1-based, as bash numbers events.
    fn entry_by_number(&self, n: usize) -> Option<String> {
        let idx = n.checked_sub(1)?;
        self.to_vec().get(idx).map(|s| s.to_string())
    }

    // `!-n`: n events back (1 is the latest, like `!!`).
    fn entry_back(&self, n: usize) -> Option<String> {
        if n == 0 {
            return None;
        }
        let v = self.to_vec();
        let idx = v.len().checked_sub(n)?;
        v.get(idx).map(|s| s.to_string())
    }

    // `!/prefix`, bish's spelling of bash's `!prefix`, leaving a bare
    // `!word` to mean run in a child shell (see `expand`): the latest entry
    // starting with it.
    fn find_starting_with(&self, prefix: &str) -> Option<String> {
        self.to_vec().iter().rev().find(|e| e.starts_with(prefix)).map(|s| s.to_string())
    }

    // `!?text`: the latest entry containing it.
    fn find_containing(&self, substr: &str) -> Option<String> {
        self.to_vec().iter().rev().find(|e| e.contains(substr)).map(|s| s.to_string())
    }
}

// A recognized `!` designator; see `find_designator`.
enum Designator<'a> {
    Bang,
    LastArg,
    Back(usize),
    Number(usize),
    Contains(&'a str),
    StartsWith(&'a str),
}

// Parses a designator right after the `!` (`rest` is what follows).
// Returns it and the bytes it consumes, or `None`, when the `!` is left
// alone or, at the line's start, means a child shell (see `expand`).
fn find_designator(rest: &str) -> Option<(usize, Designator<'_>)> {
    let mut chars = rest.chars();
    match chars.next()? {
        '!' => Some((1, Designator::Bang)),
        '$' => Some((1, Designator::LastArg)),
        '-' => {
            let digits: String = rest[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() { None } else { Some((1 + digits.len(), Designator::Back(digits.parse().ok()?))) }
        }
        c if c.is_ascii_digit() => {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            Some((digits.len(), Designator::Number(digits.parse().ok()?)))
        }
        '?' => {
            let body = &rest[1..];
            // `!?text?`, or `!?text` up to a blank or the end.
            if let Some(end) = body.find('?') {
                Some((1 + end + 1, Designator::Contains(&body[..end])))
            } else {
                let end = body.find(char::is_whitespace).unwrap_or(body.len());
                if end == 0 { None } else { Some((1 + end, Designator::Contains(&body[..end]))) }
            }
        }
        '/' => {
            let body = &rest[1..];
            let end = body.find(char::is_whitespace).unwrap_or(body.len());
            if end == 0 { None } else { Some((1 + end, Designator::StartsWith(&body[..end]))) }
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

// A typed line after history expansion. `Substituted` goes to the lexer
// as usual. `UnrecognizedBang`: the line started with a `!` that isn't a
// designator, and the rest runs in a child shell, as the caller decides
// (see `expand`).
pub enum Expansion {
    Substituted(String),
    UnrecognizedBang(String),
}

// History expansion (`!!`, `!-n`, `!n`, `!$`, and bish's `!?text` and
// `!/text` in place of bash's `!text`) on one typed line, over this
// session's chain.
//
// A line starting with `!` and no designator isn't "event not found":
// the rest runs in a child shell, like vim's `:!cmd`. The prompt wraps it
// as `(rest)`; command mode, which allows no subshells, prepends
// `command `. That's the caller's job; this reports the case.
//
// Any `!` in the line is expanded, not just a leading one (`rm !$`). A
// designator that matches nothing is an `Err` with bash's "event not
// found", and nothing runs.
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

// Rewrites the history file to its newest `limit` lines, copied
// verbatim so ids and times stay. Under a non-blocking exclusive
// `flock`, as session.rs's pidfiles: if another bish is compacting,
// skip rather than wait at a prompt, and the lock makes read-and-write
// one unit. In place, not via a temp file and rename: other sessions
// hold append fds on this inode, and a rename would send their entries
// to a file nobody reads. A crash mid-rewrite can leave it short, so a
// missing entry is never an error.
fn compact(path: &Path, limit: usize) {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).open(path) else { return };
    let fd = file.as_raw_fd();
    if crate::platform::lock_exclusive(fd).is_err() {
        return;
    }
    // Re-read under the lock: another session may have appended since the
    // caller read, and that entry would be lost.
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        crate::platform::unlock(fd);
        return;
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    for line in &lines[lines.len().saturating_sub(limit)..] {
        out.push_str(line);
        out.push('\n');
    }
    let done = file.set_len(0).and_then(|()| file.seek(SeekFrom::Start(0))).and_then(|_| file.write_all(out.as_bytes())).and_then(|()| file.flush());
    let _ = done;
    crate::platform::unlock(fd);
}

// The on-disk record format: one line per entry, one write each (see
// `record`), metadata ahead of the command:
//
//   : --id 4f2a9c.. -p 91bce0.. -t '2026-09-01T03:45:00Z' -d '/home/j'; cargo test
//
// beginning with a space. So:
//
//   * The line is a shell command doing just what the bare command does:
//     `:` ignores its arguments, and `;` runs the command after it with
//     its own status. (A multi-line entry's newlines are escaped and read
//     back as literal `\n`.)
//   * The leading space keeps it out of history if pasted back
//     (repl.rs's `starts_off_the_record`), and no legacy line starts with
//     one, since such commands were never recorded.
//
// Values are single-quoted as a security property: a pasted line runs, and
// a cwd like `/tmp/$(...)` (an unpacked archive can name one) would run
// under double quotes. A `'` inside uses the `'\''` idiom. Unknown flags
// are ignored on read, so more can be added without a migration.

// A random 64-bit id in hex: unique across processes appending to one
// file without coordination, and independent of position, so trimming
// renumbers nothing. A collision only gives one entry the wrong
// predecessor (see `resolve_parents`). splitmix64 over a pid and
// nanosecond seed, as `exec::fresh_rng_seed`.
fn fresh_id() -> u64 {
    use std::cell::Cell;
    thread_local! { static STATE: Cell<u64> = const { Cell::new(0) }; }
    STATE.with(|cell| {
        let mut x = cell.get();
        if x == 0 {
            x = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0x2545F4914F6CDD1D)
                ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
            if x == 0 {
                x = 0x2545F4914F6CDD1D;
            }
        }
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        cell.set(x);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    })
}

// Single-quotes a value so the record stays runnable. `escape` already
// made it one line; an embedded `'` becomes `'\''`.
fn single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// UTC, `YYYY-MM-DDTHH:MM:SSZ`, by the civil-from-days algorithm.
fn iso_from_epoch(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// The epoch second an `iso_from_epoch` string names, or `None`. Strict:
// an unparseable time shows as nothing, not a plausible wrong one.
// Assumes the `Z` these are always written with.
fn epoch_from_iso(text: &str) -> Option<i64> {
    let b: Vec<char> = text.chars().collect();
    if b.len() != 20 || b[4] != '-' || b[7] != '-' || b[10] != 'T' || b[13] != ':' || b[16] != ':' || b[19] != 'Z' {
        return None;
    }
    let num = |from: usize, to: usize| text[from..to].parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil, the inverse of `iso_from_epoch`'s
    // civil_from_days.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

fn iso_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    iso_from_epoch(secs as i64)
}

// One parsed line. A legacy bare line has no metadata.
#[derive(Debug, PartialEq)]
struct Record {
    id: Option<u64>,
    parent: Option<u64>,
    cwd: Option<PathBuf>,
    /// When it was recorded, from `-t`; `None` for a legacy line or an
    /// unparseable `-t`.
    time: Option<i64>,
    entry: String,
}

// Renders one record. `cwd` goes through `escape` first, so a directory
// with a newline stays on one line; a non-UTF-8 path is stored lossily,
// losing only that entry's directory.
fn format_record(id: u64, parent: Option<u64>, cwd: Option<&Path>, entry: &str) -> String {
    let mut out = format!(" : --id {id:016x}");
    if let Some(p) = parent {
        out.push_str(&format!(" -p {p:016x}"));
    }
    out.push_str(&format!(" -t {}", single_quote(&iso_now())));
    if let Some(dir) = cwd {
        out.push_str(&format!(" -d {}", single_quote(&escape(&dir.to_string_lossy()))));
    }
    out.push_str("; ");
    out.push_str(&escape(entry));
    out
}

// Splits ` : <flags>; <command>` into flag words and the command,
// tokenizing because a quoted value may contain `;` (`mkdir 'a;b'`).
// `None` for anything else (no prefix, an unterminated quote, no `;`),
// which reads as a legacy bare command, the right answer for a file
// another version appended to.
fn split_record(line: &str) -> Option<(Vec<String>, String)> {
    let rest = line.strip_prefix(" : ")?;
    let ch: Vec<char> = rest.chars().collect();
    let (mut words, mut cur, mut have, mut i) = (Vec::new(), String::new(), false, 0);
    while i < ch.len() {
        match ch[i] {
            ';' => {
                if have {
                    words.push(cur);
                }
                let tail: String = ch[i + 1..].iter().collect();
                return Some((words, tail.strip_prefix(' ').map(str::to_string).unwrap_or(tail)));
            }
            ' ' => {
                if have {
                    words.push(std::mem::take(&mut cur));
                    have = false;
                }
                i += 1;
            }
            '\'' => {
                have = true;
                i += 1;
                while i < ch.len() && ch[i] != '\'' {
                    cur.push(ch[i]);
                    i += 1;
                }
                if i >= ch.len() {
                    return None;
                }
                i += 1;
                // The `'\''` idiom: the section closed, a literal quote,
                // and the value continues in the next section.
                if i + 1 < ch.len() && ch[i] == '\\' && ch[i + 1] == '\'' {
                    cur.push('\'');
                    i += 2;
                }
            }
            c => {
                have = true;
                cur.push(c);
                i += 1;
            }
        }
    }
    None
}

// Reads one file line. Anything that isn't a metadata record is a bare
// command: every line from before this format, and lines an older bish
// appends.
fn parse_record(line: &str) -> Record {
    let Some((words, entry)) = split_record(line) else {
        return Record { id: None, parent: None, cwd: None, time: None, entry: unescape(line) };
    };
    let value = |flag: &str| words.iter().position(|w| w == flag).and_then(|i| words.get(i + 1)).map(|s| s.as_str());
    let hex = |flag: &str| value(flag).and_then(|v| u64::from_str_radix(v, 16).ok());
    Record {
        id: hex("--id"),
        parent: hex("-p"),
        cwd: value("-d").map(|d| PathBuf::from(unescape(d))),
        time: value("-t").and_then(|t| epoch_from_iso(&unescape(t))),
        entry: unescape(&entry),
    }
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

    // An empty history with no file: `record` writes nothing without a
    // path, so tests on this need no temp files. Tests of file behaviour use
    // `load_at` with a temp path.
    fn empty() -> History {
        History { path: None, tail: None, limit: 1000, appended: 0 }
    }

    #[test]
    fn a_record_round_trips_through_the_line_it_renders() {
        let line = format_record(0x4f2a, Some(0x91bc), Some(Path::new("/home/j/bish")), "cargo test");
        let got = parse_record(&line);
        assert_eq!(got.id, Some(0x4f2a));
        assert_eq!(got.parent, Some(0x91bc));
        assert_eq!(got.cwd, Some(PathBuf::from("/home/j/bish")));
        assert_eq!(got.entry, "cargo test");
    }

    #[test]
    fn the_line_is_shaped_to_run_as_the_command_it_records() {
        let line = format_record(1, None, Some(Path::new("/w")), "echo hi");
        // ` : ` so `:` swallows the metadata, a leading space so pasting it
        // back doesn't re-record it, and the command after `;` for its own
        // status.
        assert!(line.starts_with(" : "), "{line}");
        assert!(line.ends_with("; echo hi"), "{line}");
        assert!(!line.contains('\n'), "one entry stays one line");
    }

    #[test]
    fn a_value_is_single_quoted_so_the_line_cannot_execute_it() {
        // Single quotes: a runnable line would run a double-quoted
        // substitution from a directory name.
        let line = format_record(1, None, Some(Path::new("/tmp/$(echo pwned)")), "ls");
        assert!(line.contains("-d '/tmp/$(echo pwned)'"), "{line}");
        assert!(!line.contains('"'), "{line}");
        assert_eq!(parse_record(&line).cwd, Some(PathBuf::from("/tmp/$(echo pwned)")));
    }

    #[test]
    fn a_value_may_contain_the_delimiter_and_the_quote() {
        // A directory can be named anything, so `;` and `'` in one can't
        // break the split.
        for dir in ["/tmp/a;b", "/tmp/it's", "/tmp/;';", "/tmp/a b"] {
            let line = format_record(7, None, Some(Path::new(dir)), "echo x; echo y");
            let got = parse_record(&line);
            assert_eq!(got.cwd, Some(PathBuf::from(dir)), "{line}");
            // The command's own `;` isn't escaped, which would change what
            // runs.
            assert_eq!(got.entry, "echo x; echo y", "{line}");
        }
    }

    #[test]
    fn a_multi_line_entry_still_occupies_one_line() {
        let line = format_record(1, None, None, "for i in 1 2 3\ndo echo $i\ndone");
        assert!(!line.contains('\n'));
        assert_eq!(parse_record(&line).entry, "for i in 1 2 3\ndo echo $i\ndone");
    }

    #[test]
    fn an_unparseable_metadata_line_reads_as_a_plain_command() {
        // Never an error: a truncated or foreign line is read as its text.
        for line in [" : --id 00 -d 'unterminated; ls", " : --id 00 no semicolon here", "plain old command"] {
            let got = parse_record(line);
            assert!(!got.entry.is_empty(), "{line}");
        }
        assert_eq!(parse_record("plain old command").entry, "plain old command");
    }

    #[test]
    fn unknown_flags_are_ignored_rather_than_refused() {
        // A later version can add fields without a migration.
        let got = parse_record(" : --id 000000000000002a --exit 3 --duration '1.5' -d '/w'; make");
        assert_eq!(got.id, Some(42));
        assert_eq!(got.cwd, Some(PathBuf::from("/w")));
        assert_eq!(got.entry, "make");
    }

    #[test]
    fn ids_do_not_repeat() {
        let ids: std::collections::HashSet<u64> = (0..1000).map(|_| fresh_id()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[test]
    fn a_timestamp_round_trips_through_the_record() {
        // `history` formats the epoch this recovers, so the two must be exact
        // inverses.
        for secs in [0_i64, 1_000_000_000, 1_756_697_100, 1_709_208_000] {
            assert_eq!(epoch_from_iso(&iso_from_epoch(secs)), Some(secs), "{secs}");
        }
    }

    #[test]
    fn an_unreadable_timestamp_is_no_timestamp_rather_than_a_wrong_one() {
        // Legacy lines have none, and mangled ones aren't guessed at.
        for bad in ["", "not a time", "2001-09-09", "2001-09-09T01:46:40", "2001-13-09T01:46:40Z", "2001-09-32T01:46:40Z"] {
            assert_eq!(epoch_from_iso(bad), Option::None, "{bad}");
        }
        assert_eq!(parse_record("legacy bare line").time, Option::None);
        assert_eq!(parse_record(" : --id 0a -t 'nonsense'; echo hi").time, Option::None);
    }

    #[test]
    fn a_timestamp_is_utc_iso_8601() {
        assert_eq!(iso_from_epoch(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_epoch(1_756_697_100), "2025-09-01T03:25:00Z");
        // A leap day, where a hand-rolled calendar goes wrong if it will.
        assert_eq!(iso_from_epoch(1_709_208_000), "2024-02-29T12:00:00Z");
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

    // Command mode's history, of window commands with no cwd to tag.
    #[test]
    fn an_entry_recorded_without_a_directory_reports_none() {
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

    /// A real file rather than a $HOME override (see `load_at`). The
    /// directory comes back too because it owns the file and is removed when
    /// dropped.
    fn temp_history(tag: &str, contents: &str) -> (crate::tempdir::TempDir, PathBuf) {
        let dir = crate::tempdir::TempDir::new(&format!("hist-{tag}"));
        let path = dir.join("history");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    // The history file is created 0600, not per the umask.
    #[test]
    fn the_history_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::tempdir::TempDir::new("hist-mode");
        let path = dir.join("history");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

        let mut h = History::load_at(Some(path.clone()), 100);
        h.record("echo one", None);
        assert_eq!(mode(&path), 0o600, "a file this created must not be readable by anyone else");

        // An existing loose one is tightened: a umask chose 0644, not a
        // person.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        h.record("echo two", None);
        assert_eq!(mode(&path), 0o600);
        // And the entries are still there.
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }

    #[test]
    fn prev_is_none_for_first_live_entry_after_a_disk_loaded_tail() {
        // Loaded from a real file: `live`, not a missing cwd, marks a disk
        // entry.
        let (_dir, path) = temp_history("first-live", " : --id 00000000000000a1 -d '/tmp/proj'; some old command\n");
        let mut h = History::load_at(Some(path), 100);
        assert_eq!(h.entries()[0].cwd, Some(Path::new("/tmp/proj")), "a loaded entry keeps its directory");

        let cwd = PathBuf::from("/tmp/proj");
        h.record("cargo build", Some(&cwd));
        let entries = h.entries();
        // The loaded entry is only the previous line in a shared file, not
        // what this session ran, so it's no sequence link.
        assert_eq!(entries[1].prev, None);
    }

    #[test]
    fn a_reloaded_entry_keeps_the_command_it_actually_followed() {
        // A parent id recovers the link across a reload.
        let (_dir, path) = temp_history(
            "parents",
            " : --id 000000000000000a -d '/w'; git add -A\n : --id 000000000000000b -p 000000000000000a -d '/w'; git commit\n",
        );
        let h = History::load_at(Some(path), 100);
        let entries = h.entries();
        assert_eq!(entries[0].prev, None);
        assert_eq!(entries[1].prev, Some("git add -A"));
    }

    #[test]
    fn a_parent_that_was_trimmed_away_resolves_to_nothing() {
        // Truncation orphans branches, so an unresolvable parent is ordinary:
        // `-p ...0a` survives with nothing to point at.
        let (_dir, path) =
            temp_history("orphan", " : --id 000000000000000a -d '/w'; oldest\n : --id 000000000000000b -p 000000000000000a -d '/w'; newest\n");
        let h = History::load_at(Some(path), 1);
        let entries = h.entries();
        assert_eq!(entries.len(), 1, "the bound is in entries");
        assert_eq!(entries[0].text, "newest");
        assert_eq!(entries[0].prev, None, "an orphan degrades, it does not dangle");
    }

    #[test]
    fn loading_past_the_bound_rewrites_the_file_to_it() {
        let lines: String = (0..10).map(|i| format!(" : --id {i:016x} -d '/w'; echo {i}\n")).collect();
        let (_dir, path) = temp_history("trim", &lines);
        let h = History::load_at(Some(path.clone()), 4);
        assert_eq!(h.entries().len(), 4);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().count(), 4, "the file is rewritten, not just the view");
        // Verbatim, so ids and times stay.
        assert!(on_disk.starts_with(" : --id 0000000000000006 -d '/w'; echo 6\n"), "{on_disk}");
    }

    #[test]
    fn a_session_that_never_restarts_still_compacts() {
        // The other half of the bound: a session detached for weeks never
        // reloads, so `record` compacts.
        let (_dir, path) = temp_history("highwater", "");
        let mut h = History::load_at(Some(path.clone()), 4);
        for i in 0..12 {
            h.record(&format!("echo {i}"), Some(Path::new("/w")));
        }
        let on_disk = std::fs::read_to_string(&path).unwrap();
        // Hysteretic: up to twice the bound before a rewrite.
        assert!(on_disk.lines().count() <= 8, "{} lines", on_disk.lines().count());
        assert!(on_disk.lines().count() >= 4);
        // What survived is the newest, and still parses.
        let last = parse_record(on_disk.lines().last().unwrap());
        assert_eq!(last.entry, "echo 11");
    }

    #[test]
    fn a_legacy_bare_line_still_loads_as_a_command() {
        // Lines from before this format, and from an older bish appending to
        // the same file.
        let (_dir, path) = temp_history("legacy", "ls -la\ncargo test\n : --id 000000000000000c -d '/w'; git status\n");
        let h = History::load_at(Some(path), 100);
        let entries = h.entries();
        assert_eq!(entries.iter().map(|e| e.text).collect::<Vec<_>>(), vec!["ls -la", "cargo test", "git status"]);
        assert_eq!(entries[0].cwd, None, "there is no directory to recover, and none is invented");
        assert_eq!(entries[2].cwd, Some(Path::new("/w")));
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

// The shell's view of this list (see `exec::HistoryAccess`). `clear`
// and `delete` rebuild the chain rather than mutate it, since a `Node`
// may be shared with a fork: `history -c` in one window mustn't empty
// another's.
impl crate::exec::HistoryAccess for History {
    fn entries(&self) -> Vec<(String, Option<i64>)> {
        History::entries(self).into_iter().map(|e| (e.text.to_string(), e.time)).collect()
    }

    fn clear(&mut self) {
        self.tail = None;
    }

    fn delete(&mut self, n: usize) -> bool {
        let kept: Vec<String> = History::entries(self).into_iter().map(|e| e.text.to_string()).collect();
        if n == 0 || n > kept.len() {
            return false;
        }
        let mut tail: Option<Rc<Node>> = None;
        for (i, entry) in kept.into_iter().enumerate() {
            if i + 1 == n {
                continue;
            }
            // The rebuilt chain has no cwd or parent, since entries' bare
            // strings can't recover them, and inventing them would mislead
            // the suggestion heuristics; those entries rank `Legacy`.
            tail = Some(Rc::new(Node { entry, cwd: None, time: None, id: fresh_id(), parent: None, live: false, prev: tail.take() }));
        }
        self.tail = tail;
        true
    }
}

#[cfg(test)]
mod history_access_tests {
    use super::*;
    use crate::exec::HistoryAccess;

    fn built(entries: &[&str]) -> History {
        let mut h = History { path: None, tail: None, limit: 1000, appended: 0 };
        for e in entries {
            h.record(e, Some(std::path::Path::new("/tmp")));
        }
        h
    }

    #[test]
    fn the_shell_sees_every_entry_oldest_first() {
        let h = built(&["one", "two", "three"]);
        let texts: Vec<String> = HistoryAccess::entries(&h).into_iter().map(|(t, _)| t).collect();
        assert_eq!(texts, vec!["one".to_string(), "two".to_string(), "three".to_string()]);
        // Every live entry knows its time, for `HISTTIMEFORMAT`.
        assert!(HistoryAccess::entries(&h).iter().all(|(_, when)| when.is_some()));
    }

    #[test]
    fn clear_and_delete_rebuild_rather_than_mutate() {
        let mut h = built(&["one", "two", "three"]);
        // A fork shares nodes, so a change here mustn't reach it.
        let forked = h.fork();

        assert!(h.delete(2));
        let texts: Vec<String> = HistoryAccess::entries(&h).into_iter().map(|(t, _)| t).collect();
        assert_eq!(texts, vec!["one".to_string(), "three".to_string()]);
        assert_eq!(HistoryAccess::entries(&forked).len(), 3, "the fork is untouched");

        assert!(!h.delete(0), "1-based, so 0 is out of range");
        assert!(!h.delete(99));

        h.clear();
        assert!(HistoryAccess::entries(&h).is_empty());
        assert_eq!(HistoryAccess::entries(&forked).len(), 3);
    }

    #[test]
    fn a_shell_with_no_history_answers_emptily_rather_than_failing() {
        let mut none = crate::exec::NoHistory;
        assert!(none.entries().is_empty());
        assert!(!none.delete(1));
        none.clear();
    }
}
