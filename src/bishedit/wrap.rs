// How a buffer line longer than the pane is laid out: vim's `wrap`,
// `linebreak`, `breakindent` and `showbreak`, plus the two that matter
// when wrapping is *off* (`sidescrolloff`, and the `extends`/`precedes`
// markers from `listchars`).
//
// Pure: given a line's characters, a width and the options, it says
// which characters go on which screen row. Everything about scrolling,
// gutters and cursors is the editor's own problem, and keeping this
// side of it free of those is what makes the awkward cases -- a
// double-width character straddling the edge, a word longer than the
// pane, a line of nothing but spaces -- testable without a terminal.
//
// Deliberately not here: `textwidth`, which is a *hard* wrap that edits
// the buffer as you type. That is an editing feature, not a display one,
// and putting the two behind neighbouring names would invite exactly the
// confusion vim users already have about them.

use super::unicode_width::char_width;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub wrap: bool,
    // Break between words rather than at whatever character the edge
    // lands on.
    pub linebreak: bool,
    // Continued rows start at the same indent the line itself does.
    pub breakindent: bool,
    // Printed at the start of every continued row.
    pub showbreak: String,
    // Wrap here rather than at the pane's edge; 0 means the edge. Always
    // capped by the pane, so a narrow pane still wraps at the pane.
    pub column: usize,
    // With `wrap` off: how many columns to keep visible either side of
    // the cursor.
    // Vertical counterpart to `sidescrolloff`, and the one option here
    // that has nothing to do with wrapping -- it lives on this struct
    // because this is where a buffer's view options are, and splitting
    // "how the view scrolls" across two homes would be worse than the
    // slightly wide name.
    pub scrolloff: usize,
    pub sidescrolloff: usize,
    // With `wrap` off: what marks a line continuing past the right or
    // left edge. Empty for nothing, which is the default.
    pub extends: String,
    pub precedes: String,
}

impl Default for Options {
    // The defaults are `wrap` off -- the behaviour this editor has
    // always had, and vim's `nowrap` -- with the options that shape a
    // wrapped line already set the way someone turning wrapping on
    // would want them.
    fn default() -> Options {
        Options {
            wrap: false,
            linebreak: true,
            breakindent: true,
            showbreak: "\u{21B3} ".to_string(),
            column: 0,
            scrolloff: 0,
            sidescrolloff: 0,
            extends: String::new(),
            precedes: String::new(),
        }
    }
}

// One screen row's worth of a buffer line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    // Display columns of prefix this row opens with -- zero on a line's
    // first row, `showbreak` plus the line's own indent on the rest.
    pub indent: usize,
}

impl Segment {
    pub fn contains(&self, col: usize) -> bool {
        col >= self.start && (col < self.end || (col == self.end && self.end == self.start))
    }
}

// vim's own `breakat`: where a line may be broken when `linebreak` is
// on. Whitespace plus the punctuation that reads as a seam rather than
// as part of a word.
fn is_break_at(c: char) -> bool {
    c.is_whitespace() || matches!(c, '!' | '@' | '*' | '-' | '+' | ';' | ':' | ',' | '.' | '/' | '?')
}

// The rows `chars` occupies in a pane `width` columns wide. Always at
// least one segment, even for an empty line, so a caller can index the
// first without checking.
pub fn segments(chars: &[char], width: usize, opts: &Options) -> Vec<Segment> {
    let width = width.max(1);
    if !opts.wrap {
        return vec![Segment { start: 0, end: chars.len(), indent: 0 }];
    }
    // `wrap_column` narrows the text, never widens it past the pane.
    let first_width = if opts.column == 0 { width } else { opts.column.min(width) };
    let indent = continuation_indent(chars, width, opts);
    // At least one column has to remain for content, however deep the
    // indent would like to be.
    let rest_width = first_width.saturating_sub(indent).max(1);

    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let first = out.is_empty();
        let avail = if first { first_width } else { rest_width };
        let hard_end = fill(chars, start, avail);
        let end = if opts.linebreak && hard_end < chars.len() { break_before(chars, start, hard_end) } else { hard_end };
        out.push(Segment { start, end, indent: if first { 0 } else { indent } });
        // Whitespace at a break is consumed by it rather than opening
        // the next row -- otherwise every wrapped line would start with
        // the space it broke at.
        let mut next = end;
        while next < chars.len() && chars[next] == ' ' {
            next += 1;
        }
        // `fill` always advances by at least one character, so this
        // cannot loop.
        start = next.max(end.max(start + 1));
    }
    if out.is_empty() {
        out.push(Segment { start: 0, end: 0, indent: 0 });
    }
    out
}

