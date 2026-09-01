// Command history: persisted to ~/.bish_history, one entry per file line
// -- a stored entry's own newlines/backslashes are escaped so a
// multi-line history entry, e.g. a whole recalled for-loop, round-trips
// through the file as a single line rather than fragmenting on reload.
// Each line an entry now carries its own metadata inline, ahead of the
// command, in a shape that is still a runnable shell command; see
// "The on-disk record format" below for what it looks like and why.
// editor.rs is the only consumer of the search methods, driving fish-style
// up/down: prefix-filtered rather than plain chronological recall.
//
// The file is bounded (the `history_size` bishopt, default 10,000
// entries, dropped oldest-first). That bound is also the in-memory one,
// since load() stops at it -- to_vec() re-walks the whole chain on every
// keypress, so an unbounded file was an unbounded per-keystroke cost
// too, and a session that now stays detached for weeks would have found
// the other end of that. Trimming happens at load and, for a session
// that never restarts, from record() once the file reaches twice the
// bound; both go through compact(), under an flock.
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

struct Node {
    entry: String,
    // The directory this entry was recorded from. Persisted as `-d`,
    // so unlike before this survives a reload.
    cwd: Option<PathBuf>,
    // When this entry was recorded, for `history` under
    // `HISTTIMEFORMAT`. `None` for a legacy line, which has no `-t` to
    // read -- and shows no time rather than an invented one.
    time: Option<i64>,
    // This entry's own on-disk id, so a *later* entry can name it as
    // its parent. Meaningless in memory beyond that -- the chain has
    // real pointers and never looks an id up.
    id: u64,
    // The command this one actually ran after, which is NOT the same as
    // `prev`: `prev` is the previous line in a file every bish process
    // appends to concurrently, while this is the real predecessor,
    // resolved through `-p`. Keeping them apart is the whole point of
    // writing a parent id rather than relying on file order.
    parent: Option<Rc<Node>>,
    // Recorded live in this process, as opposed to loaded from disk. A
    // fresh session's first command has no true predecessor -- whatever
    // sits before it in the file is just another window's -- so
    // `record` only takes a parent from a node that is `live`.
    live: bool,
    prev: Option<Rc<Node>>,
}

// A directory-aware, borrowed view of one entry -- see History::entries.
pub struct HistoryEntry<'a> {
    pub text: &'a str,
    pub cwd: Option<&'a Path>,
    /// When it was recorded, if known.
    pub time: Option<i64>,
    // The entry run immediately before this one. Now recovered from
    // disk too, via the parent id each record carries -- which is what
    // demotes `Confidence::Legacy` from "anything that was loaded" to
    // "anything written before this format existed".
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
    // How many entries the file is allowed to keep -- the `history_size`
    // bishopt, passed in rather than read here so history.rs keeps no
    // opinion about where configuration lives. Also the in-memory bound,
    // since load() stops at it: the chain is re-walked by to_vec() on
    // every keypress, so an unbounded file was an unbounded per-keystroke
    // cost as well as an unbounded file.
    limit: usize,
    // Entries appended by this process since the last compaction. The
    // high-water check in record() is against `limit + this`, so a
    // session that stays attached (or detached) for weeks still
    // compacts rather than growing until its next start-up.
    appended: usize,
}

impl History {
    // `filename` is a bare filename under $HOME, e.g. ".bish_history" for
    // the normal shell history or ".bish_cmd_history" for command mode's.
    pub fn load(filename: &str, limit: usize) -> History {
        History::load_at(history_path(filename), limit)
    }

