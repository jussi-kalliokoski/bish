// Hand-rolled Unicode display-width table -- no external crate, same
// spirit as glob.rs/regex.rs/csscolor.rs. How many terminal columns one
// char actually occupies: 0 for a combining mark/zero-width joiner/
// variation selector, 2 for an East-Asian-Wide/Fullwidth character (CJK
// ideographs, Hangul syllables, most emoji, fullwidth forms), 1 for
// everything else. This is deliberately the well-known "per-codepoint
// width" model every terminal emulator and most terminal apps (tmux,
// less, vim's own default) already use, not full Unicode grapheme-
// cluster segmentation -- a multi-codepoint sequence (a ZWJ emoji
// family, a base character plus several combining marks) is measured
// char-by-char and summed here rather than treated as one cluster.
// That's a real, harder problem (needs a second table just for
// grapheme-break properties) genuinely deferred to a later pass if it
// ever turns out to matter in practice; this alone already fixes the
// much more common case (a single wide/combining char throwing off
// cursor math by one column).
//
// The range tables below are a deliberately trimmed, high-confidence
// subset of the full Unicode East Asian Width / combining-mark
// properties -- covering the ranges that actually come up in normal
// use (CJK ideographs and syllables, fullwidth forms, the common
// combining-diacritic and emoji-variation-selector blocks) rather than
// attempting a byte-perfect reproduction of the *complete* property
// tables (several hundred narrow ranges, many for scripts this project
// has no other support for either) from memory. A gap here degrades to
// "one column too many/few for an obscure script," not a crash or a
// stuck cursor -- worth widening if a real, specific case ever shows
// up, not attempted speculatively.

fn in_ranges(c: u32, ranges: &[(u32, u32)]) -> bool {
    ranges.binary_search_by(|&(lo, hi)| if c < lo { std::cmp::Ordering::Greater } else if c > hi { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }).is_ok()
}

// Combining marks, zero-width joiners/spaces, variation selectors --
// sorted, non-overlapping, binary-searched.
const ZERO_WIDTH: &[(u32, u32)] = &[
    (0x0300, 0x036F),   // Combining Diacritical Marks
    (0x0483, 0x0489),   // Cyrillic combining marks
    (0x200B, 0x200F),   // zero-width space/ZWNJ/ZWJ/direction marks
    (0x2028, 0x202E),   // line/paragraph separators, direction overrides
    (0x2060, 0x2064),   // word joiner and friends
    (0x20D0, 0x20FF),   // Combining Diacritical Marks for Symbols
    (0x3099, 0x309A),   // combining katakana-hiragana voicing marks
    (0xFE00, 0xFE0F),   // variation selectors
    (0xFE20, 0xFE2F),   // combining half marks
    (0xFEFF, 0xFEFF),   // zero-width no-break space / BOM
    (0xE0100, 0xE01EF), // variation selectors supplement
];

// East-Asian-Wide/Fullwidth -- same shape.
const WIDE: &[(u32, u32)] = &[
    (0x1100, 0x115F),   // Hangul Jamo
    (0x2E80, 0x303E),   // CJK Radicals, Kangxi Radicals, CJK punctuation
    (0x3041, 0x33FF),   // Hiragana, Katakana, Bopomofo, CJK compatibility
    (0x3400, 0x4DBF),   // CJK Unified Ideographs Extension A
    (0x4E00, 0x9FFF),   // CJK Unified Ideographs
    (0xA000, 0xA4CF),   // Yi Syllables/Radicals
    (0xAC00, 0xD7A3),   // Hangul Syllables
    (0xF900, 0xFAFF),   // CJK Compatibility Ideographs
    (0xFF00, 0xFF60),   // Fullwidth Forms
    (0xFFE0, 0xFFE6),   // Fullwidth signs
    (0x1F300, 0x1FAFF), // most emoji blocks
    (0x20000, 0x2FFFD), // CJK Unified Ideographs Extension B+ (supplementary plane)
    (0x30000, 0x3FFFD), // further CJK extension planes
];

// `\0` is the one control character worth a special case here (width
// 0, matching wcwidth's own convention) -- every other control
// character (`\t`, `\r`, raw ESC, ...) is 1, same as any other char:
// callers with real control-character semantics to honor (tab stops,
// escape-sequence parsing) already special-case those themselves
// before ever reaching this, same as `editor::visible_len` already
// does for SGR codes.
pub fn char_width(c: char) -> usize {
    let c = c as u32;
    if c == 0 {
        return 0;
    }
    if in_ranges(c, ZERO_WIDTH) {
        return 0;
    }
    if in_ranges(c, WIDE) {
        return 2;
    }
    1
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_always_one_column() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('~'), 1);
        assert_eq!(str_width("hello"), 5);
    }

    #[test]
    fn cjk_ideographs_and_hangul_are_two_columns() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('文'), 2);
        assert_eq!(char_width('한'), 2); // Hangul syllable
        assert_eq!(char_width('あ'), 2); // Hiragana
        assert_eq!(str_width("中文"), 4);
    }

    #[test]
    fn fullwidth_forms_are_two_columns() {
        assert_eq!(char_width('Ａ'), 2); // FULLWIDTH LATIN CAPITAL LETTER A
    }

    #[test]
    fn combining_marks_are_zero_width() {
        assert_eq!(char_width('\u{0301}'), 0); // COMBINING ACUTE ACCENT
        // "e" + combining acute accent visually reads as one column, not two.
        assert_eq!(str_width("e\u{0301}"), 1);
    }

    #[test]
    fn common_emoji_are_two_columns() {
        assert_eq!(char_width('😀'), 2);
    }

    #[test]
    fn nul_is_zero_width() {
        assert_eq!(char_width('\0'), 0);
    }
}
