// Key notation: the `<C-d>`/`<Esc>`/`<Space>` spelling `::bish map`
// reads and prints, and the parser/formatter pair that turns it into
// `editor::Key` values and back.
//
// Both directions come off ONE table (`NAMED`), so a key that can be
// typed can always be printed and vice versa -- a formatter and a
// parser maintained separately drift, and the first symptom is a
// mapping that lists in a spelling it will not accept back.
//
// This is deliberately *not* `vimkeys::key_label`, which already exists
// and looks similar. That one renders the pending-input hint in the
// status line, where `^D` and a real arrow glyph are what a person wants
// to see mid-keystroke; it is display, it is lossy (`None` for anything
// it has no glyph for), and it never has to be read back. This one is a
// wire format, has to round-trip exactly, and covers every key that can
// carry a mapping. They are different jobs and share no code on purpose.
//
// Vim's own spelling is followed wherever vim has one, because the
// population who will type this already knows it: `<C-d>`, `<Esc>`,
// `<CR>`, `<BS>`, `<Tab>`, `<S-Tab>`, `<Space>`, `<Del>`, `<Home>`,
// `<End>`, `<PageUp>`, `<PageDown>`, the arrows, and `<lt>` for a
// literal `<`.
//
// Lands ahead of its consumers -- `::bish map` reads and prints through
// this, and vimkeys.rs matches against it -- the same "build the seam,
// wire it in later" pattern history.rs's own `cwd` field and
// lexer.rs's SpannedItem already follow here. The tests below are the
// real users until then, and they cover both directions.
#![allow(dead_code)]

use crate::editor::Key;

// Every key that is not `Key::Char`, with its canonical spelling.
// Ordered roughly as the `Key` enum is, and exhaustive over the
// mappable variants -- see `is_mappable` for the ones deliberately
// missing and why.
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

// A space is spelled `<Space>` rather than written literally, because a
// mapping's two halves are separated by whitespace on the command line
// and a bare space would make `::bish map` ambiguous about where the
// left-hand side ends.
const SPACE: &str = "<Space>";
// `<` itself, so a mapping can contain one without opening a name.
const LT: &str = "<lt>";
// Vim's own spelling for "do nothing". Only meaningful as a whole
// right-hand side -- it is the *absence* of keys, so there is nothing
// for it to sit beside in a sequence.
const NOP: &str = "<Nop>";

// Whether a key can carry a mapping at all.
//
// `Mouse` and `Unknown` cannot: one is a coordinate-carrying event
// rather than a keystroke (and every view that wants it already handles
// it directly), and the other is by definition the bytes nothing
// recognized, so binding it would bind "whatever we failed to decode".
// `PasteStart`/`PasteEnd` cannot either: they are the brackets a
// terminal puts around pasted text, not something a person presses, and
// a mapping on one would fire on every paste.
pub fn is_mappable(key: Key) -> bool {
    !matches!(key, Key::Mouse(_) | Key::Unknown | Key::PasteStart | Key::PasteEnd)
}

// One key's canonical spelling. Total over mappable keys, which is what
// lets `format_keys` be infallible.
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
    // Only reachable for a non-mappable key, which no stored mapping can
    // contain -- rendered rather than panicking, since a listing is not
    // somewhere to take the shell down.
    "<unknown>".to_string()
}

/// The canonical spelling of a whole sequence, exactly as `::bish map`
/// prints it and will read it back.
pub fn format_keys(keys: &[Key]) -> String {
    keys.iter().map(|k| format_key(*k)).collect()
}

/// The same, for a right-hand side, where "no keys" is a real answer
/// and prints as what would produce it.
pub fn format_rhs(keys: &[Key]) -> String {
    if keys.is_empty() { NOP.to_string() } else { format_keys(keys) }
}