    // The half of load() that has a path already. Split out so tests can
    // point at a real temp file rather than mutating $HOME, which two
    // tests running at once cannot both do.
    fn load_at(path: Option<PathBuf>, limit: usize) -> History {
        let mut tail: Option<Rc<Node>> = None;
        if let Some(p) = &path {
            if let Ok(content) = std::fs::read_to_string(p) {
                let lines: Vec<&str> = content.lines().collect();
                let records: Vec<Record> = lines.iter().map(|l| parse_record(l)).collect();
                // Trim *before* resolving parents, so an entry whose
                // parent was trimmed away resolves to None rather than
                // reaching for a node that is no longer here. Truncating
                // a file that records a forest orphans branches, not
                // just a prefix, and that has to be ordinary rather than
                // exceptional.
                let start = records.len().saturating_sub(limit);
                let mut by_id: std::collections::HashMap<u64, Rc<Node>> = std::collections::HashMap::new();
                for record in &records[start..] {
                    // Last one wins on a duplicate id, the same rule a
                    // repeated key follows elsewhere. A collision costs
                    // one entry the wrong predecessor and nothing more.
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
                // The file was over the bound when this process opened
                // it -- rewrite it now, while we already hold the whole
                // content in memory and know exactly what survives.
                if records.len() > limit {
                    compact(p, limit);
                }
            }
        }
        History { path, tail, limit, appended: 0 }
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
        // Only a live node is a true predecessor: whatever sits at the
        // tail of a freshly loaded chain is the previous line in a file
        // every session appends to, not the command this one just ran.
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
        if let Some(p) = &self.path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                // One `write_all` of one line, deliberately -- not a
                // `writeln!` per field and not a two-line record. A
                // single write to a file opened O_APPEND is serialized
                // on the inode, so concurrent sessions interleave
                // whole entries and never halves of one. Every
                // `window new`/split forks a History that keeps
                // appending here, and a detached daemon makes
                // concurrent writers ordinary rather than an edge case.
                let mut line = format_record(id, parent.map(|n| n.id), cwd, entry);
                line.push('\n');
                let _ = f.write_all(line.as_bytes());
            }
        }
        self.appended += 1;
        self.maybe_compact();
    }

    // Compaction for a session that never restarts. Deliberately
    // hysteretic: the file is allowed to reach twice the bound before
    // being rewritten, so a long session pays for this once every
    // `limit` commands rather than on every one. Skipped entirely when
    // this process has not appended enough to be the one responsible.
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
    // enough" reasoning as to_vec() above -- which is now bounded from
    // the other end too, since load() stops at `limit`.
    //
    // `prev` reads straight off the node's own resolved parent. It used
    // to be computed here, gated on both sides having a cwd, because a
    // disk-loaded node's `.prev` is only the previous line in a shared
    // file and had to be kept out of the sequence heuristic. With a
    // parent id on disk that distinction lives in the data instead: a
    // reloaded entry now knows what it actually followed.
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

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;
const LOCK_NB: i32 = 4;

// Rewrites the history file down to its newest `limit` entries,
// verbatim -- the surviving
// lines are copied across exactly as they were read, never re-rendered,
// so ids and timestamps stay what they were rather than every entry
// claiming to have happened at compaction time.
//
// Held under an exclusive, non-blocking `flock` for the whole rewrite,
// the same primitive (and the same reasoning) session.rs uses for its
// pidfiles. Non-blocking on purpose: if another bish is already
// compacting, the right move is to skip -- it is doing this exact work,
// and a shell prompt is not somewhere to wait on a lock. The lock also
// makes "read the survivors, write them back" a unit, so two sessions
// crossing here cannot each write a different suffix over the other.
//
// Writes in place rather than through a temp file and a rename: every
// other session holds an *append* fd on this inode, and renaming a new
// file over it would send their next entries to an inode nothing will
// ever read again. Truncating in place keeps every one of those fds
// pointed at the file that survives. The cost of that choice is that a
// crash mid-rewrite can leave the file short, which is why nothing here
// treats a missing entry as an error.
fn compact(path: &Path, limit: usize) {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::io::AsRawFd;
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).open(path) else { return };
    let fd = file.as_raw_fd();
    if unsafe { flock(fd, LOCK_EX | LOCK_NB) } != 0 {
        return;
    }
    // Re-read here, under the lock, rather than reusing what the caller
    // already has: between a caller's read and this rewrite another
    // session can append, and that entry would be written straight back
    // out of existence. Reading inside the lock makes the whole
    // read-keep-write a unit.
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        unsafe { flock(fd, LOCK_UN) };
        return;
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    for line in &lines[lines.len().saturating_sub(limit)..] {
        out.push_str(line);
        out.push('\n');
    }
    let done = file
        .set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| file.write_all(out.as_bytes()))
        .and_then(|()| file.flush());
    let _ = done;
    unsafe { flock(fd, LOCK_UN) };
}

