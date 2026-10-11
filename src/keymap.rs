// Key notation: the `<C-d>`/`<Esc>`/`<Space>` spelling `::bish map`
// reads and prints, parsed to `editor::Key` and formatted back from one
// table (`NAMED`), so anything typeable is printable and the reverse.
//
// Not `vimkeys::key_label`, which renders the pending-input hint (`^D`,
// arrow glyphs): display, lossy, never read back. This is a format that
// round-trips exactly over every mappable key.
//
// Vim's spelling wherever vim has one: `<C-d>`, `<Esc>`, `<CR>`, `<BS>`,
// `<Tab>`, `<S-Tab>`, `<Space>`, `<Del>`, `<Home>`, `<End>`, `<PageUp>`,
// `<PageDown>`, the arrows, and `<lt>` for a literal `<`.

use crate::editor::Key;

// Every key that isn't `Key::Char`, with its canonical spelling, in
// `Key` order, covering the mappable variants (see `is_mappable`).
const NAMED: &[(&str, Key)] = &[
    ("<C-Space>", Key::CtrlSpace),
    ("<CR>", Key::Enter),
    ("<BS>", Key::Backspace),
    ("<Del>", Key::Delete),
    ("<Left>", Key::Left),
    ("<Right>", Key::Right),
    ("<Up>", Key::Up),
    ("<Down>", Key::Down),
    ("<Home>", Key::Home),
    ("<End>", Key::End),
    ("<PageUp>", Key::PageUp),
    ("<PageDown>", Key::PageDown),
    ("<Esc>", Key::Escape),
    ("<A-Left>", Key::AltLeft),
    ("<A-Right>", Key::AltRight),
    ("<A-Up>", Key::AltUp),
    ("<Tab>", Key::Tab),
    ("<S-Tab>", Key::BackTab),
    ("<C-a>", Key::CtrlA),
    ("<C-b>", Key::CtrlB),
    ("<C-c>", Key::CtrlC),
    ("<C-d>", Key::CtrlD),
    ("<C-e>", Key::CtrlE),
    ("<C-f>", Key::CtrlF),
    ("<C-g>", Key::CtrlG),
    ("<C-k>", Key::CtrlK),
    ("<C-l>", Key::CtrlL),
    ("<C-n>", Key::CtrlN),
    ("<C-o>", Key::CtrlO),
    ("<C-p>", Key::CtrlP),
    ("<C-q>", Key::CtrlQ),
    ("<C-r>", Key::CtrlR),
    ("<C-s>", Key::CtrlS),
    ("<C-t>", Key::CtrlT),
    ("<C-u>", Key::CtrlU),
    ("<C-v>", Key::CtrlV),
    ("<C-w>", Key::CtrlW),
    ("<C-x>", Key::CtrlX),
    ("<C-y>", Key::CtrlY),
    ("<C-z>", Key::CtrlZ),
];

// A space is `<Space>`, since whitespace separates a mapping's two
// halves on the command line.
const SPACE: &str = "<Space>";
// `<` itself, so a mapping can contain one without opening a name.
const LT: &str = "<lt>";
// Vim's spelling for doing nothing; only a whole right-hand side, being
// the absence of keys.
const NOP: &str = "<Nop>";

// Whether a key can carry a mapping. Not `Mouse`, an event with
// coordinates that views handle directly; not `Unknown`, the bytes
// nothing decoded; not `PasteStart`/`PasteEnd`, which a terminal puts
// around pastes, so a mapping would fire on every paste.
pub fn is_mappable(key: Key) -> bool {
    !matches!(key, Key::Mouse(_) | Key::Unknown | Key::PasteStart | Key::PasteEnd)
}

// One key's canonical spelling, total over mappable keys, so
// `format_keys` can't fail.
fn format_key(key: Key) -> String {
    if let Key::Char(c) = key {
        return match c {
            ' ' => SPACE.to_string(),
            '<' => LT.to_string(),
            c => c.to_string(),
        };
    }
    for (name, k) in NAMED {
        if *k == key {
            return (*name).to_string();
        }
    }
    // Only for a non-mappable key, which no mapping holds; a listing
    // shouldn't panic.
    "<unknown>".to_string()
}

/// A sequence's canonical spelling, as `::bish map` prints and reads it.
pub fn format_keys(keys: &[Key]) -> String {
    keys.iter().map(|k| format_key(*k)).collect()
}

/// The same for a right-hand side, where no keys is valid and prints as
/// `<Nop>`.
pub fn format_rhs(keys: &[Key]) -> String {
    if keys.is_empty() { NOP.to_string() } else { format_keys(keys) }
}

/// Every key sequence Normal mode resolves, with the name of what it
/// does, as `::bish map` prints it. Derived by asking
/// `vimkeys::describe_key_sequence` about every candidate, so it can't
/// disagree with the implementation (the written help never mentions
/// `D`).
///
/// Two kinds of row, which keep this an index rather than thousands of
/// lines:
///
/// - A sequence that resolves alone: `D`, `dd`, `gg`, `<C-w>s`.
/// - A pattern, where a key qualifies what follows: `d{motion}`,
///   `f{char}`, `"{char}`, `/{pattern}<CR>`, recognized mechanically: an
///   operator's description wraps the motion's in constant text, a
///   character argument's echoes the typed key, a search's quotes the
///   text before `<CR>`.
///
/// Counts are left out (`1`-`9` prefix nearly everything); `0` stays, as
/// a motion.
pub fn key_index() -> Vec<(String, String)> {
    let root = root_descriptions();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut frontier: Vec<Vec<Key>> = vec![Vec::new()];
    // Two levels of walking; patterns reach deeper through `pattern_rows`.
    for _ in 0..2 {
        let mut next = Vec::new();
        for prefix in &frontier {
            let (rows, expand) = walk(prefix, &root);
            out.extend(rows);
            next.extend(expand);
        }
        frontier = next;
    }
    crate::sort::sort(&mut out);
    out.dedup();
    out
}