// The prefix a continued row opens with: `showbreak`, plus the line's
// own leading whitespace when `breakindent` is on. Capped at half the
// pane, so a deeply indented line doesn't wrap into a sliver.
fn continuation_indent(chars: &[char], width: usize, opts: &Options) -> usize {
    let mut indent = opts.showbreak.chars().map(char_width).sum::<usize>();
    if opts.breakindent {
        indent += chars.iter().take_while(|c| **c == ' ' || **c == '\t').count();
    }
    indent.min(width / 2)
}

// How far from `start` fits in `avail` columns -- at least one
// character, however wide it is, so a pane narrower than a glyph still
// makes progress.
fn fill(chars: &[char], start: usize, avail: usize) -> usize {
    let mut used = 0;
    let mut i = start;
    while i < chars.len() {
        let w = char_width(chars[i]);
        if used + w > avail && i > start {
            break;
        }
        used += w;
        i += 1;
    }
    i.max(start + 1).min(chars.len())
}

// The last break opportunity at or before `hard_end`, or `hard_end`
// itself when the row holds no seam at all -- a single word longer than
// the pane still has to be broken somewhere.
//
// Where the seam itself goes differs by kind, and matching vim here
// matters because both look wrong the other way round: a space is
// *consumed* by the break (a row ending in a trailing space reads as a
// stray one), while punctuation stays on the row it ended, so
// `--long-option` breaks after a dash rather than before it.
fn break_before(chars: &[char], start: usize, hard_end: usize) -> usize {
    for i in (start..hard_end).rev() {
        if !is_break_at(chars[i]) {
            continue;
        }
        let end = if chars[i].is_whitespace() { i } else { i + 1 };
        if end > start {
            return end;
        }
    }
    hard_end
}

