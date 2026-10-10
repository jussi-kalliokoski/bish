// Extended grapheme cluster segmentation (UAX #29): groups chars into
// user-perceived characters (a base and its combining marks, a Hangul
// syllable from Jamo, a ZWJ emoji sequence, a flag pair) so cursor motion
// and deletion treat each as one unit. `unicode_width::str_width` uses it
// to measure a ZWJ sequence as one cluster.
//
// Implements GB1-GB999. Hangul syllables are decomposed arithmetically
// rather than from a table; every other property comes from
// `unicode_tables`, generated from the system's Unicode character database
// (tools/gen-unicode-tables.pl).

fn in_ranges(c: u32, ranges: &[(u32, u32)]) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if c < lo {
                std::cmp::Ordering::Greater
            } else if c > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gcb {
    Cr,
    Lf,
    Control,
    Extend,
    ZwjClass,
    RegionalIndicator,
    L,
    V,
    T,
    Lv,
    Lvt,
    Prepend,
    SpacingMark,
    ExtendedPictographic,
    Other,
}

use super::unicode_tables::{CONTROL, EXTEND, EXTENDED_PICTOGRAPHIC, HANGUL_L, HANGUL_T, HANGUL_V, PREPEND, SPACING_MARK};

const REGIONAL_INDICATOR: (u32, u32) = (0x1F1E6, 0x1F1FF);

const HANGUL_SYLLABLE: (u32, u32) = (0xAC00, 0xD7A3);
const HANGUL_TCOUNT: u32 = 28;

fn gcb_class(c: char) -> Gcb {
    let c = c as u32;
    match c {
        0x0D => return Gcb::Cr,
        0x0A => return Gcb::Lf,
        0x200D => return Gcb::ZwjClass,
        _ => {}
    }
    if in_ranges(c, CONTROL) {
        return Gcb::Control;
    }
    if in_ranges(c, EXTEND) {
        return Gcb::Extend;
    }
    if in_ranges(c, PREPEND) {
        return Gcb::Prepend;
    }
    if in_ranges(c, SPACING_MARK) {
        return Gcb::SpacingMark;
    }
    if c >= REGIONAL_INDICATOR.0 && c <= REGIONAL_INDICATOR.1 {
        return Gcb::RegionalIndicator;
    }
    if in_ranges(c, HANGUL_L) {
        return Gcb::L;
    }
    if in_ranges(c, HANGUL_V) {
        return Gcb::V;
    }
    if in_ranges(c, HANGUL_T) {
        return Gcb::T;
    }
    if c >= HANGUL_SYLLABLE.0 && c <= HANGUL_SYLLABLE.1 {
        // Unicode's Hangul decomposition: SIndex is the offset in the
        // 11172-syllable block, and a TIndex of 0 means no trailing
        // consonant (LV rather than LVT).
        let s_index = c - HANGUL_SYLLABLE.0;
        return if s_index % HANGUL_TCOUNT == 0 { Gcb::Lv } else { Gcb::Lvt };
    }
    if in_ranges(c, EXTENDED_PICTOGRAPHIC) {
        return Gcb::ExtendedPictographic;
    }
    Gcb::Other
}