// What a modifier wraps its operand's description in: ("delete ", "")
// for `d`, ("", " register 'a'") for `"a`.
#[derive(Clone, PartialEq)]
struct Affixes {
    before: String,
    after: String,
}

impl Affixes {
    fn render(&self) -> String {
        format!("{}{{motion}}{}", self.before, self.after)
    }
}

enum Shape {
    // Qualifies what follows, and says so in the name.
    Modifier(Affixes),
    // Qualifies what follows without changing its name: the unnamed
    // register `""`. Nothing new to list.
    Transparent,
    // Reads text up to `<CR>` and quotes it back, i.e. `/` and `?`.
    LineArgument(String),
    // An ordinary prefix: `g`, `z`, `<C-w>`. Worth walking into.
    Prefix,
}

// One level: every row `prefix` produces with one more key, plus the
// prefixes worth walking into.
fn walk(prefix: &[Key], root: &[(Key, String)]) -> (Vec<(String, String)>, Vec<Vec<Key>>) {
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut resolved: Vec<(Key, String)> = Vec::new();
    // Modifier rows, kept apart for the character-argument collapse: a
    // register is both (`"a` modifies, the `a` is typed), so
    // `"{char}{motion}` is one row instead of twenty-six.
    let mut patterned: Vec<(Key, String)> = Vec::new();
    let mut expand = Vec::new();
    for key in candidate_keys() {
        let seq = [prefix, &[key]].concat();
        match crate::bishedit::vimkeys::describe_key_sequence(&seq) {
            Ok(desc) => resolved.push((key, desc)),
            Err(reason) if reason == "incomplete" => match shape_of(&seq, root) {
                Shape::Modifier(affixes) => {
                    patterned.push((key, affixes.render()));
                    // A modifier reached by typing a character (a register)
                    // gets no extra probes: everything under `"a` is `"a`
                    // applied to something listed. An operator does,
                    // spelled `d` or `gu`.
                    if !echoes_last_key(&affixes.after, &seq) {
                        rows.extend(pattern_rows(&seq, root));
                    }
                }
                Shape::LineArgument(desc) => rows.push((format!("{}{{pattern}}<CR>", format_keys(&seq)), desc)),
                Shape::Transparent => {}
                Shape::Prefix => expand.push(seq),
            },
            Err(_) => {}
        }
    }
    rows.extend(collapse_char_arguments(prefix, resolved, ""));
    rows.extend(collapse_char_arguments(prefix, patterned, "{motion}"));
    crate::sort::sort(&mut rows);
    rows.dedup();
    (rows, expand)
}

// An operator isn't walked (`d{motion}` says it), but three things
// under one are their own facts: the doubled form (`dd`) and the two
// text-object introducers.
fn pattern_rows(seq: &[Key], root: &[(Key, String)]) -> Vec<(String, String)> {
    let Some(&last) = seq.last() else { return Vec::new() };
    let mut rows = Vec::new();
    for key in [last, Key::Char('i'), Key::Char('a')] {
        let longer = [seq, &[key]].concat();
        match crate::bishedit::vimkeys::describe_key_sequence(&longer) {
            Ok(desc) => rows.push((format_keys(&longer), desc)),
            Err(_) => match shape_of(&longer, root) {
                Shape::Modifier(affixes) => rows.push((format!("{}{{motion}}", format_keys(&longer)), affixes.render())),
                // A text object isn't named like a motion (`diw` is "delete
                // text-object inner word", `w` is "word-forward"), so the
                // operand test misses it; but it has siblings that all
                // begin alike, which is as mechanical.
                _ => rows.extend(family_row(&longer)),
            },
        }
    }
    rows
}

// One row for at least three sequences whose descriptions share all but
// a final word: `di{object}`.
fn family_row(seq: &[Key]) -> Option<(String, String)> {
    let described: Vec<String> =
        candidate_keys().into_iter().filter_map(|key| crate::bishedit::vimkeys::describe_key_sequence(&[seq, &[key]].concat()).ok()).collect();
    if described.len() < 3 {
        return None;
    }
    let first = described.first()?;
    let mut shared = first.len();
    for desc in &described[1..] {
        shared = shared.min(first.chars().zip(desc.chars()).take_while(|(a, b)| a == b).count());
    }
    let common = first[..first.char_indices().nth(shared).map_or(first.len(), |(i, _)| i)].to_string();
    let common = common.trim_end();
    // If the shared prefix is a sibling's whole description, these are names
    // that start alike, not one family.
    match common.is_empty() || described.iter().any(|d| d.trim() == common) {
        true => None,
        false => Some((format!("{}{{object}}", format_keys(seq)), format!("{common} {{object}}"))),
    }
}

