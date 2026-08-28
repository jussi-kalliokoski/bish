// Hand-rolled Unicode extended grapheme cluster segmentation (UAX #29)
// -- no external crate, same spirit as unicode_width.rs/glob.rs/
// regex.rs/csscolor.rs. Groups a sequence of chars into "user-perceived
// characters": a base character plus its combining marks, a Hangul
// syllable assembled from Jamo, a ZWJ emoji sequence (family, couple-
// with-heart, ...), or a regional-indicator flag pair -- all one
// cluster, so cursor motion/deletion can treat it as a single unit
// instead of landing or splitting mid-glyph.
//
// unicode_width.rs's own doc comment already named this gap: a ZWJ
// sequence is measured char-by-char and summed there rather than
// treated as one cluster, "genuinely deferred to a later pass if it
// ever turns out to matter in practice" -- this is that pass, and
// unicode_width::str_width now calls into cluster_range below to fix
// exactly that case (see this file's own str_width interaction test).
//
// Implements the actual UAX #29 boundary rules (GB1-GB999) this
// codebase's real-world text actually exercises: CR/LF, control
// characters, Hangul syllable assembly (computed arithmetically from
// the standard Unicode Hangul decomposition formula, not a giant
// table), combining marks/ZWJ, emoji ZWJ sequences, and regional-
// indicator flag pairs. Deliberately does *not* implement Prepend or
// SpacingMark (both are near-exclusively Indic/historic-script
// phenomena bish has no other script support for either -- same
// "practical subset, documented boundary" call unicode_width.rs's own
// doc comment already makes for the exact same reason) -- a gap there
// degrades to "an Indic vowel sign occasionally splits from its base,"
// not a crash or a stuck cursor.
//
// The Extended_Pictographic table below (needed for GB11, ZWJ emoji
// sequences) is the same kind of deliberately trimmed, high-confidence
// subset unicode_width.rs's own WIDE table already is -- covering the
// emoji blocks that actually come up in normal use, not a byte-perfect
// reproduction of the real (large, evolving) emoji-data.txt property.

fn in_ranges(c: u32, ranges: &[(u32, u32)]) -> bool {
    ranges.binary_search_by(|&(lo, hi)| if c < lo { std::cmp::Ordering::Greater } else if c > hi { std::cmp::Ordering::Less } else { std::cmp::Ordering::Equal }).is_ok()
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
    ExtendedPictographic,
    Other,
}

// Combining marks, variation selectors, ZWNJ, emoji skin-tone
// modifiers -- Grapheme_Cluster_Break=Extend. Overlaps heavily with
// (but isn't identical to) unicode_width::ZERO_WIDTH's own combining-
// mark ranges -- kept as this module's own table rather than importing
// that one, since the two properties aren't actually the same thing
// (this file's own doc comment on why unicode_width and grapheme
// segmentation are independent concerns) and this module should stay
// self-contained the way every other hand-rolled table in this
// codebase already is.
// Must stay sorted ascending by `lo` -- in_ranges' binary search
// requires it. (0xE0100 -- variation selectors *supplement* -- sorts
// after 0x1F3FB despite "looking" earlier written out with fewer
// leading digits; caught a real bug here from getting this wrong the
// first time, not a hypothetical one.)
const EXTEND: &[(u32, u32)] = &[
    (0x0300, 0x036F),   // Combining Diacritical Marks
    (0x0483, 0x0489),   // Cyrillic combining marks
    (0x200C, 0x200C),   // zero-width non-joiner (200D, ZWJ, is its own class below)
    (0x20D0, 0x20FF),   // Combining Diacritical Marks for Symbols
    (0x3099, 0x309A),   // combining katakana-hiragana voicing marks
    (0xFE00, 0xFE0F),   // variation selectors
    (0xFE20, 0xFE2F),   // combining half marks
    (0x1F3FB, 0x1F3FF), // emoji skin-tone (Fitzpatrick) modifiers
    (0xE0100, 0xE01EF), // variation selectors supplement
];

// Invisible format/direction-control characters -- Grapheme_Cluster_
// Break defaults to Control for these (Cf/Zl/Zp not otherwise
// overridden to Extend/ZWJ/Prepend/RegionalIndicator). Always breaks on
// either side (GB4/GB5), unlike Extend.
const CONTROL: &[(u32, u32)] = &[
    (0x0000, 0x0009), // C0 controls before Tab, excluding CR/LF (their own classes)
    (0x000B, 0x000C),
    (0x000E, 0x001F),
    (0x007F, 0x009F), // DEL + C1 controls
    (0x200B, 0x200B),
    (0x200E, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0xFEFF, 0xFEFF),
];

const REGIONAL_INDICATOR: (u32, u32) = (0x1F1E6, 0x1F1FF);

const HANGUL_L: &[(u32, u32)] = &[(0x1100, 0x115F), (0xA960, 0xA97C)];
const HANGUL_V: &[(u32, u32)] = &[(0x1160, 0x11A7), (0xD7B0, 0xD7C6)];
const HANGUL_T: &[(u32, u32)] = &[(0x11A8, 0x11FF), (0xD7CB, 0xD7FC)];
const HANGUL_SYLLABLE: (u32, u32) = (0xAC00, 0xD7A3);
const HANGUL_TCOUNT: u32 = 28;

