// Hand-rolled Unicode display-width table -- no external crate, same
// spirit as glob.rs/regex.rs/csscolor.rs. How many terminal columns one
// char actually occupies: 0 for a combining mark/zero-width joiner/
// variation selector, 2 for an East-Asian-Wide/Fullwidth character (CJK
// ideographs, Hangul syllables, most emoji, fullwidth forms), 1 for
// everything else. This is deliberately the well-known "per-codepoint
// width" model every terminal emulator and most terminal apps (tmux,
// less, vim's own default) already use, at the single-char level --
// `char_width` itself has no notion of a multi-codepoint sequence being
// one cluster at all, and stays that way (real cursor motion needs a
// char index either way, per-char, to stay addressable).
//
// `str_width` below *is* now grapheme-cluster-aware, via
// `bishedit::grapheme` -- this used to be the one real gap this
// comment flagged ("a ZWJ emoji family... measured char-by-char and
// summed here rather than treated as one cluster... genuinely deferred
// to a later pass"). Fixed: see `str_width`'s own doc comment.
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

// A grapheme cluster's own display width is its *first* char's width
// alone, not the sum of every char in it -- everything else in the
// cluster (combining marks, ZWJ, skin-tone modifiers, joined emoji)
// renders *within* the space that first char already occupies, not
// stacked additionally alongside it. Fixes the ZWJ-sequence gap this
// module's own top doc comment used to flag: a family emoji (person +
// ZWJ + person + ZWJ + child, each individually width-2) used to sum
// to 6+ columns; it's one ~2-column glyph in any terminal that
// understands ZWJ joining, matching real terminal behavior. Ordinary
// text (every char its own single-char cluster) is unaffected -- this
// reduces to the exact same per-char sum `char_width` always gave.
pub fn str_width(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut width = 0;
    let mut i = 0;
    while i < chars.len() {
        width += char_width(chars[i]);
        i = crate::bishedit::grapheme::next_boundary(&chars, i);
    }
    width
}

// The display column a given char index within `chars` actually starts
// at -- the sum of every earlier char's own width. `char_index` past
// the end of `chars` is treated as "the column right after the last
// char" (the common case: a cursor sitting one-past-the-end of a
// line), same as indexing a line one past its last real column already
// means elsewhere in this codebase.
pub fn col_of(chars: &[char], char_index: usize) -> usize {
    chars.iter().take(char_index).map(|&c| char_width(c)).sum()
}

// The inverse of `col_of`: the first char index whose own `col_of` is
// `>= col` -- used to find "which character is the first one visible"
// once the viewport has scrolled to some display column, not just some
// char index. Never lands *inside* a wide char (there's no such thing
// as half a CJK glyph rendered on screen): if `col` points into the
// second cell of a wide char, that char's own left cell would already
// be scrolled out of view, so this skips past it entirely and returns
// the *next* char's index instead of showing a half-obscured glyph.
pub fn char_at_col(chars: &[char], col: usize) -> usize {
    let mut acc = 0;
    for (i, &c) in chars.iter().enumerate() {
        if acc >= col {
            return i;
        }
        acc += char_width(c);
    }
    chars.len()
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
    fn zwj_emoji_sequence_is_one_clusters_own_width_not_a_sum() {
        // MAN + ZWJ + WOMAN + ZWJ + GIRL + ZWJ + BOY -- a "family" ZWJ
        // sequence, 7 individually-Wide-or-zero-width chars. Naive
        // per-char summing gives 8 (four Wide chars x2); this is one
        // cluster, one glyph, 2 columns.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(str_width(family), 2);
    }

    #[test]
    fn two_family_emoji_side_by_side_are_two_clusters_worth_of_width() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let two = format!("{family}{family}");
        assert_eq!(str_width(&two), 4);
    }

    #[test]
    fn ordinary_multi_char_text_still_sums_per_char_as_before() {
        // Every char here is its own single-char cluster, so this must
        // reduce to exactly the same result str_width already gave
        // before it became cluster-aware.
        assert_eq!(str_width("中文 hello"), 4 + 1 + 5);
    }

    #[test]
    fn common_emoji_are_two_columns() {
        assert_eq!(char_width('😀'), 2);
    }

    #[test]
    fn nul_is_zero_width() {
        assert_eq!(char_width('\0'), 0);
    }

    #[test]
    fn col_of_sums_widths_of_every_earlier_char() {
        let chars: Vec<char> = "a中b".chars().collect(); // 1 + 2 + 1
        assert_eq!(col_of(&chars, 0), 0);
        assert_eq!(col_of(&chars, 1), 1); // right after "a"
        assert_eq!(col_of(&chars, 2), 3); // right after "中" (a=1 + 中=2)
        assert_eq!(col_of(&chars, 3), 4); // right after "b", one past the end
    }

    #[test]
    fn char_at_col_finds_the_char_covering_a_display_column() {
        let chars: Vec<char> = "a中b".chars().collect();
        assert_eq!(char_at_col(&chars, 0), 0); // "a" starts at column 0
        assert_eq!(char_at_col(&chars, 1), 1); // "中" starts at column 1
        // Column 2 is the *second* cell of "中" -- "中" itself would
        // already be half scrolled out of view there, so this skips
        // past it and resolves to "b" instead of showing a half glyph.
        assert_eq!(char_at_col(&chars, 2), 2);
        assert_eq!(char_at_col(&chars, 3), 2); // "b" starts at column 3
        assert_eq!(char_at_col(&chars, 4), 3); // past the end
        assert_eq!(char_at_col(&chars, 100), 3); // clamps, doesn't panic
    }

    #[test]
    fn col_of_and_char_at_col_round_trip_on_char_boundaries() {
        let chars: Vec<char> = "hello 世界 world".chars().collect();
        for i in 0..=chars.len() {
            let col = col_of(&chars, i);
            assert_eq!(char_at_col(&chars, col), i);
        }
    }
}