// What kind of prefix `seq` is, by asking `describe_key_sequence` about
// real sequences, so it can't drift.
fn shape_of(seq: &[Key], root: &[(Key, String)]) -> Shape {
    // Does what follows keep its name, wrapped in constant text? `dw` is
    // "delete word" and `w` "word"; `"ap` is "put-after register 'a'".
    // Tallied rather than decided on the first agreement: a register
    // qualifies `p` and `Y` but not `J` or `u`, so the real shape is the
    // most common one that wraps something.
    let mut tried = 0;
    let mut tally: Vec<(Affixes, usize)> = Vec::new();
    for (key, base) in root {
        let Ok(desc) = crate::bishedit::vimkeys::describe_key_sequence(&[seq, &[*key]].concat()) else { continue };
        tried += 1;
        let Some(at) = desc.find(base.as_str()) else { continue };
        let affixes = Affixes { before: desc[..at].to_string(), after: desc[at + base.len()..].to_string() };
        match tally.iter_mut().find(|(a, _)| *a == affixes) {
            Some((_, n)) => *n += 1,
            None => tally.push((affixes, 1)),
        }
    }
    let wrapping = tally.iter().filter(|(a, _)| !(a.before.is_empty() && a.after.is_empty())).max_by_key(|(_, n)| *n);
    if let Some((affixes, count)) = wrapping
        && *count >= 3
        // An operator applies to everything after it, so the test is a large
        // majority: `d` matches 42 of 42. `<C-w>` matches 4 of 8 (`<C-w>h` is
        // "window focus-left" beside `h`'s "left") and isn't one. A register
        // never wins a majority but echoes the typed key, as `f{char}` does.
        && (count * 4 >= tried * 3 || echoes_last_key(&affixes.after, seq))
    {
        return Shape::Modifier(affixes.clone());
    }
    // What follows keeps its name exactly: the unnamed register `""`.
    if tally.iter().any(|(a, n)| a.before.is_empty() && a.after.is_empty() && *n >= 3) && wrapping.is_none() {
        return Shape::Transparent;
    }
    // Does it read text up to `<CR>` and quote it back? Asked with empty
    // text, which needs no guessing.
    if let Ok(desc) = crate::bishedit::vimkeys::describe_key_sequence(&[seq, &[Key::Enter]].concat())
        && let Some(stem) = desc.strip_suffix(" \"\"")
    {
        return Shape::LineArgument(format!("{stem} {{pattern}}"));
    }
    Shape::Prefix
}

// Whether a modifier's added text quotes the typed key: `"a` adds
// " register 'a'".
fn echoes_last_key(after: &str, seq: &[Key]) -> bool {
    matches!(seq.last(), Some(&last) if echoed_argument(after, last).is_some())
}

// Every single key that resolves alone, the operands for modifiers.
fn root_descriptions() -> Vec<(Key, String)> {
    candidate_keys().into_iter().filter_map(|key| crate::bishedit::vimkeys::describe_key_sequence(&[key]).ok().map(|desc| (key, desc))).collect()
}

// `f` reads a character and says so (`fa` is "find-char 'a'"), so a run
// whose descriptions echo their key becomes one row.
fn collapse_char_arguments(prefix: &[Key], resolved: Vec<(Key, String)>, suffix: &str) -> Vec<(String, String)> {
    let mut groups: Vec<(String, usize)> = Vec::new();
    for (key, desc) in &resolved {
        if let Some(stem) = echoed_argument(desc, *key) {
            match groups.iter_mut().find(|(s, _)| s == stem) {
                Some((_, n)) => *n += 1,
                None => groups.push((stem.to_string(), 1)),
            }
        }
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for (key, desc) in &resolved {
        let collapsed = echoed_argument(desc, *key).and_then(|stem| groups.iter().find(|(s, n)| s == stem && *n > 1));
        let row = match collapsed {
            Some((stem, _)) => (format!("{}{{char}}{suffix}", format_keys(prefix)), format!("{stem} {{char}}")),
            None => (format!("{}{suffix}", format_keys(&[prefix, &[*key]].concat())), desc.clone()),
        };
        if !out.contains(&row) {
            out.push(row);
        }
    }
    out
}

// The description without the echoed key, when it ends in the typed
// character. `'` and `\` arrive escaped (`find-char '\''`), so both
// spellings count.
fn echoed_argument(desc: &str, key: Key) -> Option<&str> {
    let Key::Char(c) = key else { return None };
    let escaped = match c {
        '\'' => Some("\\'"),
        '\\' => Some("\\\\"),
        _ => None,
    };
    desc.strip_suffix(&format!(" '{c}'")).or_else(|| escaped.and_then(|e| desc.strip_suffix(&format!(" '{e}'"))))
}

// Every printable ASCII character and named key, but `1`-`9` (see
// `key_index`).
fn candidate_keys() -> Vec<Key> {
    let mut keys: Vec<Key> =
        (0x20u8..0x7f).map(|b| Key::Char(b as char)).filter(|k| !matches!(k, Key::Char(c) if c.is_ascii_digit() && *c != '0')).collect();
    keys.extend(NAMED.iter().map(|(_, k)| *k));
    keys
}

/// Parses `<C-d>`, `<Space>w`, `10j` into keys. Names in angle brackets
/// match case-insensitively, as in vim, but print in the table's casing.
/// Anything else is a literal character, so `10j` is three keys. Errors
/// name the offending text.
pub fn parse_keys(text: &str) -> Result<Vec<Key>, String> {
    if text.eq_ignore_ascii_case(NOP) {
        // No keys: `::bish map -m insert '<Left>' '<Nop>'` takes a key out of
        // service. Only as the whole right-hand side; `a<Nop>b` is rejected
        // below as an unknown name.
        return Ok(Vec::new());
    }
    if text.is_empty() {
        return Err("empty key sequence".to_string());
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '<' {
            keys.push(Key::Char(chars[i]));
            i += 1;
            continue;
        }
        let Some(close) = chars[i..].iter().position(|c| *c == '>').map(|p| i + p) else {
            // An unmatched `<` is likely a typo in a name, so it's an error;
            // `<lt>` is the literal.
            return Err(format!("unterminated key name in '{text}' (a literal < is <lt>)"));
        };
        let name: String = chars[i..=close].iter().collect();
        i = close + 1;
        if name.eq_ignore_ascii_case(SPACE) {
            keys.push(Key::Char(' '));
            continue;
        }
        if name.eq_ignore_ascii_case(LT) {
            keys.push(Key::Char('<'));
            continue;
        }
        match NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            Some((_, key)) => keys.push(*key),
            None => return Err(format!("unknown key name '{name}'")),
        }
    }
    Ok(keys)
}

