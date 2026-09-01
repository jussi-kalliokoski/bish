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
    ("<C-k>", Key::CtrlK),
    ("<C-l>", Key::CtrlL),
    ("<C-n>", Key::CtrlN),
    ("<C-o>", Key::CtrlO),
    ("<C-p>", Key::CtrlP),
    ("<C-r>", Key::CtrlR),
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
pub const MODES: &[&str] = &["normal", "insert", "visual", "command", "terminal"];

/// Unscoped: every mode. A mapping with no `--mode` is global, which is
/// what vim's own `:noremap` (as against `:nnoremap`) means.
pub const DEFAULT_MODE: &str = "*";

/// The modes that actually consult the keymap today.
///
/// `normal` and `visual` share one `VimKeys`, which resolves a key
/// sequence to a named action -- so a mapping there has something to be
/// described *as*. The other three dispatch on raw `Key` arms with no
/// enumerated action space, so they are accepted by the glob and
/// reported as not yet remappable rather than being silently stored and
/// never fired, which is the worse of the two failures: a mapping that
/// lists but does nothing gives no hint that anything is missing.
pub const REMAPPABLE: &[&str] = &["normal", "visual"];

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

/// Whether a `--mode` glob selects nothing that can act on a mapping --
/// i.e. this mapping can never fire as things stand.
///
/// Deliberately not "does it select *any* mode that cannot act on one":
/// the default glob is `*`, which selects all five, so that reading
/// would print a warning for every unscoped mapping in every config
/// file. A global mapping working in normal and visual is exactly what
/// was asked for. The warning is worth making only when the answer is
/// "this will never fire at all", which is a typo or a misunderstanding
/// rather than a partial success.
pub fn never_fires(glob: &str) -> bool {
    !modes_matching(glob).iter().any(|m| REMAPPABLE.contains(m))
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
        format!("Only {} act on mappings so far.", REMAPPABLE.join(" and ")),
        "Write a space as <Space>, a literal < as <lt>.".to_string(),
        "Quote KEYS in the shell: a bare < is a redirection.".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_glob_selects_modes_the_way_abbr_selects_languages() {
        assert_eq!(modes_matching("normal"), vec!["normal"]);
        assert_eq!(modes_matching(DEFAULT_MODE), MODES.to_vec());
        // The same extglob the shell's own `case` understands, because
        // it is the same engine -- no second pattern dialect.
        assert_eq!(modes_matching("!(insert)"), vec!["normal", "visual", "command", "terminal"]);
        assert_eq!(modes_matching("*al"), vec!["normal", "visual", "terminal"]);
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
    fn only_a_mapping_that_can_never_fire_is_worth_warning_about() {
        // The default glob selects all five, three of which cannot act
        // on a mapping yet -- warning on that would fire for every
        // unscoped mapping in every config file, which is noise, not a
        // warning. Working in normal and visual is what was asked for.
        assert!(!never_fires(DEFAULT_MODE));
        assert!(!never_fires("normal"));
        assert!(!never_fires("*al"));
        // These genuinely cannot fire, which is worth saying.
        assert!(never_fires("insert"));
        assert!(never_fires("@(command|terminal)"));
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