/// Parses `<C-d>`, `<Space>w`, `10j` and so on into the keys they name.
///
/// Angle-bracket names are matched case-insensitively (`<esc>`, `<ESC>`
/// and `<Esc>` are one key, as in vim) but always *print* in the table's
/// own casing, so a listing has one spelling regardless of how it was
/// typed. Anything else is a literal character, which is what makes
/// `10j` three ordinary keys rather than needing an escape.
///
/// The error is the message the user sees, so it names the offending
/// text rather than an offset.
pub fn parse_keys(text: &str) -> Result<Vec<Key>, String> {
    if text.eq_ignore_ascii_case(NOP) {
        // No keys at all: `::bish map -m insert '<Left>' '<Nop>'` is how
        // a key is taken out of service. Only the whole right-hand side,
        // never part of a sequence -- `a<Nop>b` would be `ab` written
        // confusingly, so it is rejected below as an unknown name.
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
            // An unmatched `<` is far more likely a typo in a name than
            // a deliberate literal, so it is an error rather than being
            // silently taken as `Key::Char('<')` -- `<lt>` is there for
            // when a literal is meant.
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
/// these, the same shape and the same engine `abbr --lang` uses, so one
/// mapping can cover a family (`-m '*al'` is normal and visual) or
/// everything but one (`-m '!(insert)'`).
///
/// There is deliberately no `terminal`. While a foreground job owns the
/// keyboard, `drive_fg_job` forwards raw bytes to it and never decodes
/// keys at all -- which is exactly what keeps a real vim running under
/// bish receiving its own escape sequences intact. Mapping there would
/// need a second, parallel key-to-bytes encoder kept in step with the
/// decoder, on the most fidelity-sensitive path in the codebase; and
/// because the default scope is `*`, an ordinary global mapping would
/// then silently apply *inside* every full-screen program, so mapping
/// `jk` for Insert mode would break `j` in vim. The keyboard belongs to
/// the program that has it.
pub const MODES: &[&str] = &["normal", "insert", "visual", "command"];

/// Unscoped: every mode. A mapping with no `--mode` is global, which is
/// what vim's own `:noremap` (as against `:nnoremap`) means.
pub const DEFAULT_MODE: &str = "*";

/// One `::bish map` entry.
///
/// The right-hand side is stored as *keys*, not as the action it
/// resolves to. Resolution still happens when the mapping is defined --
/// that is what validates it and produces the canonical description a
/// listing shows -- but dispatch replays these keys into the live
/// `VimKeys` with remapping suppressed. That is still exactly noremap,
/// since a replayed key cannot trigger another mapping, and it is what
/// lets a typed count compose with a mapped motion: `2<C-d>` where
/// `<C-d>` is mapped to `j` means two lines down, which a frozen
/// `Motion(Down, None)` could not express.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub modes: String,
    pub lhs: Vec<Key>,
    pub rhs: Vec<Key>,
}

impl Mapping {
    /// Whether this mapping is live in `mode`. The shell's own glob
    /// engine, so `--mode` accepts exactly what a `case` pattern does,
    /// with no second dialect to learn.
    pub fn applies_to(&self, mode: &str) -> bool {
        crate::glob::matches(&self.modes, mode)
    }
}

/// Whether a `--mode` glob names anything at all.
///
/// A glob matching no known mode is always a mistake -- `-m nrmal`
/// would otherwise be accepted, listed, and never fire, with nothing
/// anywhere saying why. Checked once when the mapping is defined rather
/// than being discovered at a keystroke that does not happen.
pub fn mode_glob_is_known(glob: &str) -> bool {
    MODES.iter().any(|m| crate::glob::matches(glob, m))
}

/// The modes a `--mode` glob selects, in the order `MODES` lists them.
pub fn modes_matching(glob: &str) -> Vec<&'static str> {
    MODES.iter().copied().filter(|m| crate::glob::matches(glob, m)).collect()
}

/// Whether a glob covers a mode driven by `VimKeys`, and so one where a
/// right-hand side resolves to a *named action* rather than just to
/// keys. Normal and visual share that machine; insert and command
/// dispatch on raw keys and have no such vocabulary, which is why both
/// validation and the listing have to ask.
pub fn has_vim_mode(glob: &str) -> bool {
    modes_matching(glob).iter().any(|m| *m == "normal" || *m == "visual")
}