/// The modes a mapping can be scoped to. `--mode`/`-m` is a glob over
/// them, as `abbr --lang`: `-m '*al'` is normal and visual,
/// `-m '!(insert)'` all but insert.
///
/// No `terminal` mode: while a foreground job has the keyboard,
/// `drive_fg_job` forwards raw bytes and decodes nothing, so a program
/// like vim gets its escape sequences intact. Mapping there would need
/// an encoder kept in step with the decoder, and a global mapping would
/// apply inside every full-screen program (an Insert-mode `jk` would
/// break `j` in vim).
pub const MODES: &[&str] = &["normal", "insert", "visual", "command"];

/// Unscoped: every mode, as vim's `:noremap` is to `:nnoremap`.
pub const DEFAULT_MODE: &str = "*";

/// One `::bish map` entry. The right-hand side is stored as keys:
/// resolved at definition to validate it and get its description, then
/// replayed into the live `VimKeys` with remapping off, which is noremap.
/// Replaying keys lets a count compose with a mapped motion: with `<C-d>`
/// mapped to `j`, `2<C-d>` goes down two.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub modes: String,
    pub lhs: Vec<Key>,
    pub rhs: Vec<Key>,
}

impl Mapping {
    /// Whether this mapping is live in `mode`, by the shell's glob engine,
    /// so `--mode` takes what a `case` pattern does.
    pub fn applies_to(&self, mode: &str) -> bool {
        crate::glob::matches(&self.modes, mode)
    }
}

/// Whether a `--mode` glob names any mode: `-m nrmal` would otherwise be
/// accepted and never fire. Checked at definition.
pub fn mode_glob_is_known(glob: &str) -> bool {
    MODES.iter().any(|m| crate::glob::matches(glob, m))
}

/// The modes a `--mode` glob selects, in the order `MODES` lists them.
pub fn modes_matching(glob: &str) -> Vec<&'static str> {
    MODES.iter().copied().filter(|m| crate::glob::matches(glob, m)).collect()
}

/// Whether a glob covers a `VimKeys` mode, where a right-hand side
/// resolves to a named action. Normal and visual do; insert and command
/// dispatch raw keys.
pub fn has_vim_mode(glob: &str) -> bool {
    modes_matching(glob).iter().any(|m| *m == "normal" || *m == "visual")
}

// Takes `--mode=GLOB`, `--mode GLOB` or `-m GLOB` from `::bish map`'s
// arguments, a free function like `snippet::take_lang_flag`, so the add
// and list paths agree. Recognized anywhere before the key, not after,
// so a right-hand side can be `-m`.
pub fn take_mode_flag(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut mode = None;
    let mut rest: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if rest.is_empty() || rest.iter().all(|r| r.starts_with('-')) {
            if let Some(value) = arg.strip_prefix("--mode=") {
                mode = Some(value.to_string());
                i += 1;
                continue;
            }
            if (arg == "--mode" || arg == "-m") && i + 1 < args.len() {
                mode = Some(args[i + 1].clone());
                i += 2;
                continue;
            }
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (rest, mode)
}

// `::bish map`'s usage, shared by `help` and the missing-argument error.
pub fn usage() -> Vec<String> {
    vec![
        "::bish map [-m GLOB]                   what is mapped".to_string(),
        "::bish map [-m GLOB] KEYS ACTION-KEYS  remap KEYS".to_string(),
        "::bish map [-m GLOB] -e KEYS           remove a mapping".to_string(),
        String::new(),
        "Always non-recursive, as vim's `noremap` is: ACTION-KEYS mean what".to_string(),
        "they mean by default and never chain through another mapping.".to_string(),
        format!("-m is a glob over modes ({}), default '*'.", MODES.join(", ")),
        "Write a space as <Space>, a literal < as <lt>.".to_string(),
        "Quote KEYS in the shell: a bare < is a redirection.".to_string(),
    ]
}

/// Keys Normal mode's host loop answers before `VimKeys` sees them (see
/// `run_normal_mode_navigation`'s pre-match). `VimKeys::feed` returns
/// `None` for them, so validating against it alone would reject `:`, `K`,
/// `ZZ`, `q` and `@`. A table, since that loop has no seam to ask; the
/// test below catches an entry that moved into `VimKeys`, but a new host
/// arm must be added here.
const HOST_KEYS: &[(Key, &str)] = &[
    (Key::Char(':'), "command-mode"),
    (Key::Char('K'), "hover"),
    (Key::Char('Z'), "write-quit"),
    (Key::Char('q'), "record-macro"),
    (Key::Char('@'), "replay-macro"),
];

/// What the host loop calls this key, if it is one it handles.
pub fn describe_host_key(key: Key) -> Option<&'static str> {
    HOST_KEYS.iter().find(|(k, _)| *k == key).map(|(_, name)| *name)
}

/// Names what a right-hand side does across both layers. A sequence
/// starting with a host key is named by it, the rest shown as keys:
/// `:w<CR>` is `command-mode w<CR>`, since after `:` the rest is text for
/// the colon line.
pub fn describe_rhs(keys: &[Key]) -> Result<String, String> {
    if keys.is_empty() {
        return Ok("nothing".to_string());
    }
    if let Some(name) = keys.first().copied().and_then(describe_host_key) {
        let rest = format_keys(&keys[1..]);
        return Ok(if rest.is_empty() { name.to_string() } else { format!("{name} {rest}") });
    }
    crate::bishedit::vimkeys::describe_key_sequence(keys)
}