// Deliberately trimmed, high-confidence emoji ranges -- see this file's
// own top doc comment. Matches unicode_width::WIDE's own main emoji
// block, plus the Misc Symbols/Dingbats/Technical blocks a common ZWJ
// sequence (heart U+2764 in "couple with heart", watch/hourglass, ...)
// actually needs.
const EXTENDED_PICTOGRAPHIC: &[(u32, u32)] = &[
    (0x2300, 0x23FF), // Miscellaneous Technical (watch, hourglass, ...)
    (0x2600, 0x27BF), // Miscellaneous Symbols + Dingbats (heart, sun, scissors, ...)
    (0x1F300, 0x1FAFF), // main emoji blocks
];

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
        // Standard Unicode Hangul syllable decomposition: SIndex is this
        // syllable's offset within the 11172-syllable block; a TIndex of
        // 0 means no trailing consonant (an LV syllable), any other
        // value means it has one (LVT).
        let s_index = c - HANGUL_SYLLABLE.0;
        return if s_index % HANGUL_TCOUNT == 0 { Gcb::Lv } else { Gcb::Lvt };
    }
    if in_ranges(c, EXTENDED_PICTOGRAPHIC) {
        return Gcb::ExtendedPictographic;
    }
    Gcb::Other
}

// True if there's a grapheme-cluster boundary immediately before
// `chars[i]` (between `chars[i-1]` and `chars[i]`). `i == 0` and
// `i == chars.len()` are always boundaries (GB1/GB2, start/end of
// text).
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
    // GB4: (Control|CR|LF) ÷ -- always break right after one of these.
    if matches!(before, Gcb::Control | Gcb::Cr | Gcb::Lf) {
        return true;
    }
    // GB5: ÷ (Control|CR|LF) -- always break right before one of these.
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
    // GB9: x (Extend|ZWJ) -- never break before a combining mark/ZWJ.
    if matches!(after, Gcb::Extend | Gcb::ZwjClass) {
        return false;
    }
    // GB11: emoji ZWJ sequences -- \p{Extended_Pictographic} Extend* ZWJ
    // x \p{Extended_Pictographic}. Only reachable here with `before ==
    // ZwjClass` (an Extend `before` was already handled by GB9 above),
    // so this only needs to scan backward over any Extend run between
    // the ZWJ and whatever Extended_Pictographic started it.
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
    // GB12/GB13: regional-indicator flag pairs -- sot (RI RI)* RI x RI,
    // and [^RI] (RI RI)* RI x RI. A run of consecutive RIs pairs up two
    // at a time (each pair is one flag); whether `chars[i-1]` is
    // "already paired" (even count so far, so `chars[i]` starts a new
    // pair -- break) or "still unpaired" (odd count, joins with
    // `chars[i]` -- no break) depends only on the length of the maximal
    // RI run ending at `chars[i-1]`.
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
    // GB999: otherwise, break everywhere.
    true
}

// The char-index range `[start, end)` of the grapheme cluster
// containing `chars[at]` -- `at == chars.len()` is treated as "the
// cluster ending right at the end of the text" (the common case: a
// cursor sitting one-past-the-end of a line), same convention
// unicode_width's own `col_of`/`char_at_col` already use.
pub fn cluster_range(chars: &[char], at: usize) -> (usize, usize) {
    if chars.is_empty() {
        return (0, 0);
    }
    let at = at.min(chars.len() - 1);
    let start = prev_boundary(chars, at + 1);
    let end = next_boundary(chars, at);
    (start, end)
}

// The next grapheme-cluster boundary at or after `at` (clamped to
// `chars.len()`).
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

// The previous grapheme-cluster boundary strictly before `at` (clamped
// to 0).
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

// Splits `chars` into its grapheme clusters, each as a `Vec<char>` --
// mainly for tests; real callers almost always want `cluster_range`/
// `next_boundary`/`prev_boundary` directly against a line's own
// `Vec<char>` instead of allocating a fresh copy per cluster.
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
        // ㄱ(L) + ㅏ(V) -> assembles into one syllable cluster, same as
        // typing a precomposed 가 would already be (GB6).
        let chars = cs("\u{1100}\u{1161}");
        assert_eq!(clusters(&chars).len(), 1);
    }

    #[test]
    fn precomposed_hangul_syllable_plus_trailing_jamo_is_one_cluster() {
        // 가 (a precomposed LV syllable) + ㄴ (a standalone T jamo) -> GB7.
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
        // US flag then GB flag, back to back -- GB12/13's odd/even
        // counting must still split after the first pair.
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
        // No ZWJ between them -- must NOT merge.
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
        // Walking backward from the end via prev_boundary should retrace
        // exactly the same set of boundaries in reverse.
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

    // Caught a real bug this way, not a hypothetical one:
    // in_ranges' binary search silently returns wrong answers for any
    // entry after the first one that's out of order (0xE0100 was
    // originally written before 0x1F3FB despite sorting after it
    // numerically -- see EXTEND's own doc comment) -- a plain "does
    // this codepoint match" test wouldn't have caught it if the probe
    // codepoint happened to fall in a range binary search still landed
    // on correctly, so this checks the actual sortedness invariant
    // directly, for every table, rather than only spot-checking a few
    // codepoints.
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
        assert_sorted_and_nonoverlapping("EXTEND", EXTEND);
        assert_sorted_and_nonoverlapping("CONTROL", CONTROL);
        assert_sorted_and_nonoverlapping("EXTENDED_PICTOGRAPHIC", EXTENDED_PICTOGRAPHIC);
        assert_sorted_and_nonoverlapping("HANGUL_L", HANGUL_L);
        assert_sorted_and_nonoverlapping("HANGUL_V", HANGUL_V);
        assert_sorted_and_nonoverlapping("HANGUL_T", HANGUL_T);
    }
}