// Pulls `--mode=GLOB`/`--mode GLOB`/`-m GLOB` out of `::bish map`'s
// arguments. A free function rather than a method for the same reason
// `snippet::take_lang_flag` is one: it needs no shell state, and both
// the add and the list paths have to agree about the spelling.
//
// Recognized anywhere before the key, since the flag and the mode
// flags are order-independent in `abbr` too -- but not after it, so a
// mapping whose right-hand side is literally `-m` is still possible.
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

// `::bish map`'s own usage, shared by `help` and by the error a missing
// argument prints -- one text, so the two cannot disagree.
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

/// Keys the Normal-mode *host loop* answers itself, before `VimKeys`
/// ever sees them -- see `run_normal_mode_navigation`'s own pre-match on
/// `key`, above its `match vk.feed(key)`.
///
/// `VimKeys::feed` returns `None` for every one of these, so validating
/// a right-hand side against that alone rejected `:`, `K`, `ZZ`, `q` and
/// `@` -- which is to say every way of reaching command mode, hover,
/// write-and-quit or a macro from a mapping. Normal mode's key handling
/// is split across two layers and only one of them was being asked.
///
/// A table rather than a query, because that loop is a several-hundred
/// line match with no seam to ask. The test below checks every entry is
/// still unknown to `VimKeys`, which catches an entry that has since
/// moved into the key machine; a *new* host arm has to be added here by
/// hand.
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

/// Names what a right-hand side does, across both layers of Normal
/// mode.
///
/// A sequence *starting* with a host key is named by it, with whatever
/// follows shown as keys: `:w<CR>` is `command-mode w<CR>`. The tail
/// cannot be resolved further and should not pretend to be -- once `:`
/// has opened the colon line, the rest is text for a different reader
/// entirely.
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
    /// Characters to take back off the screen before dispatching
    /// `keys` -- always 0 unless the caller asked to speculate.
    pub revert: usize,
    /// Keys to dispatch, in order. Empty while a sequence is still
    /// being decided.
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

/// Whether a key can be typed now and taken back later: its whole
/// effect has to be one character on screen. A letter or digit
/// qualifies; whitespace does not (Space and Enter trigger `abbr`
/// expansion, which one backspace would not undo), and nor does
/// anything that is not a character at all.
fn can_speculate(key: Key) -> bool {
    matches!(key, Key::Char(c) if !c.is_whitespace())
}

/// Translates typed keys into the keys they are mapped to, in front of
/// `VimKeys` rather than inside it.
///
/// Sitting in front is what makes this work. The input loop already
/// applies exactly one outcome per key it feeds `VimKeys`, so a mapping
/// that expands to a sequence just feeds several keys and the loop
/// applies each outcome -- no change to `feed`'s contract, and none of
/// the "one key in, several actions out" problem that putting the
/// keymap inside `VimKeys` would have created.
///
/// It is also what makes the mapping non-recursive for free: what comes
/// out goes straight to `VimKeys` and never back through here, so a
/// right-hand side cannot trigger another mapping however the table is
/// arranged. There is no flag to get wrong.
pub struct Matcher {
    mappings: Vec<Mapping>,
    /// Keys typed so far that are still a live prefix of some mapping.
    buffer: Vec<Key>,
    /// How many of `buffer`'s keys have already been dispatched on the
    /// chance that this is not a mapping after all -- see `feed`'s
    /// `speculative`. Exactly the number of characters to take back if
    /// it turns out to be one.
    speculated: usize,
    /// The longest complete mapping the buffer has matched so far, and
    /// how many keys it consumed -- remembered because a longer mapping
    /// may still match, and if it does not, this is what fires. See
    /// `feed` for why the wait is until the next key rather than a
    /// timer.
    matched: Option<(Vec<Key>, usize)>,
}

impl Matcher {
    pub fn new(mappings: Vec<Mapping>) -> Matcher {
        Matcher { mappings, buffer: Vec::new(), speculated: 0, matched: None }
    }

    /// Whether anything is mapped at all -- lets a caller skip the whole
    /// mechanism, and keeps a shell with no mappings behaving exactly as
    /// it did before there were any.
    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    /// Keys held mid-sequence, for a caller that needs to flush before
    /// leaving the mode (see `flush`).
    pub fn pending(&self) -> &[Key] {
        &self.buffer
    }

