// Display width: how many terminal columns a char takes. 0 for combining
// marks, ZWJ and variation selectors, 2 for East Asian Wide and Fullwidth
// characters (CJK, Hangul syllables, most emoji), 1 otherwise. `char_width`
// is per codepoint, the model terminals use; `str_width` and the column
// conversions are grapheme-aware (via `bishedit::grapheme`).
//
// The ranges come from `unicode_tables`, generated from the system's
// Unicode database (tools/gen-unicode-tables.pl).

use super::unicode_tables::{WIDE, ZERO_WIDTH};

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

// `\0` is 0, as in wcwidth. Other control characters are 1: callers that
// give them meaning (tab stops, escape sequences) handle them before
// this, as `editor::visible_len` does for SGR codes.
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

// A cluster is as wide as its first char: the rest (combining marks,
// ZWJ, skin tones, joined emoji) renders within that space. So a ZWJ
// family is one 2-column glyph, not 8 columns. For ordinary text, every
// char its own cluster, this is the plain per-char sum.
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

// The display column `char_index` starts at: the widths of the clusters
// before it. Past the end is the column after the last char (a cursor at
// end of line). An index inside a cluster gets the cluster's start column,
// as a combining mark adds nothing.
pub fn col_of(chars: &[char], char_index: usize) -> usize {
    let mut col = 0;
    let mut i = 0;
    while i < chars.len() && i < char_index {
        let cluster_end = crate::bishedit::grapheme::next_boundary(chars, i);
        if cluster_end > char_index {
            break;
        }
        col += char_width(chars[i]);
        i = cluster_end;
    }
    col
}