/// What a matcher decided about one key.
#[derive(Debug, PartialEq)]
pub struct Dispatch {
    /// Characters to take back off the screen before dispatching `keys`;
    /// 0 unless the caller asked to speculate.
    pub revert: usize,
    /// Keys to dispatch, in order; empty while undecided.
    pub keys: Vec<Key>,
}

impl Dispatch {
    fn keys(keys: Vec<Key>) -> Dispatch {
        Dispatch { revert: 0, keys }
    }
    fn none() -> Dispatch {
        Dispatch { revert: 0, keys: Vec::new() }
    }
}

/// Whether a key can be typed now and taken back: its effect must be one
/// character on screen. Letters and digits; not whitespace (Space and
/// Enter trigger `abbr`), nor non-characters.
fn can_speculate(key: Key) -> bool {
    matches!(key, Key::Char(c) if !c.is_whitespace())
}

/// Translates typed keys into their mappings, in front of `VimKeys`.
/// The input loop applies one outcome per key fed, so a mapping to a
/// sequence feeds several keys, with no change to `feed`. Output goes
/// straight to `VimKeys`, never back here, so mappings are
/// non-recursive by construction.
pub struct Matcher {
    mappings: Vec<Mapping>,
    /// Keys typed so far that are still a live prefix of some mapping.
    buffer: Vec<Key>,
    /// How many of `buffer`'s keys were dispatched speculatively (see
    /// `feed`): the characters to take back if it's a mapping after all.
    speculated: usize,
    /// The longest complete mapping matched so far and the keys it took,
    /// since a longer one may still match; if not, this fires. See `feed`
    /// for why that waits for the next key, not a timer.
    matched: Option<(Vec<Key>, usize)>,
}

impl Matcher {
    pub fn new(mappings: Vec<Mapping>) -> Matcher {
        Matcher { mappings, buffer: Vec::new(), speculated: 0, matched: None }
    }

    /// Whether anything is mapped, so a caller can skip the mechanism and
    /// an unmapped shell behaves as before.
    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    /// Keys held mid-sequence, for a caller flushing before it leaves the
    /// mode (see `flush`). Only tests ask so far.
    #[cfg(test)]
    pub fn pending(&self) -> &[Key] {
        &self.buffer
    }

    /// What to do with `key`. `speculative` means the caller can take a key
    /// back: in Insert mode a printable character is just one character on
    /// screen, so it's typed now and removed if the sequence becomes a
    /// mapping. So `jk` -> `<Esc>` needs no timer: `j` shows at once and goes
    /// when `k` completes it. Normal mode passes false: nothing echoes, and
    /// `d` or `x` can't be undone by deleting a character. The longest match
    /// wins, decided by the next key, never by time, so the result doesn't
    /// depend on typing speed.
    pub fn feed(&mut self, key: Key, mode: &str, speculative: bool) -> Dispatch {
        if self.mappings.is_empty() {
            return Dispatch::keys(vec![key]);
        }
        self.buffer.push(key);
        let live: Vec<&Mapping> = self.mappings.iter().filter(|m| m.applies_to(mode)).collect();

        if let Some(exact) = live.iter().find(|m| m.lhs == self.buffer) {
            self.matched = Some((exact.rhs.clone(), self.buffer.len()));
        }
        // A longer mapping could still match: hold.
        if live.iter().any(|m| m.lhs.len() > self.buffer.len() && m.lhs.starts_with(&self.buffer)) {
            if speculative && can_speculate(key) {
                // Typed now, takeable. Only printable non-whitespace: Space
                // and Enter trigger `abbr` (and Enter autoindents), which
                // one deletion can't undo; control keys don't echo, so
                // holding them shows nothing.
                self.speculated += 1;
                return Dispatch::keys(vec![key]);
            }
            return Dispatch::none();
        }
        match self.matched.take() {
            // The best match fires. Everything speculated is taken back
            // first, including keys past the match, which come back in
            // `leftover` and are dispatched again.
            Some((rhs, consumed)) => {
                let leftover: Vec<Key> = self.buffer.split_off(consumed);
                self.buffer.clear();
                let revert = std::mem::take(&mut self.speculated);
                Dispatch { revert, keys: rhs.into_iter().chain(leftover).collect() }
            }
            // A false start: nothing this could become is mapped, so each
            // key stands as itself. What was speculated is already on
            // screen and isn't sent twice.
            None => {
                let all = std::mem::take(&mut self.buffer);
                let already = std::mem::take(&mut self.speculated);
                Dispatch { revert: 0, keys: all[already.min(all.len())..].to_vec() }
            }
        }
    }