    /// What to do with `key`.
    ///
    /// `speculative` says the caller can take a key back: in Insert mode
    /// a printable character's whole effect is one character on screen,
    /// so it can be typed now and removed if the sequence turns out to
    /// be a mapping. That is what makes `jk` -> `<Esc>` feel right
    /// without a timer -- `j` appears the instant it is pressed and
    /// disappears when `k` completes the mapping. Normal mode passes
    /// false: nothing echoes there so there is no lag to remove, and
    /// `d` or `x` does real work that "delete one character" could not
    /// undo.
    ///
    /// Longest match wins either way, decided by the next key rather
    /// than by a clock. Nothing here ever fires because time passed, so
    /// the same keys always produce the same result however fast they
    /// were typed.
    pub fn feed(&mut self, key: Key, mode: &str, speculative: bool) -> Dispatch {
        if self.mappings.is_empty() {
            return Dispatch::keys(vec![key]);
        }
        self.buffer.push(key);
        let live: Vec<&Mapping> = self.mappings.iter().filter(|m| m.applies_to(mode)).collect();

        if let Some(exact) = live.iter().find(|m| m.lhs == self.buffer) {
            self.matched = Some((exact.rhs.clone(), self.buffer.len()));
        }
        // A longer mapping could still match, so hold and see.
        if live.iter().any(|m| m.lhs.len() > self.buffer.len() && m.lhs.starts_with(&self.buffer)) {
            if speculative && can_speculate(key) {
                // Typed now, remembered as takeable. Only printable
                // non-whitespace: Space and Enter trigger `abbr`
                // expansion (and Enter autoindents), and removing one
                // character does not undo an expansion. A control key
                // does not echo, so holding it costs nothing visible.
                self.speculated += 1;
                return Dispatch::keys(vec![key]);
            }
            return Dispatch::none();
        }
        match self.matched.take() {
            // The best match fires. Everything speculated is taken back
            // first -- including any that fell *outside* the match,
            // since those keys reappear in `leftover` and are
            // dispatched again.
            Some((rhs, consumed)) => {
                let leftover: Vec<Key> = self.buffer.split_off(consumed);
                self.buffer.clear();
                let revert = std::mem::take(&mut self.speculated);
                Dispatch { revert, keys: rhs.into_iter().chain(leftover).collect() }
            }
            // A false start: nothing this buffer could still become is
            // mapped, so every key of it stands as itself. Whatever was
            // speculated is already on screen and must not be sent
            // twice.
            None => {
                let all: Vec<Key> = self.buffer.drain(..).collect();
                let already = std::mem::take(&mut self.speculated);
                Dispatch { revert: 0, keys: all[already.min(all.len())..].to_vec() }
            }
        }
    }