// The inverse of `col_of`: the first char index whose column is `>= col`,
// to find the first visible char after a horizontal scroll. Never inside
// a cluster: if `col` is past a cluster's first cell, that cluster is
// already partly scrolled off, so the next one's index is returned.
pub fn char_at_col(chars: &[char], col: usize) -> usize {
    let mut acc = 0;
    let mut i = 0;
    while i < chars.len() {
        if acc >= col {
            return i;
        }
        let cluster_end = crate::bishedit::grapheme::next_boundary(chars, i);
        let w = char_width(chars[i]);
        if acc + w > col {
            // Inside this cluster's span: skip it whole, since its next
            // char index could still be mid-cluster.
            return cluster_end;
        }
        acc += w;
        i = cluster_end;
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
        // `e` plus a combining acute is one column.
        assert_eq!(str_width("e\u{0301}"), 1);
    }

    #[test]
    fn zwj_emoji_sequence_is_one_clusters_own_width_not_a_sum() {
        // A ZWJ family of seven chars, four of them Wide: summed naively 8,
        // but one glyph of 2 columns.
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
        // Single-char clusters only: the plain sum.
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
        // Column 2 is the second cell of `中`, which is then half scrolled
        // off, so this resolves to `b`.
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

    #[test]
    fn col_of_treats_a_zwj_cluster_as_its_first_chars_width_alone() {
        // `a`, MAN+ZWJ+WOMAN (3 chars, 2 columns), `b`.
        let chars: Vec<char> = "a\u{1F468}\u{200D}\u{1F469}b".chars().collect();
        assert_eq!(col_of(&chars, 0), 0);
        assert_eq!(col_of(&chars, 1), 1, "right after 'a'");
        assert_eq!(col_of(&chars, 4), 3, "right after the whole cluster: 1 (a) + 2 (cluster), not 1 + 8");
        // A mid-cluster index reports the cluster's start column.
        assert_eq!(col_of(&chars, 2), 1);
        assert_eq!(col_of(&chars, 3), 1);
    }

    #[test]
    fn char_at_col_skips_a_whole_cluster_not_just_one_char() {
        let chars: Vec<char> = "a\u{1F468}\u{200D}\u{1F469}b".chars().collect();
        assert_eq!(char_at_col(&chars, 0), 0); // 'a'
        assert_eq!(char_at_col(&chars, 1), 1); // the cluster's own start
        // Column 2 is the cluster's second cell, so the whole 3-char
        // cluster is skipped.
        assert_eq!(char_at_col(&chars, 2), 4);
        assert_eq!(char_at_col(&chars, 3), 4); // 'b' starts at column 3
    }

    #[test]
    fn col_of_and_char_at_col_round_trip_with_a_zwj_cluster_present() {
        let chars: Vec<char> = "hi \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} bye".chars().collect();
        for i in 0..=chars.len() {
            let col = col_of(&chars, i);
            // A mid-cluster index doesn't round-trip, so the invariant
            // checked is that `char_at_col(col_of(i))` is a cluster start
            // at or before `i`.
            let back = char_at_col(&chars, col);
            assert!(back <= i, "char_at_col(col_of({i})) = {back} should never overshoot past {i}");
        }
    }
    // Each of these would put a pane border in the wrong column or a cursor
    // inside a glyph if wrong.
    #[test]
    fn the_generated_table_covers_what_the_recalled_one_missed() {
        // East_Asian_Width=Wide, far from the CJK blocks.
        assert_eq!(char_width('\u{231A}'), 2, "a watch is two columns wide");
        assert_eq!(char_width('\u{2B50}'), 2, "and so is a star");
        // Combining marks outside the Latin and Cyrillic blocks: Arabic,
        // Thai, Hebrew.
        for mark in ['\u{064B}', '\u{0E31}', '\u{05B0}'] {
            assert_eq!(char_width(mark), 0, "U+{:04X} is a combining mark", mark as u32);
        }
    }

    // The generator's one carve-out: a soft hyphen is a format character, and
    // terminals give it a column anyway.
    #[test]
    fn a_soft_hyphen_keeps_its_column() {
        assert_eq!(char_width('\u{00AD}'), 1);
    }

    // A spacing mark takes a column, unlike the combining marks above, which
    // is why grapheme segmentation has its own rule for it.
    #[test]
    fn a_spacing_mark_is_not_weightless() {
        assert_eq!(char_width('\u{093F}'), 1, "devanagari vowel sign I");
        assert_eq!(str_width("कि"), 1, "but the cluster it belongs to is still one column");
    }
    // Runs the generator again and checks the committed tables still match
    // this machine's Unicode database. Skips without perl, and when the
    // machine's Unicode is newer than the tables', which is a difference
    // between databases rather than a mistake.
    #[test]
    fn the_committed_tables_still_match_the_systems_own_unicode() {
        use crate::bishedit::unicode_tables as t;
        let generator = concat!(env!("CARGO_MANIFEST_DIR"), "/tools/gen-unicode-tables.pl");
        let Ok(out) = std::process::Command::new("perl").arg(generator).output() else {
            return; // no perl on this machine
        };
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            assert!(why.contains("Can't locate"), "the generator failed for a reason that is not a missing module: {why}");
            return;
        }
        let source = String::from_utf8(out.stdout).expect("the generator writes Rust source");
        let version = parse_version(&source);
        if version != t::UNICODE_VERSION {
            return; // this machine has a different Unicode than these tables came from
        }
        for (name, committed) in [
            ("WIDE", t::WIDE),
            ("ZERO_WIDTH", t::ZERO_WIDTH),
            ("EXTEND", t::EXTEND),
            ("CONTROL", t::CONTROL),
            ("PREPEND", t::PREPEND),
            ("SPACING_MARK", t::SPACING_MARK),
            ("HANGUL_L", t::HANGUL_L),
            ("HANGUL_V", t::HANGUL_V),
            ("HANGUL_T", t::HANGUL_T),
            ("EXTENDED_PICTOGRAPHIC", t::EXTENDED_PICTOGRAPHIC),
        ] {
            let fresh = parse_table(&source, name);
            assert_eq!(fresh, committed, "{name} has drifted from Unicode {version} -- regenerate it");
        }
    }

    fn parse_version(source: &str) -> String {
        let line = source.lines().find(|l| l.contains("UNICODE_VERSION")).expect("the generator records the version");
        line.split('"').nth(1).expect("as a string literal").to_string()
    }

    // A small scanner: the input is the generator's output, `pub const NAME`
    // followed by `(0x.., 0x..)` pairs up to `];`.
    fn parse_table(source: &str, name: &str) -> Vec<(u32, u32)> {
        let start = source.find(&format!("pub const {name}:")).unwrap_or_else(|| panic!("{name} is missing from the generator's output"));
        let body = &source[start..];
        let end = body.find("];").expect("every table ends");
        let mut out = Vec::new();
        for pair in body[..end].split('(').skip(1) {
            let inside = pair.split(')').next().unwrap_or("");
            let mut halves = inside.split(',').map(str::trim).filter_map(|h| h.strip_prefix("0x"));
            if let (Some(lo), Some(hi)) = (halves.next(), halves.next()) {
                out.push((u32::from_str_radix(lo, 16).unwrap(), u32::from_str_radix(hi, 16).unwrap()));
            }
        }
        out
    }
}