    /// Gives back what's held mid-sequence, so leaving the mode doesn't
    /// swallow it. Only tests use it so far.
    #[cfg(test)]
    pub fn flush(&mut self) -> Vec<Key> {
        self.matched = None;
        let already = std::mem::take(&mut self.speculated);
        let all: Vec<Key> = std::mem::take(&mut self.buffer);
        all[already.min(all.len())..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    // Asked, not written, so it can't miss a key the help forgets.
    #[test]
    fn the_key_index_answers_both_directions() {
        let index = super::key_index();
        let action = |keys: &str| index.iter().find(|(k, _)| k == keys).map(|(_, a)| a.as_str());

        assert_eq!(action("D"), Some("delete line-end"), "the miss that started this");
        assert_eq!(action("dd"), Some("delete line"));
        assert_eq!(action("gg"), Some("goto-first-line"));
        assert_eq!(action("<C-w>s"), Some("window split"));

        // The patterns, each recognized by a different property (see
        // `key_index`).
        assert_eq!(action("d{motion}"), Some("delete {motion}"));
        assert_eq!(action("di{object}"), Some("delete text-object inner {object}"));
        assert_eq!(action("f{char}"), Some("find-char {char}"));
        assert_eq!(action("\"{char}{motion}"), Some("{motion} register {char}"));
        assert_eq!(action("/{pattern}<CR>"), Some("search-forward {pattern}"));

        // And what the patterns replace: `dw`, `db`, `d$` aren't listed.
        assert_eq!(action("dw"), None);
        assert_eq!(action("fa"), None);
        assert_eq!(action("\"ayy"), None);

        // The reverse question: which keys delete.
        let deletes: Vec<&str> = index.iter().filter(|(_, a)| a.contains("delete")).map(|(k, _)| k.as_str()).collect();
        assert!(deletes.contains(&"D"), "{deletes:?}");

        // Small enough to read: the unfiltered walk finds about six thousand
        // sequences.
        assert!((100..250).contains(&index.len()), "{} rows", index.len());
    }

    // The key column is notation: anything without a `{placeholder}`
    // parses back and describes as its own row.
    #[test]
    fn every_literal_row_round_trips_through_parse_keys() {
        for (keys, action) in super::key_index() {
            if keys.contains('{') {
                continue;
            }
            let parsed = super::parse_keys(&keys).unwrap_or_else(|e| panic!("{keys}: {e}"));
            assert_eq!(super::format_keys(&parsed), keys, "spelling is not canonical");
            assert_eq!(crate::bishedit::vimkeys::describe_key_sequence(&parsed), Ok(action.clone()), "{keys}");
        }
    }

    use super::*;

    fn mapping(modes: &str, lhs: &str, rhs: &str) -> Mapping {
        Mapping { modes: modes.to_string(), lhs: parse_keys(lhs).unwrap(), rhs: parse_keys(rhs).unwrap() }
    }

    // Types `text` a key at a time and collects what was dispatched, as
    // notation.
    fn through(matcher: &mut Matcher, text: &str, mode: &str) -> String {
        let mut out = Vec::new();
        for key in parse_keys(text).unwrap() {
            out.extend(matcher.feed(key, mode, false).keys);
        }
        format_keys(&out)
    }

    // The same, speculating: what was dispatched and how many characters
    // were taken back, which is what Insert mode sees.
    fn speculating(matcher: &mut Matcher, text: &str) -> (String, usize) {
        let (mut out, mut reverted) = (Vec::new(), 0);
        for key in parse_keys(text).unwrap() {
            let d = matcher.feed(key, "insert", true);
            reverted += d.revert;
            out.extend(d.keys);
        }
        (format_keys(&out), reverted)
    }

    #[test]
    fn an_unmapped_key_passes_straight_through() {
        let mut m = Matcher::new(vec![mapping("*", "<C-d>", "10j")]);
        assert_eq!(through(&mut m, "jjk", "normal"), "jjk");
    }

    #[test]
    fn a_mapped_key_becomes_what_it_is_mapped_to() {
        let mut m = Matcher::new(vec![mapping("*", "<C-d>", "10j")]);
        assert_eq!(through(&mut m, "<C-d>", "normal"), "10j");
        assert_eq!(through(&mut m, "j<C-d>k", "normal"), "j10jk");
    }

    #[test]
    fn what_comes_out_is_never_mapped_again() {
        // Non-recursive: `x` -> `y` and `y` -> `z` give `y`, never `z`.
        let mut m = Matcher::new(vec![mapping("*", "x", "y"), mapping("*", "y", "z")]);
        assert_eq!(through(&mut m, "x", "normal"), "y");
        // And `y` typed directly is mapped, so the second mapping isn't just
        // ignored.
        assert_eq!(through(&mut m, "y", "normal"), "z");
    }

    #[test]
    fn a_multi_key_sequence_waits_and_then_fires() {
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        // Nothing is emitted while the sequence is undecided.
        assert!(m.feed(Key::Char(' '), "normal", false).keys.is_empty());
        assert_eq!(format_keys(&m.feed(Key::Char('w'), "normal", false).keys), "<C-w>s");
    }

    #[test]
    fn a_false_start_gives_every_key_back_rather_than_eating_them() {
        // `<Space>` is a motion in Normal mode, and mapping `<Space>w`
        // mustn't lose either key of `<Space>x`.
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        assert_eq!(through(&mut m, "<Space>x", "normal"), "<Space>x");
    }

    #[test]
    fn the_longest_mapping_wins_and_the_shorter_still_fires() {
        let mut m = Matcher::new(vec![mapping("*", "gh", "^"), mapping("*", "ghj", "$")]);
        // The longer one, when it arrives.
        assert_eq!(through(&mut m, "ghj", "normal"), "$");
        // The shorter one once the next key rules out the longer, and that
        // key isn't lost.
        assert_eq!(through(&mut m, "ghk", "normal"), "^k");
    }

    #[test]
    fn a_mapping_only_fires_in_the_modes_its_glob_names() {
        let mut m = Matcher::new(vec![mapping("normal", "x", "j")]);
        assert_eq!(through(&mut m, "x", "normal"), "j");
        assert_eq!(through(&mut m, "x", "visual"), "x");
    }

    #[test]
    fn leaving_mid_sequence_gives_the_held_keys_back() {
        // Held keys don't vanish when the mode ends.
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        assert!(m.feed(Key::Char(' '), "normal", false).keys.is_empty());
        assert_eq!(format_keys(m.pending()), "<Space>");
        assert_eq!(format_keys(&m.flush()), "<Space>");
        assert!(m.pending().is_empty());
    }

    #[test]
    fn a_speculated_prefix_is_typed_now_and_taken_back_if_it_completes() {
        // `j` shows the moment it's pressed and comes back off when `k` makes
        // it a mapping: no lag, no timer.
        let mut m = Matcher::new(vec![mapping("insert", "jk", "<Esc>")]);
        let d = m.feed(Key::Char('j'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("j".to_string(), 0), "j is typed immediately");
        let d = m.feed(Key::Char('k'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("<Esc>".to_string(), 1), "and taken back when the mapping fires");
    }

    #[test]
    fn a_speculated_key_that_leads_nowhere_is_not_sent_twice() {
        // `jl` is `jl`, not `jjl`: the `j` is already on screen.
        let mut m = Matcher::new(vec![mapping("insert", "jk", "<Esc>")]);
        assert_eq!(speculating(&mut m, "jl"), ("jl".to_string(), 0));
        assert_eq!(speculating(&mut m, "jam"), ("jam".to_string(), 0));
    }

    #[test]
    fn whitespace_is_never_speculated() {
        // Space and Enter trigger `abbr`, which a backspace can't undo, so
        // they hold, like non-character keys.
        let mut m = Matcher::new(vec![mapping("insert", "<Space>w", "<C-w>s")]);
        let d = m.feed(Key::Char(' '), "insert", true);
        assert!(d.keys.is_empty(), "held rather than typed");
        let d = m.feed(Key::Char('w'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("<C-w>s".to_string(), 0), "nothing to take back");
    }

    #[test]
    fn normal_mode_never_speculates() {
        // Nothing echoes in Normal mode, and `d` or `x` can't be undone by a
        // backspace.
        let mut m = Matcher::new(vec![mapping("normal", "jk", "<Esc>")]);
        assert!(m.feed(Key::Char('j'), "normal", false).keys.is_empty());
        let d = m.feed(Key::Char('k'), "normal", false);
        assert_eq!((format_keys(&d.keys), d.revert), ("<Esc>".to_string(), 0));
    }

    #[test]
    fn leaving_mid_sequence_does_not_re_send_what_is_already_on_screen() {
        let mut m = Matcher::new(vec![mapping("insert", "jk", "<Esc>")]);
        assert_eq!(format_keys(&m.feed(Key::Char('j'), "insert", true).keys), "j");
        assert!(m.flush().is_empty(), "the j was already dispatched");
    }

    #[test]
    fn an_empty_table_changes_nothing() {
        let mut m = Matcher::new(Vec::new());
        assert!(m.is_empty());
        assert_eq!(through(&mut m, "dw", "normal"), "dw");
    }

    #[test]
    fn a_host_key_is_a_valid_right_hand_side_even_though_vimkeys_never_sees_it() {
        // Host-loop keys resolve too, or `:`, `K`, `ZZ`, `q` and `@`
        // couldn't be mapped to.
        assert_eq!(describe_rhs(&parse_keys(":w<CR>").unwrap()), Ok("command-mode w<CR>".to_string()));
        assert_eq!(describe_rhs(&parse_keys("K").unwrap()), Ok("hover".to_string()));
        assert_eq!(describe_rhs(&parse_keys("@a").unwrap()), Ok("replay-macro a".to_string()));
        // An ordinary action resolves as usual.
        assert_eq!(describe_rhs(&parse_keys("10j").unwrap()), Ok("down 10".to_string()));
        assert_eq!(describe_rhs(&parse_keys("qqq").unwrap()), Ok("record-macro qq".to_string()));
    }

    #[test]
    fn every_host_key_is_one_vimkeys_really_does_not_know() {
        // If one of these moves into `VimKeys`, the table is wrong and this
        // catches it. A new host arm must still be added by hand.
        for (key, name) in HOST_KEYS {
            assert!(
                crate::bishedit::vimkeys::describe_key_sequence(&[*key]).is_err(),
                "{name}: VimKeys resolves this now, so it does not belong in HOST_KEYS"
            );
        }
    }

    #[test]
    fn nop_is_no_keys_at_all_and_prints_as_itself() {
        assert_eq!(parse_keys("<Nop>"), Ok(Vec::new()));
        assert_eq!(parse_keys("<nop>"), Ok(Vec::new()));
        assert_eq!(format_rhs(&[]), "<Nop>");
        assert_eq!(describe_rhs(&[]), Ok("nothing".to_string()));
        // Only the whole right-hand side.
        assert!(parse_keys("a<Nop>b").is_err());
        // An empty left-hand side is nothing to press.
        assert!(parse_keys("").is_err());
    }

    #[test]
    fn only_the_vim_driven_modes_resolve_keys_to_named_actions() {
        // Whether a right-hand side must resolve: `<Esc>` is a fine Insert
        // mapping and no Normal action.
        assert!(has_vim_mode("normal"));
        assert!(has_vim_mode("visual"));
        assert!(has_vim_mode(DEFAULT_MODE));
        assert!(!has_vim_mode("insert"));
        assert!(!has_vim_mode("command"));
        assert!(!has_vim_mode("@(insert|command)"));
    }

    #[test]
    fn a_mode_glob_selects_modes_the_way_abbr_selects_languages() {
        assert_eq!(modes_matching("normal"), vec!["normal"]);
        assert_eq!(modes_matching(DEFAULT_MODE), MODES.to_vec());
        // The same extglob `case` understands: the same engine.
        assert_eq!(modes_matching("!(insert)"), vec!["normal", "visual", "command"]);
        assert_eq!(modes_matching("*al"), vec!["normal", "visual"]);
    }

    #[test]
    fn a_glob_that_names_no_mode_is_a_typo_rather_than_an_empty_scope() {
        // A mapping that never fires would be accepted silently.
        assert!(mode_glob_is_known("normal"));
        assert!(mode_glob_is_known("*"));
        assert!(!mode_glob_is_known("nrmal"));
        assert!(!mode_glob_is_known(""));
    }

    #[test]
    fn a_mapping_is_live_in_the_modes_its_glob_names() {
        let m = Mapping { modes: "normal".to_string(), lhs: vec![Key::Char('x')], rhs: vec![Key::Char('j')] };
        assert!(m.applies_to("normal"));
        assert!(!m.applies_to("visual"));
        let global = Mapping { modes: DEFAULT_MODE.to_string(), ..m.clone() };
        assert!(global.applies_to("normal") && global.applies_to("visual"));
    }

    #[test]
    fn the_mode_flag_is_taken_in_any_of_its_spellings_and_only_before_the_key() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for spelling in [vec!["--mode=normal", "x", "j"], vec!["--mode", "normal", "x", "j"], vec!["-m", "normal", "x", "j"]] {
            let (rest, mode) = take_mode_flag(&args(&spelling));
            assert_eq!(mode.as_deref(), Some("normal"), "{spelling:?}");
            assert_eq!(rest, args(&["x", "j"]), "{spelling:?}");
        }
        // Not after the key, so a right-hand side can be `-m`.
        let (rest, mode) = take_mode_flag(&args(&["x", "-m", "normal"]));
        assert_eq!(mode, Option::None);
        assert_eq!(rest, args(&["x", "-m", "normal"]));
    }

    #[test]
    fn every_named_key_round_trips_through_its_own_spelling() {
        // Nothing prints in a spelling that won't parse back.
        for (name, key) in NAMED {
            assert_eq!(parse_keys(name), Ok(vec![*key]), "{name} did not parse");
            assert_eq!(format_keys(&[*key]), *name, "{name} did not format back");
        }
    }

    #[test]
    fn the_control_keys_bish_binds_to_nothing_are_still_mappable() {
        // These mean nothing elsewhere in bish, so people map them, `<C-s>`
        // for save most of all. bish's raw mode clears IXON, so flow
        // control doesn't eat `<C-s>`.
        for (spelling, key) in [("<C-q>", Key::CtrlQ), ("<C-s>", Key::CtrlS)] {
            assert_eq!(parse_keys(spelling), Ok(vec![key]), "{spelling}");
            assert_eq!(format_keys(&[key]), spelling);
            assert!(is_mappable(key));
        }
    }

    #[test]
    fn a_plain_character_is_a_key_and_needs_no_escaping() {
        assert_eq!(parse_keys("j"), Ok(vec![Key::Char('j')]));
        // A count is just keys.
        assert_eq!(parse_keys("10j"), Ok(vec![Key::Char('1'), Key::Char('0'), Key::Char('j')]));
        assert_eq!(format_keys(&parse_keys("10j").unwrap()), "10j");
    }

    #[test]
    fn a_sequence_mixes_names_and_characters() {
        assert_eq!(parse_keys("<Space>w"), Ok(vec![Key::Char(' '), Key::Char('w')]));
        assert_eq!(parse_keys("g<C-d>"), Ok(vec![Key::Char('g'), Key::CtrlD]));
        assert_eq!(format_keys(&parse_keys("<Space>w").unwrap()), "<Space>w");
    }

    #[test]
    fn a_space_and_a_less_than_have_spellings_rather_than_being_literal() {
        // A literal space couldn't be told from the separator; a literal `<`
        // would open a name.
        assert_eq!(format_keys(&[Key::Char(' ')]), "<Space>");
        assert_eq!(format_keys(&[Key::Char('<')]), "<lt>");
        assert_eq!(parse_keys("<lt>"), Ok(vec![Key::Char('<')]));
        assert_eq!(format_keys(&parse_keys("<lt>").unwrap()), "<lt>");
    }

    #[test]
    fn names_are_read_in_any_case_and_printed_in_one() {
        for spelling in ["<esc>", "<ESC>", "<EsC>"] {
            assert_eq!(parse_keys(spelling), Ok(vec![Key::Escape]), "{spelling}");
        }
        assert_eq!(format_keys(&parse_keys("<c-D>").unwrap()), "<C-d>", "one listing spelling, however it was typed");
    }

    #[test]
    fn a_bad_name_says_what_was_wrong_with_it() {
        assert_eq!(parse_keys("<Nope>"), Err("unknown key name '<Nope>'".to_string()));
        assert!(parse_keys("<C-d").unwrap_err().contains("unterminated"));
        // An unterminated `<` is usually a typo, so the error names `<lt>`.
        assert!(parse_keys("a<b").unwrap_err().contains("<lt>"));
        assert!(parse_keys("").is_err());
    }

    #[test]
    fn the_keys_that_cannot_carry_a_mapping_are_the_ones_nobody_presses() {
        use crate::editor::MouseEvent;
        assert!(!is_mappable(Key::Mouse(MouseEvent { button: 0, col: 1, row: 1, pressed: true })));
        assert!(!is_mappable(Key::Unknown));
        assert!(!is_mappable(Key::PasteStart));
        assert!(!is_mappable(Key::PasteEnd));
        assert!(is_mappable(Key::Char('j')));
        assert!(is_mappable(Key::CtrlD));
        // Everything the table names is mappable.
        for (name, key) in NAMED {
            assert!(is_mappable(*key), "{name}");
        }
    }
}