    /// Gives back whatever is being held mid-sequence, so a caller
    /// leaving the mode does not silently swallow it.
    pub fn flush(&mut self) -> Vec<Key> {
        self.matched = None;
        let already = std::mem::take(&mut self.speculated);
        let all: Vec<Key> = std::mem::take(&mut self.buffer);
        all[already.min(all.len())..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(modes: &str, lhs: &str, rhs: &str) -> Mapping {
        Mapping { modes: modes.to_string(), lhs: parse_keys(lhs).unwrap(), rhs: parse_keys(rhs).unwrap() }
    }

    // Types `text` a key at a time and collects everything dispatched,
    // rendered back as notation so the assertion reads like what was
    // typed.
    fn through(matcher: &mut Matcher, text: &str, mode: &str) -> String {
        let mut out = Vec::new();
        for key in parse_keys(text).unwrap() {
            out.extend(matcher.feed(key, mode, false).keys);
        }
        format_keys(&out)
    }

    // The same, speculating -- returning what was dispatched and how
    // many characters had to be taken back, which together are the
    // whole of what Insert mode sees.
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
        // Non-recursive with no flag to get wrong: the matcher's output
        // goes to VimKeys, not back through here. `x` -> `y` and
        // `y` -> `z` must give `y`, never `z`.
        let mut m = Matcher::new(vec![mapping("*", "x", "y"), mapping("*", "y", "z")]);
        assert_eq!(through(&mut m, "x", "normal"), "y");
        // ...and `y` typed directly is still mapped, so this is not just
        // the second mapping being ignored.
        assert_eq!(through(&mut m, "y", "normal"), "z");
    }

    #[test]
    fn a_multi_key_sequence_waits_and_then_fires() {
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        // Nothing is emitted while the sequence is still being decided.
        assert!(m.feed(Key::Char(' '), "normal", false).keys.is_empty());
        assert_eq!(format_keys(&m.feed(Key::Char('w'), "normal", false).keys), "<C-w>s");
    }

    #[test]
    fn a_false_start_gives_every_key_back_rather_than_eating_them() {
        // The bug this shape exists to avoid: `<Space>` is a real motion
        // in normal mode, and mapping `<Space>w` must not make
        // `<Space>x` lose either key.
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        assert_eq!(through(&mut m, "<Space>x", "normal"), "<Space>x");
    }

    #[test]
    fn the_longest_mapping_wins_and_the_shorter_still_fires() {
        let mut m = Matcher::new(vec![mapping("*", "gh", "^"), mapping("*", "ghj", "$")]);
        // The longer one, when it arrives.
        assert_eq!(through(&mut m, "ghj", "normal"), "$");
        // The shorter one, once the next key rules the longer one out --
        // and the key that ruled it out is not lost.
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
        // Held keys must not vanish because the mode ended.
        let mut m = Matcher::new(vec![mapping("*", "<Space>w", "<C-w>s")]);
        assert!(m.feed(Key::Char(' '), "normal", false).keys.is_empty());
        assert_eq!(format_keys(m.pending()), "<Space>");
        assert_eq!(format_keys(&m.flush()), "<Space>");
        assert!(m.pending().is_empty());
    }

    #[test]
    fn a_speculated_prefix_is_typed_now_and_taken_back_if_it_completes() {
        // The whole point: `j` reaches the screen the instant it is
        // pressed, so there is no lag and no timer -- and comes back off
        // when `k` turns it into a mapping.
        let mut m = Matcher::new(vec![mapping("insert", "jk", "<Esc>")]);
        let d = m.feed(Key::Char('j'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("j".to_string(), 0), "j is typed immediately");
        let d = m.feed(Key::Char('k'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("<Esc>".to_string(), 1), "and taken back when the mapping fires");
    }

    #[test]
    fn a_speculated_key_that_leads_nowhere_is_not_sent_twice() {
        // `jl` must be `jl`, not `jjl`: the `j` is already on screen.
        let mut m = Matcher::new(vec![mapping("insert", "jk", "<Esc>")]);
        assert_eq!(speculating(&mut m, "jl"), ("jl".to_string(), 0));
        assert_eq!(speculating(&mut m, "jam"), ("jam".to_string(), 0));
    }

    #[test]
    fn whitespace_is_never_speculated() {
        // Space and Enter trigger `abbr` expansion, and one backspace
        // does not undo an expansion -- so they hold, like every
        // non-character key, where holding costs nothing visible.
        let mut m = Matcher::new(vec![mapping("insert", "<Space>w", "<C-w>s")]);
        let d = m.feed(Key::Char(' '), "insert", true);
        assert!(d.keys.is_empty(), "held rather than typed");
        let d = m.feed(Key::Char('w'), "insert", true);
        assert_eq!((format_keys(&d.keys), d.revert), ("<C-w>s".to_string(), 0), "nothing to take back");
    }

    #[test]
    fn normal_mode_never_speculates() {
        // Nothing echoes there, so there is no lag to remove -- and `d`
        // or `x` does real work that one backspace could not undo.
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
        // Normal mode is handled across two layers and only one of them
        // resolves keys to actions, so validating against `VimKeys`
        // alone rejected every way of reaching command mode, hover,
        // write-and-quit or a macro from a mapping.
        assert_eq!(describe_rhs(&parse_keys(":w<CR>").unwrap()), Ok("command-mode w<CR>".to_string()));
        assert_eq!(describe_rhs(&parse_keys("K").unwrap()), Ok("hover".to_string()));
        assert_eq!(describe_rhs(&parse_keys("@a").unwrap()), Ok("replay-macro a".to_string()));
        // ...and an ordinary action still resolves the ordinary way.
        assert_eq!(describe_rhs(&parse_keys("10j").unwrap()), Ok("down 10".to_string()));
        assert_eq!(describe_rhs(&parse_keys("qqq").unwrap()), Ok("record-macro qq".to_string()));
    }

    #[test]
    fn every_host_key_is_one_vimkeys_really_does_not_know() {
        // Half of the drift guard: if one of these moves into the key
        // machine, the table here is wrong and this catches it. A *new*
        // host arm still has to be added by hand.
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
        // Only ever the whole right-hand side: inside a sequence it
        // would be `ab` written confusingly.
        assert!(parse_keys("a<Nop>b").is_err());
        // An empty left-hand side is still nothing to press.
        assert!(parse_keys("").is_err());
    }

    #[test]
    fn only_the_vim_driven_modes_resolve_keys_to_named_actions() {
        // What the fourth column of a listing means, and whether a
        // right-hand side has to resolve at all. `<Esc>` is a fine
        // insert-mode mapping and no normal-mode action whatsoever.
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
        // The same extglob the shell's own `case` understands, because
        // it is the same engine -- no second pattern dialect.
        assert_eq!(modes_matching("!(insert)"), vec!["normal", "visual", "command"]);
        assert_eq!(modes_matching("*al"), vec!["normal", "visual"]);
    }

    #[test]
    fn a_glob_that_names_no_mode_is_a_typo_rather_than_an_empty_scope() {
        // Accepting it would store a mapping that lists and never fires,
        // with nothing anywhere saying why.
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
        // Not after the key, so a mapping whose right-hand side is
        // literally `-m` stays possible.
        let (rest, mode) = take_mode_flag(&args(&["x", "-m", "normal"]));
        assert_eq!(mode, Option::None);
        assert_eq!(rest, args(&["x", "-m", "normal"]));
    }

    #[test]
    fn every_named_key_round_trips_through_its_own_spelling() {
        // The property the one-table design exists to guarantee: nothing
        // can be printed in a spelling that will not parse back.
        for (name, key) in NAMED {
            assert_eq!(parse_keys(name), Ok(vec![*key]), "{name} did not parse");
            assert_eq!(format_keys(&[*key]), *name, "{name} did not format back");
        }
    }

    #[test]
    fn the_control_keys_bish_binds_to_nothing_are_still_mappable() {
        // These four have no meaning of their own anywhere in bish,
        // which is exactly why someone would map them -- `<C-s>` for
        // "save" most of all. They used not to decode at all, so the
        // mapping was refused as an unknown name and the diagnosis
        // ("flow control eats <C-s>") was wrong twice over: bish's raw
        // mode already clears IXON.
        for (spelling, key) in
            [("<C-g>", Key::CtrlG), ("<C-q>", Key::CtrlQ), ("<C-s>", Key::CtrlS), ("<C-t>", Key::CtrlT)]
        {
            assert_eq!(parse_keys(spelling), Ok(vec![key]), "{spelling}");
            assert_eq!(format_keys(&[key]), spelling);
            assert!(is_mappable(key));
        }
    }

    #[test]
    fn a_plain_character_is_a_key_and_needs_no_escaping() {
        assert_eq!(parse_keys("j"), Ok(vec![Key::Char('j')]));
        // The reason a count needs no special case: it is just keys.
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
        // A literal space would make `::bish map` unable to tell where
        // the left-hand side ends; a literal `<` would open a name.
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
        // Pointing at `<lt>` matters: an unterminated `<` is a typo far
        // more often than a wanted literal, so the error has to say how
        // to ask for the literal.
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
        // Everything the table names is mappable, or it would be listing
        // a spelling for something that can never be bound.
        for (name, key) in NAMED {
            assert!(is_mappable(*key), "{name}");
        }
    }
}