// Whether there's a boundary just before `chars[i]`. `i == 0` and
// `i == chars.len()` always are (GB1/GB2).
pub fn is_boundary(chars: &[char], i: usize) -> bool {
    if i == 0 || i >= chars.len() {
        return true;
    }
    let before = gcb_class(chars[i - 1]);
    let after = gcb_class(chars[i]);

    // GB3: CR x LF
    if before == Gcb::Cr && after == Gcb::Lf {
        return false;
    }
    // GB4: (Control|CR|LF) ÷
    if matches!(before, Gcb::Control | Gcb::Cr | Gcb::Lf) {
        return true;
    }
    // GB5: ÷ (Control|CR|LF)
    if matches!(after, Gcb::Control | Gcb::Cr | Gcb::Lf) {
        return true;
    }
    // GB6/7/8: Hangul syllable assembly.
    if before == Gcb::L && matches!(after, Gcb::L | Gcb::V | Gcb::Lv | Gcb::Lvt) {
        return false;
    }
    if matches!(before, Gcb::Lv | Gcb::V) && matches!(after, Gcb::V | Gcb::T) {
        return false;
    }
    if matches!(before, Gcb::Lvt | Gcb::T) && after == Gcb::T {
        return false;
    }
    // GB9: × (Extend|ZWJ)
    if matches!(after, Gcb::Extend | Gcb::ZwjClass) {
        return false;
    }
    // GB9a: × SpacingMark. A mark that takes a column of its own still
    // belongs to the character before it: a Devanagari vowel sign, as in
    // `कि`.
    if after == Gcb::SpacingMark {
        return false;
    }
    // GB9b: Prepend ×. A character that belongs to what follows, such as the
    // Arabic number sign.
    if before == Gcb::Prepend {
        return false;
    }
    // GB11: \p{Extended_Pictographic} Extend* ZWJ ×
    // \p{Extended_Pictographic}. An Extend `before` was handled by GB9, so
    // `before` is the ZWJ here, and this scans back over any Extend run to
    // the pictograph that started it.
    if before == Gcb::ZwjClass && after == Gcb::ExtendedPictographic {
        let zwj_index = i - 1;
        let mut k = zwj_index;
        while k > 0 && gcb_class(chars[k - 1]) == Gcb::Extend {
            k -= 1;
        }
        if k > 0 && gcb_class(chars[k - 1]) == Gcb::ExtendedPictographic {
            return false;
        }
    }
    // GB12/GB13: RIs pair up two at a time, one flag per pair. Whether
    // `chars[i]` joins `chars[i-1]` depends only on the length of the RI run
    // ending at `chars[i-1]`: odd joins, even breaks.
    if before == Gcb::RegionalIndicator && after == Gcb::RegionalIndicator {
        let mut count = 0usize;
        let mut k = i;
        while k > 0 && gcb_class(chars[k - 1]) == Gcb::RegionalIndicator {
            count += 1;
            k -= 1;
        }
        if count % 2 == 1 {
            return false;
        }
    }
    // GB999: otherwise, break.
    true
}

// The char range `[start, end)` of the cluster containing `chars[at]`.
// `at == chars.len()` is the cluster ending at the end of the text, as for
// `unicode_width`'s `col_of`.
pub fn cluster_range(chars: &[char], at: usize) -> (usize, usize) {
    if chars.is_empty() {
        return (0, 0);
    }
    let at = at.min(chars.len() - 1);
    let start = prev_boundary(chars, at + 1);
    let end = next_boundary(chars, at);
    (start, end)
}

// The next boundary at or after `at`, clamped to `chars.len()`.
pub fn next_boundary(chars: &[char], at: usize) -> usize {
    let mut i = at.min(chars.len());
    if i >= chars.len() {
        return chars.len();
    }
    i += 1;
    while i < chars.len() && !is_boundary(chars, i) {
        i += 1;
    }
    i
}

// The previous boundary strictly before `at`, clamped to 0.
pub fn prev_boundary(chars: &[char], at: usize) -> usize {
    let mut i = at.min(chars.len());
    if i == 0 {
        return 0;
    }
    i -= 1;
    while i > 0 && !is_boundary(chars, i) {
        i -= 1;
    }
    i
}