// ---------------------------------------------------------------------
// The on-disk record format
// ---------------------------------------------------------------------
//
// Still one line per entry, still appended with a single write -- see
// record() for why that matters. An entry recorded by this version
// carries its metadata inline, ahead of the command it describes:
//
//   : --id 4f2a9c.. -p 91bce0.. -t '2026-09-01T03:45:00Z' -d '/home/j'; cargo test
//
// ...beginning with a space. Two properties fall out of that shape, and
// both were the point of choosing it:
//
//   * The line is a valid shell command that does exactly what the bare
//     command does. `:` is the null builtin -- exec.rs returns
//     Status(0) without so much as looking at its arguments -- and `;`
//     sequences the real command after it, so the exit status is the
//     command's own. A line pasted back into a shell runs what it
//     records. (Single-line entries only: a multi-line entry has its
//     newlines escaped, exactly as it always did, and reads back as a
//     literal `\n`.)
//   * The leading space keeps it *out* of history if it is pasted back
//     (repl.rs's starts_off_the_record), and going the other way
//     guarantees a metadata line can never be mistaken for a legacy
//     bare-command line -- a command beginning with a space was never
//     recorded in the first place, so no pre-existing line can start
//     with one.
//
// **Values are single-quoted, not double-quoted, and that is a security
// property rather than a style choice.** The line is meant to be
// executable, which is exactly what makes it exploitable if its values
// expand. A directory name is attacker-influenceable -- unpack an
// archive, `cd` into it, run anything -- and under double quotes a cwd
// of `/tmp/$(...)` would put a live command substitution into the
// history file that fires the moment the line is pasted back. Verified
// against the real binary: double-quoted, the substitution runs; single
// quoted, it does not. Single quotes expand nothing, and a `'` inside a
// value uses the shell's own `'\''` idiom so the line still parses.
//
// Unknown flags are ignored on read, which is what makes the flag shape
// worth its bytes: exit status, duration or anything else can be added
// later without a second format migration.