// Which segment `col` (a char index) falls on, and the row's own index.
pub fn segment_of(segments: &[Segment], col: usize) -> usize {
    segments.iter().rposition(|s| col >= s.start).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(wrap: bool) -> Options {
        Options { wrap, showbreak: String::new(), breakindent: false, linebreak: false, ..Options::default() }
    }

    fn text(chars: &[char], seg: &Segment) -> String {
        chars[seg.start..seg.end].iter().collect()
    }

    fn split(line: &str, width: usize, opts: &Options) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        segments(&chars, width, opts).iter().map(|s| text(&chars, s)).collect()
    }

    #[test]
    fn without_wrapping_a_line_is_one_segment_however_long() {
        assert_eq!(split("a rather long line indeed", 10, &opts(false)), vec!["a rather long line indeed"]);
    }

    #[test]
    fn an_empty_line_is_always_one_empty_segment() {
        assert_eq!(split("", 10, &opts(true)), vec![""]);
        assert_eq!(split("", 10, &opts(false)), vec![""]);
    }

    #[test]
    fn wrapping_fills_each_row_to_the_width() {
        assert_eq!(split("abcdefghij", 4, &opts(true)), vec!["abcd", "efgh", "ij"]);
    }

    // linebreak: break between words, and the space that was broken at
    // belongs to neither row.
    #[test]
    fn linebreak_breaks_between_words() {
        let o = Options { linebreak: true, ..opts(true) };
        assert_eq!(split("the quick brown fox", 10, &o), vec!["the quick", "brown fox"]);
        // Punctuation stays on the row it ended, so an option name
        // breaks after a dash rather than before it.
        assert_eq!(split("--some-long-option here", 12, &o), vec!["--some-long-", "option here"]);
    }

    // ...but a single word longer than the pane still has to break.
    #[test]
    fn a_word_longer_than_the_pane_breaks_anyway() {
        let o = Options { linebreak: true, ..opts(true) };
        assert_eq!(split("supercalifragilistic", 8, &o), vec!["supercal", "ifragili", "stic"]);
    }

    #[test]
    fn showbreak_and_breakindent_narrow_the_continued_rows() {
        let o = Options { showbreak: "> ".to_string(), breakindent: true, ..opts(true) };
        let chars: Vec<char> = "    abcdefghij".chars().collect();
        let segs = segments(&chars, 12, &o);
        assert_eq!(segs[0].indent, 0, "the first row has no prefix");
        // "> " is 2 columns, the line's own indent is 4.
        assert_eq!(segs[1].indent, 6);
        assert_eq!(text(&chars, &segs[0]), "    abcdefgh");
        // 12 columns minus a 6-column prefix leaves 6.
        assert_eq!(text(&chars, &segs[1]), "ij");
    }

    // A pathologically indented line must not wrap into a one-column
    // sliver.
    #[test]
    fn the_continuation_indent_is_capped_at_half_the_pane() {
        let o = Options { showbreak: String::new(), breakindent: true, ..opts(true) };
        let chars: Vec<char> = format!("{}word here", " ".repeat(40)).chars().collect();
        let segs = segments(&chars, 20, &o);
        assert!(segs.iter().skip(1).all(|s| s.indent <= 10), "{segs:?}");
    }

    // Display width, not character count: a double-width glyph must not
    // straddle the edge.
    // Four columns hold two double-width glyphs, or one plus two
    // single-width ones. Counting *characters* instead would put three
    // glyphs on the first row and overflow the pane.
    #[test]
    fn wrapping_measures_display_width() {
        let o = opts(true);
        assert_eq!(split("\u{65e5}\u{672c}\u{8a9e}ab", 4, &o), vec!["\u{65e5}\u{672c}", "\u{8a9e}ab"]);
        // At three columns no two double-width glyphs fit together --
        // character counting would give ["\u{65e5}\u{672c}\u{8a9e}", "ab"].
        assert_eq!(split("\u{65e5}\u{672c}\u{8a9e}ab", 3, &o), vec!["\u{65e5}", "\u{672c}", "\u{8a9e}a", "b"]);
    }

    #[test]
    fn a_glyph_wider_than_the_pane_still_makes_progress() {
        let o = opts(true);
        assert_eq!(split("\u{65e5}\u{672c}", 1, &o), vec!["\u{65e5}", "\u{672c}"]);
    }

    #[test]
    fn wrap_column_narrows_the_text_but_never_past_the_pane() {
        let o = Options { column: 4, ..opts(true) };
        assert_eq!(split("abcdefgh", 20, &o), vec!["abcd", "efgh"]);
        // A pane narrower than the column still wraps at the pane.
        let o = Options { column: 40, ..opts(true) };
        assert_eq!(split("abcdefgh", 4, &o), vec!["abcd", "efgh"]);
    }

    #[test]
    fn every_character_appears_exactly_once_across_the_segments() {
        let o = Options { linebreak: true, showbreak: "-- ".to_string(), breakindent: true, ..opts(true) };
        let line = "  a line with words, punctuation/seams and a verylongunbrokentokenindeed at the end";
        let chars: Vec<char> = line.chars().collect();
        for width in 4..40 {
            let segs = segments(&chars, width, &o);
            // Segments are in order and cover the line, apart from the
            // whitespace a break consumed.
            let mut at = 0;
            let mut seen = String::new();
            for seg in &segs {
                assert!(seg.start >= at, "segments went backwards at width {width}: {segs:?}");
                assert!(seg.end >= seg.start, "inverted segment at width {width}");
                seen.push_str(&text(&chars, seg));
                at = seg.end;
            }
            assert_eq!(at, chars.len(), "segments must reach the end of the line at width {width}");
            let without_spaces: String = line.chars().filter(|c| *c != ' ').collect();
            let seen_without: String = seen.chars().filter(|c| *c != ' ').collect();
            assert_eq!(seen_without, without_spaces, "content lost or duplicated at width {width}");
        }
    }

    #[test]
    fn segment_of_finds_the_row_a_column_is_on() {
        let chars: Vec<char> = "abcdefghij".chars().collect();
        let segs = segments(&chars, 4, &opts(true));
        assert_eq!(segment_of(&segs, 0), 0);
        assert_eq!(segment_of(&segs, 3), 0);
        assert_eq!(segment_of(&segs, 4), 1);
        assert_eq!(segment_of(&segs, 9), 2);
        // Past the end clamps to the last row, which is where the cursor
        // sits at end-of-line.
        assert_eq!(segment_of(&segs, 99), 2);
    }
}