// Each cluster as its own `Vec<char>`, mostly for tests; callers usually
// want `cluster_range` and the boundary functions.
pub fn clusters(chars: &[char]) -> Vec<Vec<char>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = next_boundary(chars, i);
        out.push(chars[i..end].to_vec());
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn plain_ascii_is_one_cluster_per_char() {
        let chars = cs("abc");
        assert_eq!(clusters(&chars), vec![vec!['a'], vec!['b'], vec!['c']]);
    }

    #[test]
    fn base_plus_combining_mark_is_one_cluster() {
        // "e" + COMBINING ACUTE ACCENT
        let chars = cs("e\u{0301}");
        assert_eq!(clusters(&chars), vec![vec!['e', '\u{0301}']]);
        assert!(!is_boundary(&chars, 1));
    }

    #[test]
    fn base_plus_several_combining_marks_is_still_one_cluster() {
        let chars: Vec<char> = "a\u{0301}\u{0302}\u{0303}".chars().collect();
        assert_eq!(clusters(&chars).len(), 1);
        assert_eq!(clusters(&chars)[0].len(), 4);
    }

    #[test]
    fn cr_lf_do_not_split() {
        let chars = cs("\r\n");
        assert_eq!(clusters(&chars), vec![vec!['\r', '\n']]);
    }

    #[test]
    fn cr_alone_and_lf_alone_are_separate_clusters_from_neighbors() {
        let chars = cs("a\rb");
        assert_eq!(clusters(&chars), vec![vec!['a'], vec!['\r'], vec!['b']]);
    }

    #[test]
    fn hangul_syllable_assembled_from_jamo_is_one_cluster() {
        // ㄱ (L) + ㅏ (V) assemble into one syllable cluster, GB6.
        let chars = cs("\u{1100}\u{1161}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn precomposed_hangul_syllable_plus_trailing_jamo_is_one_cluster() {
        // 가 (a precomposed LV syllable) + ㄴ (a T jamo), GB7.
        let chars = cs("\u{AC00}\u{11AB}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn ordinary_precomposed_hangul_syllable_is_its_own_single_char_cluster() {
        let chars = cs("한글");
        assert_eq!(clusters(&chars), vec![vec!['한'], vec!['글']]);
    }

    #[test]
    fn regional_indicator_pair_is_one_flag_cluster() {
        // US flag: REGIONAL INDICATOR SYMBOL LETTER U + LETTER S
        let chars = cs("\u{1F1FA}\u{1F1F8}");
        assert_eq!(clusters(&chars).len(), 1);
        assert_eq!(clusters(&chars)[0].len(), 2);
    }

    #[test]
    fn two_flags_in_a_row_are_two_separate_clusters_not_one_run() {
        // Two flags back to back split after the first pair.
        let chars = cs("\u{1F1FA}\u{1F1F8}\u{1F1EC}\u{1F1E7}");
        let cl = clusters(&chars);
        assert_eq!(cl.len(), 2);
        assert_eq!(cl[0].len(), 2);
        assert_eq!(cl[1].len(), 2);
    }

    #[test]
    fn family_zwj_sequence_is_one_cluster() {
        // MAN + ZWJ + WOMAN + ZWJ + GIRL + ZWJ + BOY
        let chars = cs("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}");
        assert_eq!(clusters(&chars).len(), 1);
        assert_eq!(clusters(&chars)[0].len(), 7);
    }

    #[test]
    fn couple_with_heart_zwj_sequence_is_one_cluster() {
        // MAN + ZWJ + HEAVY BLACK HEART + ZWJ + MAN
        let chars = cs("\u{1F468}\u{200D}\u{2764}\u{200D}\u{1F468}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn skin_tone_modified_emoji_is_one_cluster() {
        // THUMBS UP + EMOJI MODIFIER FITZPATRICK TYPE-4
        let chars = cs("\u{1F44D}\u{1F3FD}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn rainbow_flag_zwj_sequence_is_one_cluster() {
        // WAVING WHITE FLAG + VARIATION SELECTOR-16 + ZWJ + RAINBOW
        let chars = cs("\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn two_plain_emoji_side_by_side_are_two_clusters() {
        // No ZWJ between them, so no merge.
        let chars = cs("\u{1F600}\u{1F600}");
        assert_eq!(clusters(&chars).len(), 2);
    }

    #[test]
    fn mixed_text_clusters_correctly() {
        let chars = cs("h\u{1100}\u{1161}i");
        let cl = clusters(&chars);
        assert_eq!(cl.len(), 3);
        assert_eq!(cl[0], vec!['h']);
        assert_eq!(cl[1].len(), 2);
        assert_eq!(cl[2], vec!['i']);
    }

    #[test]
    fn cluster_range_finds_the_bounds_of_a_cluster_from_any_char_index_inside_it() {
        let chars = cs("a\u{1F468}\u{200D}\u{1F469}b");
        // indices: 0='a', 1..3 = the ZWJ cluster, 4='b'
        assert_eq!(cluster_range(&chars, 0), (0, 1));
        assert_eq!(cluster_range(&chars, 1), (1, 4));
        assert_eq!(cluster_range(&chars, 2), (1, 4), "querying from mid-cluster still finds the whole cluster");
        assert_eq!(cluster_range(&chars, 3), (1, 4));
        assert_eq!(cluster_range(&chars, 4), (4, 5));
    }

    #[test]
    fn cluster_range_on_an_empty_slice_is_a_degenerate_zero_range() {
        assert_eq!(cluster_range(&[], 0), (0, 0));
    }

    #[test]
    fn next_and_prev_boundary_round_trip_over_mixed_text() {
        let chars = cs("a\u{1F468}\u{200D}\u{1F469}b한글\u{1F1FA}\u{1F1F8}c");
        let mut boundaries = vec![0];
        let mut i = 0;
        while i < chars.len() {
            i = next_boundary(&chars, i);
            boundaries.push(i);
        }
        assert_eq!(*boundaries.last().unwrap(), chars.len());
        // Walking back with `prev_boundary` retraces the same boundaries.
        let mut back = vec![chars.len()];
        let mut j = chars.len();
        while j > 0 {
            j = prev_boundary(&chars, j);
            back.push(j);
        }
        back.reverse();
        assert_eq!(back, boundaries);
    }

    #[test]
    fn next_boundary_clamps_at_the_end_instead_of_panicking() {
        let chars = cs("ab");
        assert_eq!(next_boundary(&chars, 2), 2);
        assert_eq!(next_boundary(&chars, 100), 2);
    }

    #[test]
    fn prev_boundary_clamps_at_zero_instead_of_underflowing() {
        let chars = cs("ab");
        assert_eq!(prev_boundary(&chars, 0), 0);
    }

    // Checks the sortedness invariant `in_ranges`' binary search relies on,
    // for every table: an out-of-order entry gives wrong answers that a
    // spot-check of a few codepoints can miss.
    fn assert_sorted_and_nonoverlapping(name: &str, ranges: &[(u32, u32)]) {
        for w in ranges.windows(2) {
            let (a_lo, a_hi) = w[0];
            let (b_lo, _) = w[1];
            assert!(a_lo <= a_hi, "{name}: malformed range ({a_lo:#X}, {a_hi:#X})");
            assert!(a_hi < b_lo, "{name}: not sorted/overlapping at ({a_lo:#X}, {a_hi:#X}) then ({b_lo:#X}, ..)");
        }
    }

    #[test]
    fn every_range_table_is_sorted_and_nonoverlapping() {
        use crate::bishedit::unicode_tables::{SPACING_MARK, WIDE, ZERO_WIDTH};
        assert_sorted_and_nonoverlapping("EXTEND", EXTEND);
        assert_sorted_and_nonoverlapping("CONTROL", CONTROL);
        assert_sorted_and_nonoverlapping("EXTENDED_PICTOGRAPHIC", EXTENDED_PICTOGRAPHIC);
        assert_sorted_and_nonoverlapping("HANGUL_L", HANGUL_L);
        assert_sorted_and_nonoverlapping("HANGUL_V", HANGUL_V);
        assert_sorted_and_nonoverlapping("HANGUL_T", HANGUL_T);
        assert_sorted_and_nonoverlapping("PREPEND", PREPEND);
        assert_sorted_and_nonoverlapping("SPACING_MARK", SPACING_MARK);
        assert_sorted_and_nonoverlapping("WIDE", WIDE);
        assert_sorted_and_nonoverlapping("ZERO_WIDTH", ZERO_WIDTH);
    }

    // GB9a: a Devanagari vowel sign takes its own column, so it isn't Extend,
    // but it belongs to the consonant before it.
    #[test]
    fn a_spacing_mark_joins_the_character_before_it() {
        let chars: Vec<char> = "कि".chars().collect();
        assert_eq!(chars.len(), 2, "two code points");
        assert_eq!(cluster_range(&chars, 0), (0, 2), "and one cluster");
        assert!(!is_boundary(&chars, 1));
    }

    // GB9b: a character that belongs to what follows.
    #[test]
    fn a_prepend_joins_the_character_after_it() {
        let chars: Vec<char> = "\u{0600}7".chars().collect();
        assert_eq!(cluster_range(&chars, 0), (0, 2));
        assert!(!is_boundary(&chars, 1));
    }

    // A star is Extended_Pictographic, so this ZWJ sequence is one cluster.
    #[test]
    fn a_zwj_sequence_holds_together_outside_the_blocks_that_were_guessed_at() {
        let chars: Vec<char> = "\u{2B50}\u{200D}\u{2B50}".chars().collect();
        assert_eq!(cluster_range(&chars, 0), (0, 3));
    }
}
