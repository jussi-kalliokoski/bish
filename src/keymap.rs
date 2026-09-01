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

#[cfg(test)]
mod tests {
    use super::*;

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