// A random 64-bit id, hex-formatted. Random rather than a counter
// because ids have to be unique across *processes* that append to one
// file with no coordination between them, and position-independent so
// that trimming renumbers nothing.
//
// A collision has no error path anywhere downstream and deliberately so
// (see resolve_parents): two entries sharing an id means one of them
// resolves to the wrong predecessor, which costs a worse suggestion and
// nothing else. splitmix64 over a pid/nanosecond seed, the same shape
// exec.rs's own fresh_rng_seed already uses for `$RANDOM`.
fn fresh_id() -> u64 {
    use std::cell::Cell;
    thread_local! { static STATE: Cell<u64> = const { Cell::new(0) }; }
    STATE.with(|cell| {
        let mut x = cell.get();
        if x == 0 {
            x = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545F4914F6CDD1D)
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

// Wraps a value for the shell so the record stays a runnable line.
// `escape` has already made it single-line; this only has to balance
// the quotes, which for single quotes means the one classic idiom:
// close, backslash-quote, reopen.
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

// UTC, `YYYY-MM-DDTHH:MM:SSZ`. Nothing reads this yet -- it is written
// so the format does not need revisiting when something does. Days are
// converted with the standard civil-from-days algorithm rather than
// shelling out to `date`, for the same reason roff.rs parses roff
// itself.
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

// The epoch second an `iso_from_epoch` string names, or None for
// anything that is not one.
//
// The inverse of the civil-from-days conversion above, and deliberately
// strict: a timestamp that does not parse is a timestamp we do not
// know, and `history` shows nothing for it rather than a plausible
// wrong time. Assumes the `Z` these are always written with -- there is
// no other producer.
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
    // days_from_civil, Howard Hinnant's, the exact mirror of the
    // civil_from_days in `iso_from_epoch`.
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

// One parsed line. A legacy bare line yields a Record with no metadata
// at all, which is the honest reading of it -- there is none to recover.
#[derive(Debug, PartialEq)]
struct Record {
    id: Option<u64>,
    parent: Option<u64>,
    cwd: Option<PathBuf>,
    /// When it was recorded, from `-t`. `None` for a legacy bare line,
    /// and for a `-t` that does not parse.
    time: Option<i64>,
    entry: String,
}

// Renders one record. `cwd` goes through `escape` before quoting so a
// directory containing a newline still leaves the record on one line;
// a path that is not valid UTF-8 is stored lossily, which loses the
// directory heuristic for that one entry rather than the entry itself.
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

// Splits ` : <flags>; <command>` into its flag words and the command
// text. Tokenized rather than split on the first `;`, because a
// single-quoted value may legitimately contain one -- a directory can
// be named anything at all, and a format that breaks on `mkdir 'a;b'`
// would be a format that breaks.
//
// None for anything that is not a well-formed metadata line: no
// prefix, an unterminated quote, or no `;` at all. Every one of those
// reads as a legacy bare command instead, which is both the safe answer
// and the right one for a file another version appended to.
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
                // The `'\''` idiom: the section just closed, a literal
                // quote follows, and the value continues in the next
                // section -- which the outer loop picks up on its own.
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

// Reads one file line. Anything that does not parse as a metadata
// record is a bare command -- that covers every line written before
// this format existed, and every line an older bish appends to a file
// this one has already started writing.
fn parse_record(line: &str) -> Record {
    let Some((words, entry)) = split_record(line) else {
        return Record { id: None, parent: None, cwd: None, time: None, entry: unescape(line) };
    };
    let value = |flag: &str| {
        words.iter().position(|w| w == flag).and_then(|i| words.get(i + 1)).map(|s| s.as_str())
    };
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

    // A fully valid, filesystem-free empty history: record()'s own disk
    // write is already a no-op whenever `path` is None, so tests built
    // on this are deterministic with no temp files involved. Tests that
    // need real file behaviour (loading, trimming, legacy lines) use
    // load_at with a temp path instead.
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
        // ` : ` so the null builtin swallows the metadata, a leading
        // space so pasting it back does not re-record it, and the real
        // command after a `;` so the exit status is the command's own.
        assert!(line.starts_with(" : "), "{line}");
        assert!(line.ends_with("; echo hi"), "{line}");
        assert!(!line.contains('\n'), "one entry stays one line");
    }

    #[test]
    fn a_value_is_single_quoted_so_the_line_cannot_execute_it() {
        // Not a style preference. The line is meant to be runnable,
        // which is what would make a double-quoted value runnable too --
        // and a directory name is attacker-influenceable. Verified
        // against the real binary: under double quotes the substitution
        // fires, under single quotes it does not.
        let line = format_record(1, None, Some(Path::new("/tmp/$(echo pwned)")), "ls");
        assert!(line.contains("-d '/tmp/$(echo pwned)'"), "{line}");
        assert!(!line.contains('"'), "{line}");
        assert_eq!(parse_record(&line).cwd, Some(PathBuf::from("/tmp/$(echo pwned)")));
    }

    #[test]
    fn a_value_may_contain_the_delimiter_and_the_quote() {
        // A directory can be named anything, so neither a `;` nor a `'`
        // in one may break the split -- which is why the metadata is
        // tokenized rather than cut at the first `;`.
        for dir in ["/tmp/a;b", "/tmp/it's", "/tmp/;';", "/tmp/a b"] {
            let line = format_record(7, None, Some(Path::new(dir)), "echo x; echo y");
            let got = parse_record(&line);
            assert_eq!(got.cwd, Some(PathBuf::from(dir)), "{line}");
            // ...and the command keeps its own `;`, which must not be
            // escaped: escaping it would change what the line runs.
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
        // Never an error: a truncated or foreign line is more useful
        // read as the text it is than dropped.
        for line in [" : --id 00 -d 'unterminated; ls", " : --id 00 no semicolon here", "plain old command"] {
            let got = parse_record(line);
            assert!(!got.entry.is_empty(), "{line}");
        }
        assert_eq!(parse_record("plain old command").entry, "plain old command");
    }

    #[test]
    fn unknown_flags_are_ignored_rather_than_refused() {
        // What the flag shape is for: a later version can add a field
        // without a second format migration.
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
        // `history` under HISTTIMEFORMAT formats the epoch this
        // recovers, so the two conversions have to be exact inverses.
        for secs in [0_i64, 1_000_000_000, 1_756_697_100, 1_709_208_000] {
            assert_eq!(epoch_from_iso(&iso_from_epoch(secs)), Some(secs), "{secs}");
        }
    }

    #[test]
    fn an_unreadable_timestamp_is_no_timestamp_rather_than_a_wrong_one() {
        // A legacy line has none at all, and a mangled one must not be
        // guessed at -- `history` shows nothing for either.
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
        // A leap day, which is where a hand-rolled civil calendar goes
        // wrong if it is going to.
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

    // Command mode's own history, which records window-management
    // commands and has no shell cwd worth tagging them with. This used
    // to double as a stand-in for a disk-loaded chain, back when a
    // loaded entry never had a cwd either -- see
    // prev_is_none_for_first_live_entry_after_a_disk_loaded_tail, which
    // now loads a real file instead.
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

    // A real file, not a $HOME override -- see load_at.
    fn temp_history(tag: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-hist-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn prev_is_none_for_first_live_entry_after_a_disk_loaded_tail() {
        // Genuinely loaded from a file, rather than approximated with a
        // cwd-less record(): a disk entry now carries a cwd of its own,
        // so "no cwd" stopped being a stand-in for "came from disk" and
        // `live` is the real distinction.
        let path = temp_history("first-live", " : --id 00000000000000a1 -d '/tmp/proj'; some old command\n");
        let mut h = History::load_at(Some(path), 100);
        assert_eq!(h.entries()[0].cwd, Some(Path::new("/tmp/proj")), "a loaded entry keeps its directory");

        let cwd = PathBuf::from("/tmp/proj");
        h.record("cargo build", Some(&cwd));
        let entries = h.entries();
        // The loaded entry is only the previous *line in a file every
        // session appends to*. It is not what this session just ran, so
        // it must not become a sequence link.
        assert_eq!(entries[1].prev, None);
    }

    #[test]
    fn a_reloaded_entry_keeps_the_command_it_actually_followed() {
        // The point of writing a parent id at all: this used to be
        // unrecoverable, and every reloaded entry ranked `Legacy`.
        let path = temp_history(
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
        // Truncation orphans branches rather than a clean prefix, so an
        // unresolvable parent has to be ordinary. `-p ...0a` survives
        // into the kept window with nothing to point at.
        let path = temp_history(
            "orphan",
            " : --id 000000000000000a -d '/w'; oldest\n : --id 000000000000000b -p 000000000000000a -d '/w'; newest\n",
        );
        let h = History::load_at(Some(path), 1);
        let entries = h.entries();
        assert_eq!(entries.len(), 1, "the bound is in entries");
        assert_eq!(entries[0].text, "newest");
        assert_eq!(entries[0].prev, None, "an orphan degrades, it does not dangle");
    }

    #[test]
    fn loading_past_the_bound_rewrites_the_file_to_it() {
        let lines: String = (0..10).map(|i| format!(" : --id {i:016x} -d '/w'; echo {i}\n")).collect();
        let path = temp_history("trim", &lines);
        let h = History::load_at(Some(path.clone()), 4);
        assert_eq!(h.entries().len(), 4);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.lines().count(), 4, "the file is rewritten, not just the view");
        // Verbatim, so ids and timestamps stay what they were rather
        // than every survivor claiming to have happened just now.
        assert!(on_disk.starts_with(" : --id 0000000000000006 -d '/w'; echo 6\n"), "{on_disk}");
    }

    #[test]
    fn a_session_that_never_restarts_still_compacts() {
        // The other half of the bound. Trimming at load covers every
        // ordinary shell; a session detached for weeks never reaches
        // one, and used to grow without limit.
        let path = temp_history("highwater", "");
        let mut h = History::load_at(Some(path.clone()), 4);
        for i in 0..12 {
            h.record(&format!("echo {i}"), Some(Path::new("/w")));
        }
        let on_disk = std::fs::read_to_string(&path).unwrap();
        // Hysteretic on purpose: the file is allowed to reach twice the
        // bound before a rewrite, so this costs one pass per `limit`
        // commands rather than one per command.
        assert!(on_disk.lines().count() <= 8, "{} lines", on_disk.lines().count());
        assert!(on_disk.lines().count() >= 4);
        // Whatever survived is still the newest, and still parses.
        let last = parse_record(on_disk.lines().last().unwrap());
        assert_eq!(last.entry, "echo 11");
    }

    #[test]
    fn a_legacy_bare_line_still_loads_as_a_command() {
        // The 1,000-odd lines already in everyone's file, and anything
        // an older bish appends to one this version has started writing.
        let path = temp_history("legacy", "ls -la\ncargo test\n : --id 000000000000000c -d '/w'; git status\n");
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

// The shell's own view of this list -- see `exec::HistoryAccess` for why
// the `history` builtin reaches it through a trait rather than through
// a field.
//
// `clear` and `delete` rebuild the chain rather than mutating it,
// because a `Node` may be shared with a forked child (`fork` is an
// O(1) `Rc` clone -- see its own doc comment). Rebuilding is what keeps
// `history -c` in one window from emptying another's.
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
            // The rebuilt chain carries no cwd and no parent: neither is
            // recoverable from `entries()`'s bare strings, and inventing
            // either would feed the suggestion engine's own directory
            // and sequence heuristics a lie. Losing them makes those
            // entries rank as `Legacy` -- a real cost now that a
            // reloaded entry would otherwise keep both, and still the
            // honest answer for a chain rebuilt from text alone.
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
        // ...and every live entry knows when it happened, which is what
        // `HISTTIMEFORMAT` shows.
        assert!(HistoryAccess::entries(&h).iter().all(|(_, when)| when.is_some()));
    }

    #[test]
    fn clear_and_delete_rebuild_rather_than_mutate() {
        let mut h = built(&["one", "two", "three"]);
        // A fork shares nodes (it is an O(1) `Rc` clone), so a change
        // here must not reach it -- `history -c` in one window emptying
        // another's is exactly what rebuilding prevents.
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
