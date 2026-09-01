use super::grapheme;
use super::Buffer;
use crate::regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Down,
    Up,
    LineStart,          // 0
    LineFirstNonBlank,  // ^
    LineEnd,            // $ (count moves down count-1 lines first)
    LineLastNonBlank,   // g_ (count moves down count-1 lines first)
    GotoColumn,          // | (count is the 1-indexed target column, default 1)
    GotoFirstLine,        // gg (count is the 1-indexed target line, default first)
    GotoLastLine,          // G  (count is the 1-indexed target line, default last)
    GotoPercent,           // {count}% (only emitted when a count precedes '%' -- see vimkeys.rs)
    WordForward,            // w
    WordForwardBig,           // W
    WordBackward,              // b
    WordBackwardBig,            // B
    WordEnd,                     // e
    WordEndBig,                    // E
    WordEndBackward,                // ge
    WordEndBackwardBig,               // gE
    FindChar {
        // f F t T. Repeat via ';'/',' is vimkeys' job (stage 4): it remembers
        // the last FindChar and re-issues it (flipping `forward` for ',').
        ch: char,
        till: bool,
        forward: bool,
    },
    ScreenTop,       // H (count = line offset from the top, default 1)
    ScreenMiddle,    // M (count ignored)
    ScreenBottom,    // L (count = line offset from the bottom, default 1)
    HalfPageDown,    // Ctrl-D
    HalfPageUp,      // Ctrl-U
    PageDown,        // Ctrl-F
    PageUp,          // Ctrl-B
    ScrollLineDown,  // Ctrl-E (count = lines to scroll, default 1)
    ScrollLineUp,    // Ctrl-Y (count = lines to scroll, default 1)
    ScrollCenter,    // zz (count ignored; centers the current line)
    ScrollTop,       // zt (count ignored; current line becomes the top)
    ScrollBottom,    // zb (count ignored; current line becomes the bottom)
    ParagraphForward,  // }
    ParagraphBackward, // {
    SentenceForward,   // )
    SentenceBackward,  // (
    NextLineNonBlank,  // + or Enter
    PrevLineNonBlank,  // -
    MatchPair,         // %
    SetMark(char),     // m{a-z}
    GotoMark(char),    // `{mark}
    GotoMarkLine(char), // '{mark}
    // Repeat via 'n'/'N' is vimkeys' job (stage 4), same as ';'/',' for
    // FindChar: it remembers the last search (the literal string it parsed
    // for '/'/'?', or simply that '*'/'#' was used) and re-issues the same
    // motion, flipping direction for 'N'. For '*'/'#' this works out neatly
    // because the word under the cursor after landing on a match is, by
    // construction, textually identical to the word that was searched for.
    SearchForward(String),  // /pattern<Enter>
    SearchBackward(String), // ?pattern<Enter>
    SearchWordForward,      // *
    SearchWordBackward,     // #
    // `g*`/`g#` -- same "search for the word under the cursor" idea as
    // `*`/`#`, but a plain substring search: no `\<`/`\>` word-boundary
    // requirement, so "foobar" also matches when the word under the
    // cursor is "foo".
    SearchWordForwardUnbounded,
    SearchWordBackwardUnbounded,
    UnmatchedOpenParen,   // [(
    UnmatchedCloseParen,  // ])
    UnmatchedOpenBrace,   // [{
    UnmatchedCloseBrace,  // ]}
    SectionForward,       // ]] (next line starting with '{')
    SectionForwardEnd,    // ][ (next line starting with '}')
    SectionBackward,      // [[ (previous line starting with '{')
    SectionBackwardEnd,   // [] (previous line starting with '}')
    // `iw`/`aw`/`i(`/`a(`/`i"`/`a"`/... -- vim's text objects, valid only as
    // an operator's target (`vimkeys.rs` only ever produces this while an
    // operator is armed -- see its own doc comment on `i`/`a`'s gating).
    // Unlike every other `Motion`, the cursor usually sits *inside* the
    // target range rather than at one of its ends, so `motion_range` can't
    // use its usual "apply the motion, diff the cursor before/after" trick
    // -- it special-cases this variant to call `text_object_range` directly
    // instead. `bool` is `around` (`a{obj}`) vs inner (`i{obj}`).
    TextObject(TextObjectKind, bool),
}

/// The object an `i`/`a` text object names. `b`/`B` are vim's own aliases for
/// `(`/`{` respectively (both map to `Paren`/`Brace`); tag objects (`it`/
/// `at`) are out of scope for this pass -- rare outside markup, and this is
/// a shell editor, not a web one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextObjectKind {
    Word,
    WordBig,
    Sentence,
    Paragraph,
    Paren,
    Brace,
    Bracket,
    Angle,
    DoubleQuote,
    SingleQuote,
    Backtick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

// Whether `c` is part of a word, given the buffer's own `iskeyword`
// answer -- see `Buffer::word_chars`. Letters and digits always are, in
// any script; `extra` says which punctuation joins them.
fn is_word_char(c: char, extra: &str) -> bool {
    c.is_alphanumeric() || extra.contains(c)
}

/// `~`/`gu`/`gU`/`g~`'s own shared notion of "which way to change case" --
/// a non-alphabetic character is always left untouched by every variant
/// (vim's own rule: only letters have a case to change).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseKind {
    Lower,
    Upper,
    Toggle,
}

/// The single-character transform every case command (`~`'s own direct
/// per-character toggle, and `gu{motion}`/`gU{motion}`/`g~{motion}`'s
/// operator versions) is built from. ASCII-only, matching `r`'s own
/// scope -- a full Unicode case fold can expand one character into
/// several (`ß` -> `SS`), which wouldn't fit this "one char in, one char
/// out" shape any of these commands assume; non-ASCII letters simply
/// pass through unchanged.
pub fn case_transform(c: char, kind: CaseKind) -> char {
    match kind {
        CaseKind::Lower => c.to_ascii_lowercase(),
        CaseKind::Upper => c.to_ascii_uppercase(),
        CaseKind::Toggle => {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else if c.is_ascii_lowercase() {
                c.to_ascii_uppercase()
            } else {
                c
            }
        }
    }
}

fn classify(buf: &impl Buffer, line: usize, col: usize, big: bool) -> Class {
    let ch = match buf.char_at(line, col) {
        Some(c) => c,
        None => return Class::Blank,
    };
    if ch.is_whitespace() {
        Class::Blank
    } else if big || is_word_char(ch, buf.word_chars()) {
        Class::Word
    } else {
        Class::Punct
    }
}

// The rightmost column a Normal-mode cursor can ever sit at on `line`
// -- the shared clamp point almost every motion in this file uses
// (Left/Right, Up/Down's own column preservation, GotoLine, page
// scrolling, ...), so fixing it here is what actually makes "the
// cursor never lands mid-grapheme-cluster" hold everywhere at once,
// not just for the motions that call `grapheme::next_boundary`/
// `prev_boundary` directly. If the line's last char is part of a
// multi-char cluster (a trailing ZWJ emoji sequence, say), the cursor's
// own last valid position is that cluster's *start*, not its last raw
// char index -- landing on any char index past the start would put the
// cursor visually inside the glyph.
fn last_col(buf: &impl Buffer, line: usize) -> usize {
    let len = buf.line_len(line);
    if len == 0 {
        return 0;
    }
    let chars = buf.line_chars(line);
    grapheme::prev_boundary(&chars, len)
}

fn first_non_blank(buf: &impl Buffer, line: usize) -> usize {
    for col in 0..buf.line_len(line) {
        if let Some(c) = buf.char_at(line, col) {
            if !c.is_whitespace() {
                return col;
            }
        }
    }
    0
}

fn last_non_blank(buf: &impl Buffer, line: usize) -> usize {
    for col in (0..buf.line_len(line)).rev() {
        if let Some(c) = buf.char_at(line, col) {
            if !c.is_whitespace() {
                return col;
            }
        }
    }
    0
}

/// `$`'s own target row: walks forward past every `line_wraps` row so it
/// always lands on the true end of the logical line, not wherever a
/// column-width autowrap happened to cut it -- for every ordinary buffer
/// (`line_wraps` always false) this is just `line` itself, unchanged.
fn logical_line_end(buf: &impl Buffer, line: usize) -> usize {
    let mut l = line;
    while buf.line_wraps(l) && l + 1 < buf.line_count() {
        l += 1;
    }
    l
}

/// Steps one character forward. Never lands on the virtual "just past the
/// last character" slot of a line -- that slot is skipped straight through
/// to column 0 of the next line, since it isn't a real character and vim's
/// word motions treat the line break itself as the boundary.
fn step_forward(buf: &impl Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
    let (line, col) = pos;
    let len = buf.line_len(line);
    if col + 1 < len {
        Some((line, col + 1))
    } else if line + 1 < buf.line_count() {
        Some((line + 1, 0))
    } else {
        None
    }
}

/// Mirror of `step_forward`: stepping back from column 0 lands on the last
/// real character of the previous line (or column 0 if that line is empty),
/// never on a virtual past-the-end slot.
fn step_backward(buf: &impl Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
    let (line, col) = pos;
    if col > 0 {
        Some((line, col - 1))
    } else if line > 0 {
        let prev_line = line - 1;
        Some((prev_line, last_col(buf, prev_line)))
    } else {
        None
    }
}

fn word_forward_once(buf: &impl Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
    let (line, col) = pos;
    if buf.line_len(line) == 0 {
        return step_forward(buf, pos).unwrap_or(pos);
    }
    let start_class = classify(buf, line, col, big);
    let mut cur = pos;
    if start_class != Class::Blank {
        loop {
            match step_forward(buf, cur) {
                Some(next) => {
                    if next.0 != cur.0 {
                        cur = next;
                        break;
                    }
                    let cls = classify(buf, next.0, next.1, big);
                    cur = next;
                    if cls != start_class {
                        break;
                    }
                }
                None => return cur,
            }
        }
    }
    loop {
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        if classify(buf, cur.0, cur.1, big) != Class::Blank {
            return cur;
        }
        match step_forward(buf, cur) {
            Some(next) => cur = next,
            None => return cur,
        }
    }
}

fn word_backward_once(buf: &impl Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
    let mut cur = match step_backward(buf, pos) {
        Some(p) => p,
        None => return pos,
    };
    loop {
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        let cls = classify(buf, cur.0, cur.1, big);
        if cls != Class::Blank {
            loop {
                match step_backward(buf, cur) {
                    Some(prev) => {
                        if prev.0 != cur.0 {
                            return cur;
                        }
                        let pcls = classify(buf, prev.0, prev.1, big);
                        if pcls == cls {
                            cur = prev;
                        } else {
                            return cur;
                        }
                    }
                    None => return cur,
                }
            }
        }
        match step_backward(buf, cur) {
            Some(prev) => cur = prev,
            None => return cur,
        }
    }
}

fn word_end_forward_once(buf: &impl Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
    let mut cur = match step_forward(buf, pos) {
        Some(p) => p,
        None => return pos,
    };
    loop {
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        let cls = classify(buf, cur.0, cur.1, big);
        if cls != Class::Blank {
            loop {
                match step_forward(buf, cur) {
                    Some(next) => {
                        if next.0 != cur.0 {
                            return cur;
                        }
                        let ncls = classify(buf, next.0, next.1, big);
                        if ncls == cls {
                            cur = next;
                        } else {
                            return cur;
                        }
                    }
                    None => return cur,
                }
            }
        }
        match step_forward(buf, cur) {
            Some(next) => cur = next,
            None => return cur,
        }
    }
}

/// `ge`/`gE`: end of the word before the cursor. Unlike `w`/`b`/`e`, if there
/// is no previous word to find (the cursor is already within the buffer's
/// first word), this leaves the cursor unmoved rather than clamping to the
/// farthest reachable position -- matching vim, which simply fails the
/// motion in that case.
fn word_end_backward_once(buf: &impl Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
    let (oline, ocol) = pos;
    let orig_is_word = buf.line_len(oline) > 0 && classify(buf, oline, ocol, big) != Class::Blank;
    let orig_class = if orig_is_word {
        Some(classify(buf, oline, ocol, big))
    } else {
        None
    };
    let mut cur = pos;
    let mut left_run = !orig_is_word;
    loop {
        let prev = match step_backward(buf, cur) {
            Some(p) => p,
            None => return pos,
        };
        let crossed_line = prev.0 != cur.0;
        cur = prev;
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        let cls = classify(buf, cur.0, cur.1, big);
        if !left_run {
            if crossed_line || Some(cls) != orig_class {
                left_run = true;
            } else {
                continue;
            }
        }
        if cls != Class::Blank {
            return cur;
        }
    }
}

fn find_char_forward_once(buf: &impl Buffer, line: usize, from_col: usize, ch: char) -> Option<usize> {
    ((from_col + 1)..buf.line_len(line)).find(|&c| buf.char_at(line, c) == Some(ch))
}

fn find_char_backward_once(buf: &impl Buffer, line: usize, from_col: usize, ch: char) -> Option<usize> {
    (0..from_col).rev().find(|&c| buf.char_at(line, c) == Some(ch))
}

fn is_sentence_end_punct(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
}

fn is_closing_punct(c: char) -> bool {
    matches!(c, ')' | ']' | '"' | '\'')
}

fn is_blank_line(buf: &impl Buffer, line: usize) -> bool {
    buf.line_len(line) == 0
}

/// `pos` must hold a sentence-ending punctuation character. Returns the
/// position right after it (and any immediately following closing
/// quotes/brackets, same line only) if that position is whitespace,
/// end-of-line, or end-of-buffer -- i.e. if `pos` really does end a sentence
/// -- or `None` if it doesn't (e.g. "3.14", where '.' isn't a sentence end).
fn sentence_boundary_after(buf: &impl Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
    let mut cur = pos;
    loop {
        match step_forward(buf, cur) {
            Some(next) => {
                if next.0 != cur.0 {
                    return Some(next);
                }
                match buf.char_at(next.0, next.1) {
                    Some(c) if is_closing_punct(c) => cur = next,
                    Some(c) if c.is_whitespace() => return Some(next),
                    Some(_) => return None,
                    None => return Some(next),
                }
            }
            None => return Some(cur),
        }
    }
}

fn skip_blank_forward(buf: &impl Buffer, pos: (usize, usize)) -> (usize, usize) {
    let mut cur = pos;
    loop {
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        match buf.char_at(cur.0, cur.1) {
            Some(c) if c.is_whitespace() => match step_forward(buf, cur) {
                Some(next) => cur = next,
                None => return cur,
            },
            _ => return cur,
        }
    }
}

fn sentence_forward_once(buf: &impl Buffer, pos: (usize, usize)) -> (usize, usize) {
    let mut cur = pos;
    loop {
        if buf.line_len(cur.0) == 0 {
            let mut l = cur.0;
            while l + 1 < buf.line_count() && buf.line_len(l + 1) == 0 {
                l += 1;
            }
            if l + 1 < buf.line_count() {
                let target = l + 1;
                return (target, first_non_blank(buf, target));
            }
            return (l, 0);
        }
        let next = match step_forward(buf, cur) {
            Some(n) => n,
            None => return cur,
        };
        let crossed = next.0 != cur.0;
        cur = next;
        if buf.line_len(cur.0) == 0 {
            return cur;
        }
        if crossed {
            continue;
        }
        if let Some(ch) = buf.char_at(cur.0, cur.1) {
            if is_sentence_end_punct(ch) {
                if let Some(after) = sentence_boundary_after(buf, cur) {
                    return skip_blank_forward(buf, after);
                }
            }
        }
    }
}

/// Sentence-start positions, in document order, computed by repeatedly
/// applying `sentence_forward_once` from the first sentence. `(`
/// (sentence-backward) is defined in terms of this list rather than as an
/// independent backward scan -- much easier to keep correct than
/// hand-rolling a mirrored version of the forward algorithm's boundary
/// detection.
fn sentence_starts(buf: &impl Buffer) -> Vec<(usize, usize)> {
    let first = if buf.line_len(0) == 0 {
        (0, 0)
    } else {
        (0, first_non_blank(buf, 0))
    };
    let mut starts = vec![first];
    let mut cur = first;
    loop {
        let next = sentence_forward_once(buf, cur);
        if next == cur {
            break;
        }
        starts.push(next);
        cur = next;
    }
    starts
}

fn pos_lt(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
}

fn sentence_backward_once(buf: &impl Buffer, pos: (usize, usize)) -> (usize, usize) {
    let starts = sentence_starts(buf);
    let mut result = starts[0];
    for s in starts {
        if pos_lt(s, pos) {
            result = s;
        } else {
            break;
        }
    }
    result
}

fn paragraph_forward_once(buf: &impl Buffer, line: usize) -> usize {
    let last = buf.line_count() - 1;
    let mut l = line;
    if !is_blank_line(buf, l) {
        while l < last && !is_blank_line(buf, l + 1) {
            l += 1;
        }
    } else {
        while l < last && is_blank_line(buf, l + 1) {
            l += 1;
        }
        while l < last && !is_blank_line(buf, l + 1) {
            l += 1;
        }
    }
    if l < last {
        l + 1
    } else {
        last
    }
}

fn paragraph_backward_once(buf: &impl Buffer, line: usize) -> usize {
    let mut l = line;
    if l == 0 {
        return 0;
    }
    if !is_blank_line(buf, l) {
        while l > 0 && !is_blank_line(buf, l - 1) {
            l -= 1;
        }
    } else {
        while l > 0 && is_blank_line(buf, l - 1) {
            l -= 1;
        }
        while l > 0 && !is_blank_line(buf, l - 1) {
            l -= 1;
        }
    }
    if l > 0 {
        l - 1
    } else {
        0
    }
}

fn viewport_bottom(buf: &impl Buffer) -> usize {
    (buf.viewport_top() + buf.viewport_height().saturating_sub(1)).min(buf.line_count() - 1)
}

/// For a bracket character, returns (the character that closes/opens the
/// pair, whether `c` itself is the opening bracket).
fn bracket_partner(c: char) -> Option<(char, bool)> {
    match c {
        '(' => Some((')', true)),
        ')' => Some(('(', false)),
        '[' => Some((']', true)),
        ']' => Some(('[', false)),
        '{' => Some(('}', true)),
        '}' => Some(('{', false)),
        _ => None,
    }
}

/// `%`: if the cursor isn't on a bracket, scans forward on the current line
/// only (matching vim) for the first one, then walks forward or backward
/// through the buffer tracking nesting depth to find its partner.
fn match_pair_once(buf: &impl Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
    let (line, col) = pos;
    let len = buf.line_len(line);
    let mut start = None;
    for c in col..len {
        if let Some(ch) = buf.char_at(line, c) {
            if bracket_partner(ch).is_some() {
                start = Some((line, c));
                break;
            }
        }
    }
    let start = start?;
    let ch0 = buf.char_at(start.0, start.1)?;
    let (partner_char, is_opening) = bracket_partner(ch0)?;
    let mut depth = 1;
    let mut cur = start;
    loop {
        cur = if is_opening {
            step_forward(buf, cur)?
        } else {
            step_backward(buf, cur)?
        };
        if let Some(c) = buf.char_at(cur.0, cur.1) {
            if c == ch0 {
                depth += 1;
            } else if c == partner_char {
                depth -= 1;
                if depth == 0 {
                    return Some(cur);
                }
            }
        }
    }
}

/// The word (contiguous run of word chars) at or after `pos`. If `pos`
/// isn't on a word char, advances to the next word first, matching `*`/`#`'s
/// vim behavior of searching from the nearest word forward.
///
/// Public so a caller rendering search-match highlighting can recover the
/// pattern a `*`/`#` search actually used: `SearchWordForward`/
/// `SearchWordBackward`'s own doc comment already notes that the word
/// under the cursor *after* landing on a match is, by construction,
/// textually identical to the word that was searched for -- so there's no
/// need for `vimkeys.rs` to separately track and expose that text itself.
pub fn word_under_cursor(buf: &impl Buffer, pos: (usize, usize)) -> Option<String> {
    let mut p = pos;
    if buf.line_len(p.0) == 0 || !matches!(buf.char_at(p.0, p.1), Some(c) if is_word_char(c, buf.word_chars())) {
        let next = word_forward_once(buf, p, false);
        if next == p {
            return None;
        }
        p = next;
    }
    if buf.line_len(p.0) == 0 {
        return None;
    }
    if !matches!(buf.char_at(p.0, p.1), Some(c) if is_word_char(c, buf.word_chars())) {
        return None;
    }
    let line = p.0;
    let mut start_col = p.1;
    while start_col > 0 && matches!(buf.char_at(line, start_col - 1), Some(c) if is_word_char(c, buf.word_chars())) {
        start_col -= 1;
    }
    let mut end_col = p.1;
    while end_col + 1 < buf.line_len(line)
        && matches!(buf.char_at(line, end_col + 1), Some(c) if is_word_char(c, buf.word_chars()))
    {
        end_col += 1;
    }
    Some((start_col..=end_col).filter_map(|c| buf.char_at(line, c)).collect())
}

/// The chunk (contiguous run of the same `Class`) at `col` on `line`,
/// extended `count - 1` further chunks forward -- the shared core of
/// `iw`/`aw`/`iW`/`aW`: vim defines `[count]iw` as "the word (or
/// punctuation-run, or blank-run -- whichever the cursor sits on) plus the
/// next `count - 1` chunks of *any* class immediately following it".
fn word_object_range(buf: &impl Buffer, pos: (usize, usize), big: bool, around: bool, count: Option<usize>) -> Option<MotionRange> {
    let (line, col) = pos;
    if buf.line_len(line) == 0 {
        return None;
    }
    let col = col.min(last_col(buf, line));
    let last = last_col(buf, line);
    let cls = classify(buf, line, col, big);
    let mut start = col;
    while start > 0 && classify(buf, line, start - 1, big) == cls {
        start -= 1;
    }
    let mut end = col;
    while end < last && classify(buf, line, end + 1, big) == cls {
        end += 1;
    }
    for _ in 1..count.unwrap_or(1).max(1) {
        if end >= last {
            break;
        }
        let next_cls = classify(buf, line, end + 1, big);
        end += 1;
        while end < last && classify(buf, line, end + 1, big) == next_cls {
            end += 1;
        }
    }
    if around {
        if end < last && classify(buf, line, end + 1, big) == Class::Blank {
            end += 1;
            while end < last && classify(buf, line, end + 1, big) == Class::Blank {
                end += 1;
            }
        } else if start > 0 && classify(buf, line, start - 1, big) == Class::Blank {
            start -= 1;
            while start > 0 && classify(buf, line, start - 1, big) == Class::Blank {
                start -= 1;
            }
        }
    }
    Some(MotionRange { shape: MotionShape::Inclusive, from: (line, start), to: (line, end) })
}

/// The raw positions of the two `quote` characters `quote_object_range`
/// (below) would resolve around/inside -- factored out so `ds"`/`cs"` (see
/// `surround_pair_positions`) can act on exactly the delimiter characters
/// themselves, with none of `quote_object_range`'s own around-whitespace
/// swallowing or inner-content trimming.
fn quote_pair_positions(buf: &impl Buffer, pos: (usize, usize), quote: char) -> Option<((usize, usize), (usize, usize))> {
    let (line, col) = pos;
    let len = buf.line_len(line);
    let positions: Vec<usize> = (0..len).filter(|&c| buf.char_at(line, c) == Some(quote)).collect();
    let mut i = 0;
    while i + 1 < positions.len() {
        let (a, b) = (positions[i], positions[i + 1]);
        if col <= b {
            return Some(((line, a), (line, b)));
        }
        i += 2;
    }
    None
}

/// `i"`/`a"`/`i'`/`a'`/`` i` ``/`` a` `` -- same-line only, matching vim
/// (string text objects never cross a line break). Pairs up every
/// occurrence of `quote` on the line left to right (1st+2nd, 3rd+4th, ...)
/// and picks the first pair the cursor is at-or-before the close of --
/// vim's own rule: inside a pair selects that pair, before all pairs
/// selects the first, in the gap between two pairs selects the next one.
fn quote_object_range(buf: &impl Buffer, pos: (usize, usize), quote: char, around: bool) -> Option<MotionRange> {
    let (line, _) = pos;
    let len = buf.line_len(line);
    let ((_, a), (_, b)) = quote_pair_positions(buf, pos, quote)?;
    if around {
        let mut end = b;
        if end < len.saturating_sub(1) && matches!(buf.char_at(line, end + 1), Some(c) if c.is_whitespace()) {
            end += 1;
            while end < len.saturating_sub(1) && matches!(buf.char_at(line, end + 1), Some(c) if c.is_whitespace()) {
                end += 1;
            }
            return Some(MotionRange { shape: MotionShape::Inclusive, from: (line, a), to: (line, end) });
        }
        let mut start = a;
        while start > 0 && matches!(buf.char_at(line, start - 1), Some(c) if c.is_whitespace()) {
            start -= 1;
        }
        Some(MotionRange { shape: MotionShape::Inclusive, from: (line, start), to: (line, end) })
    } else if b > a + 1 {
        Some(MotionRange { shape: MotionShape::Inclusive, from: (line, a + 1), to: (line, b - 1) })
    } else {
        // Adjacent quotes ("") have nothing between them -- matches vim's
        // own `di"` no-op on an empty string.
        None
    }
}

/// Walks forward from just after `open_pos` (already known to hold `open`),
/// tracking nesting depth, to `open_pos`'s matching `close`.
fn scan_matching_forward(buf: &impl Buffer, open_pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
    let mut depth = 1;
    let mut cur = open_pos;
    loop {
        cur = step_forward(buf, cur)?;
        match buf.char_at(cur.0, cur.1) {
            Some(c) if c == open => depth += 1,
            Some(c) if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(cur);
                }
            }
            _ => {}
        }
    }
}

/// Mirror of `scan_matching_forward`: walks backward from just before
/// `close_pos` to its matching `open`.
fn scan_matching_backward(buf: &impl Buffer, close_pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
    let mut depth = 1;
    let mut cur = close_pos;
    loop {
        cur = step_backward(buf, cur)?;
        match buf.char_at(cur.0, cur.1) {
            Some(c) if c == close => depth += 1,
            Some(c) if c == open => {
                depth -= 1;
                if depth == 0 {
                    return Some(cur);
                }
            }
            _ => {}
        }
    }
}

/// Walks backward from `pos` (not itself on `open`/`close`) for the nearest
/// `open` that isn't already balanced by a `close` seen along the way --
/// i.e. the bracket that innermost-encloses `pos`.
fn scan_unmatched_open_backward(buf: &impl Buffer, pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
    let mut depth = 0;
    let mut cur = pos;
    loop {
        cur = step_backward(buf, cur)?;
        match buf.char_at(cur.0, cur.1) {
            Some(c) if c == close => depth += 1,
            Some(c) if c == open => {
                if depth == 0 {
                    return Some(cur);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
}

/// The bracket pair that innermost-encloses `pos` -- `pos` sitting exactly
/// on `open` or `close` counts as enclosed by that pair (matching vim: `%`
/// on either paren already works the same way).
fn find_enclosing_pair(buf: &impl Buffer, pos: (usize, usize), open: char, close: char) -> Option<((usize, usize), (usize, usize))> {
    if let Some(c) = buf.char_at(pos.0, pos.1) {
        if c == open {
            return scan_matching_forward(buf, pos, open, close).map(|close_pos| (pos, close_pos));
        }
        if c == close {
            return scan_matching_backward(buf, pos, open, close).map(|open_pos| (open_pos, pos));
        }
    }
    let open_pos = scan_unmatched_open_backward(buf, pos, open, close)?;
    let close_pos = scan_matching_forward(buf, open_pos, open, close)?;
    Some((open_pos, close_pos))
}

/// Mirror of `scan_unmatched_open_backward`: walks forward from `pos` for
/// the nearest `close` that isn't already balanced by an `open` seen along
/// the way -- `[(`/`[{`'s own forward-facing sibling, `])`/`]}`.
fn scan_unmatched_close_forward(buf: &impl Buffer, pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
    let mut depth = 0;
    let mut cur = pos;
    loop {
        cur = step_forward(buf, cur)?;
        match buf.char_at(cur.0, cur.1) {
            Some(c) if c == open => depth += 1,
            Some(c) if c == close => {
                if depth == 0 {
                    return Some(cur);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
}

/// `[(`/`[{`: `count` unmatched opening brackets backward from the cursor.
/// Each repeat starts strictly before the previous find (`scan_unmatched_
/// open_backward` never looks at `pos` itself, so re-feeding the same
/// position back in naturally continues past it).
fn unmatched_backward(buf: &impl Buffer, pos: (usize, usize), open: char, close: char, count: usize) -> Option<(usize, usize)> {
    let mut cur = pos;
    let mut found = None;
    for _ in 0..count.max(1) {
        cur = scan_unmatched_open_backward(buf, cur, open, close)?;
        found = Some(cur);
    }
    found
}

/// `])`/`]}`: `count` unmatched closing brackets forward from the cursor.
fn unmatched_forward(buf: &impl Buffer, pos: (usize, usize), open: char, close: char, count: usize) -> Option<(usize, usize)> {
    let mut cur = pos;
    let mut found = None;
    for _ in 0..count.max(1) {
        cur = scan_unmatched_close_forward(buf, cur, open, close)?;
        found = Some(cur);
    }
    found
}

/// A line whose very first character is `ch` -- vim's own (language-
/// agnostic) definition of a section boundary: `{` starts one, `}` ends
/// one. No 'sections' option, no form-feed handling -- just the plain-text
/// default.
fn line_starts_with(buf: &impl Buffer, line: usize, ch: char) -> bool {
    buf.line_len(line) > 0 && buf.char_at(line, 0) == Some(ch)
}

/// `]]`/`][`: the next line (after the cursor's own) starting with `boundary`,
/// clamped to the buffer's last line if there isn't one -- same clamp-at-
/// the-edge convention `paragraph_forward_once` already uses.
fn section_forward_once(buf: &impl Buffer, line: usize, boundary: char) -> usize {
    let last = buf.line_count() - 1;
    let mut l = line;
    while l < last {
        l += 1;
        if line_starts_with(buf, l, boundary) {
            return l;
        }
    }
    last
}

/// Mirror of `section_forward_once`, searching backward and clamping to 0.
fn section_backward_once(buf: &impl Buffer, line: usize, boundary: char) -> usize {
    let mut l = line;
    while l > 0 {
        l -= 1;
        if line_starts_with(buf, l, boundary) {
            return l;
        }
    }
    0
}

/// `i(`/`a(`/`ib`/`ab`/`i{`/`a{`/... -- spans lines (unlike word/quote
/// objects), tracking nesting via `find_enclosing_pair`. `count` widens
/// outward one enclosing level per extra count, same as vim's `2i(` inside
/// nested parens.
fn pair_object_range(buf: &impl Buffer, pos: (usize, usize), open: char, close: char, around: bool, count: Option<usize>) -> Option<MotionRange> {
    let n = count.unwrap_or(1).max(1);
    let mut cur_pos = pos;
    let mut pair = None;
    for i in 0..n {
        let found = find_enclosing_pair(buf, cur_pos, open, close)?;
        pair = Some(found);
        if i + 1 < n {
            cur_pos = step_backward(buf, found.0)?;
        }
    }
    let (open_pos, close_pos) = pair?;
    if around {
        Some(MotionRange { shape: MotionShape::Inclusive, from: open_pos, to: close_pos })
    } else {
        let inner_start = step_forward(buf, open_pos)?;
        let inner_end = step_backward(buf, close_pos)?;
        if pos_lt(inner_end, inner_start) {
            // Adjacent brackets ("()") have nothing between them -- matches
            // vim's own `di(` no-op there, same as the empty-quote case.
            None
        } else {
            Some(MotionRange { shape: MotionShape::Inclusive, from: inner_start, to: inner_end })
        }
    }
}

/// `ip`/`ap` -- the run of contiguous lines with the same "blank or not" as
/// the cursor's own line, `count - 1` more such runs extended forward.
/// `around` additionally swallows one trailing blank-line run (or, only if
/// there's none to swallow -- e.g. the buffer's last paragraph -- a leading
/// one instead), matching vim's own "prefer trailing, fall back to
/// leading" rule (same shape as `word_object_range`'s own `around` logic).
fn paragraph_object_range(buf: &impl Buffer, pos: (usize, usize), around: bool, count: Option<usize>) -> Option<MotionRange> {
    let (line, _) = pos;
    let last = buf.line_count() - 1;
    let is_blank = is_blank_line(buf, line);
    let mut start = line;
    while start > 0 && is_blank_line(buf, start - 1) == is_blank {
        start -= 1;
    }
    let mut end = line;
    while end < last && is_blank_line(buf, end + 1) == is_blank {
        end += 1;
    }
    for _ in 1..count.unwrap_or(1).max(1) {
        if end >= last {
            break;
        }
        let next_is_blank = is_blank_line(buf, end + 1);
        end += 1;
        while end < last && is_blank_line(buf, end + 1) == next_is_blank {
            end += 1;
        }
    }
    if around && !is_blank {
        if end < last && is_blank_line(buf, end + 1) {
            end += 1;
            while end < last && is_blank_line(buf, end + 1) {
                end += 1;
            }
        } else if start > 0 && is_blank_line(buf, start - 1) {
            start -= 1;
            while start > 0 && is_blank_line(buf, start - 1) {
                start -= 1;
            }
        }
    }
    Some(MotionRange { shape: MotionShape::Linewise, from: (start, 0), to: (end, 0) })
}

/// `is`/`as` -- the sentence (per `sentence_starts`'s own boundary rules)
/// containing the cursor, `count - 1` more sentences extended forward.
/// `around` includes the whitespace run up to the next sentence's start (or
/// to the end of the buffer, for the last sentence); inner trims that back
/// off, stopping at the last non-blank character.
fn sentence_object_range(buf: &impl Buffer, pos: (usize, usize), around: bool, count: Option<usize>) -> Option<MotionRange> {
    let starts = sentence_starts(buf);
    let mut idx = 0;
    for (i, s) in starts.iter().enumerate() {
        if !pos_lt(pos, *s) {
            idx = i;
        } else {
            break;
        }
    }
    let start = starts[idx];
    let buf_end = {
        let last_line = buf.line_count() - 1;
        (last_line, last_col(buf, last_line))
    };
    let end_idx = (idx + count.unwrap_or(1).max(1) - 1).min(starts.len() - 1);
    // `sentence_starts` can end with a phantom "start" sitting exactly on
    // the buffer's very last character -- `sentence_forward_once` clamps
    // there (see its own test's "clamped on the last '.'" case) rather
    // than reporting "no more sentences" the way it does mid-buffer, so
    // that clamp lands in the starts list looking like a genuine next
    // sentence. It never is one (a sentence can't both end and begin on
    // the same single character), so it's treated as "no next sentence"
    // here rather than truncating this object's own last sentence short.
    let next_start = starts.get(end_idx + 1).copied().filter(|&s| s != buf_end);
    let around_end = next_start.and_then(|ns| step_backward(buf, ns)).unwrap_or(buf_end);
    if pos_lt(around_end, start) {
        return None;
    }
    if around {
        return Some(MotionRange { shape: MotionShape::Inclusive, from: start, to: around_end });
    }
    let mut end = around_end;
    while pos_lt(start, end) {
        match buf.char_at(end.0, end.1) {
            Some(c) if c.is_whitespace() => end = step_backward(buf, end).unwrap_or(start),
            _ => break,
        }
    }
    Some(MotionRange { shape: MotionShape::Inclusive, from: start, to: end })
}

/// Resolves a text object (`iw`/`aw`/`i(`/`a(`/...) against the cursor's
/// current position -- unlike `motion_range`'s usual motions, this doesn't
/// move anything itself; see `Motion::TextObject`'s own doc comment for why
/// `motion_range` special-cases this variant instead of going through
/// `apply_motion`.
pub fn text_object_range(buf: &impl Buffer, kind: TextObjectKind, around: bool, count: Option<usize>) -> Option<MotionRange> {
    let pos = buf.cursor();
    match kind {
        TextObjectKind::Word => word_object_range(buf, pos, false, around, count),
        TextObjectKind::WordBig => word_object_range(buf, pos, true, around, count),
        TextObjectKind::Sentence => sentence_object_range(buf, pos, around, count),
        TextObjectKind::Paragraph => paragraph_object_range(buf, pos, around, count),
        TextObjectKind::Paren => pair_object_range(buf, pos, '(', ')', around, count),
        TextObjectKind::Brace => pair_object_range(buf, pos, '{', '}', around, count),
        TextObjectKind::Bracket => pair_object_range(buf, pos, '[', ']', around, count),
        TextObjectKind::Angle => pair_object_range(buf, pos, '<', '>', around, count),
        TextObjectKind::DoubleQuote => quote_object_range(buf, pos, '"', around),
        TextObjectKind::SingleQuote => quote_object_range(buf, pos, '\'', around),
        TextObjectKind::Backtick => quote_object_range(buf, pos, '`', around),
    }
}

/// vim-surround's own `ds`/`cs` target character -- which pair-shaped
/// `TextObjectKind` it names, restricted to the kinds that actually have a
/// pair of single-character delimiters to search for (`w`/`s`/`p` aren't
/// valid `ds`/`cs` targets in the real plugin either). `b`/`B`/`r` are its
/// own mnemonic aliases for `(`/`{`/`[` respectively; either half of a pair
/// (`(` or `)`) names the same kind, matching how `find_enclosing_pair`
/// itself doesn't care which side of a pair `pos` is searched from.
pub fn surround_target_kind(ch: char) -> Option<TextObjectKind> {
    Some(match ch {
        '(' | ')' | 'b' => TextObjectKind::Paren,
        '{' | '}' | 'B' => TextObjectKind::Brace,
        '[' | ']' | 'r' => TextObjectKind::Bracket,
        '<' | '>' => TextObjectKind::Angle,
        '"' => TextObjectKind::DoubleQuote,
        '\'' => TextObjectKind::SingleQuote,
        '`' => TextObjectKind::Backtick,
        _ => return None,
    })
}

/// The two delimiter strings vim-surround inserts for a given trigger
/// character -- `ys`/`cs`'s own "which character did you press" half.
/// Pressing an *opening* bracket (`(`/`{`/`[`/`<`, or their `b`/`B`/`r`
/// mnemonics) pads its inner side with a space (`ysiw(` -> `( word )`);
/// its own closing counterpart inserts tight (`ysiw)` -> `(word)`), and
/// quotes/backtick never pad either way. Any other printable, non-
/// alphanumeric character (`*`, `_`, `=`, ...) is its own literal,
/// unpadded pair -- vim-surround's own generic fallback for a delimiter
/// with no dedicated meaning. Letters beyond `b`/`B`/`r` are deliberately
/// excluded from that fallback: unlike real vim-surround (which allows
/// any single character), reserving them avoids a typo in an unsupported
/// tag/function-call surround (`t`, `f`, ...) silently succeeding as a
/// literal-letter wrap instead of being rejected.
pub fn surround_delims(ch: char) -> Option<(String, String)> {
    Some(match ch {
        '(' | 'b' => ("( ".to_string(), " )".to_string()),
        ')' => ("(".to_string(), ")".to_string()),
        '{' | 'B' => ("{ ".to_string(), " }".to_string()),
        '}' => ("{".to_string(), "}".to_string()),
        '[' | 'r' => ("[ ".to_string(), " ]".to_string()),
        ']' => ("[".to_string(), "]".to_string()),
        '<' => ("< ".to_string(), " >".to_string()),
        '>' => ("<".to_string(), ">".to_string()),
        '"' => ("\"".to_string(), "\"".to_string()),
        '\'' => ("'".to_string(), "'".to_string()),
        '`' => ("`".to_string(), "`".to_string()),
        c if c.is_ascii_graphic() && !c.is_ascii_alphanumeric() => (c.to_string(), c.to_string()),
        _ => return None,
    })
}

/// The raw `(open_pos, close_pos)` of the delimiter pair `ds`/`cs` should
/// act on for a given target kind, found relative to the buffer's own
/// current cursor -- `find_enclosing_pair` for the four bracket kinds
/// (already nesting-aware and multi-line), `quote_pair_positions` for the
/// three quote kinds (same-line only, matching vim). `Word`/`WordBig`/
/// `Sentence`/`Paragraph` never reach here (see `surround_target_kind`'s
/// own doc comment on why those characters never resolve to a kind at
/// all), but are listed explicitly rather than via a wildcard so this
/// stays exhaustive if `TextObjectKind` ever grows a new pair-like kind.
pub fn surround_pair_positions(buf: &impl Buffer, kind: TextObjectKind) -> Option<((usize, usize), (usize, usize))> {
    let pos = buf.cursor();
    match kind {
        TextObjectKind::Paren => find_enclosing_pair(buf, pos, '(', ')'),
        TextObjectKind::Brace => find_enclosing_pair(buf, pos, '{', '}'),
        TextObjectKind::Bracket => find_enclosing_pair(buf, pos, '[', ']'),
        TextObjectKind::Angle => find_enclosing_pair(buf, pos, '<', '>'),
        TextObjectKind::DoubleQuote => quote_pair_positions(buf, pos, '"'),
        TextObjectKind::SingleQuote => quote_pair_positions(buf, pos, '\''),
        TextObjectKind::Backtick => quote_pair_positions(buf, pos, '`'),
        TextObjectKind::Word | TextObjectKind::WordBig | TextObjectKind::Sentence | TextObjectKind::Paragraph => None,
    }
}

/// `ds{ch}`'s own deletion span for each side of a found pair -- just the
/// delimiter character itself, except for a bracket kind (`(`/`{`/`[`/`<`),
/// where vim-surround also strips one immediately-adjacent space on each
/// side (its own "probably padding `ys` added" convention -- quotes never
/// pad, so this never extends for them). Returns `(open_range, close_range)`
/// as two single-position `MotionRange`s, ready for two `delete_range`
/// calls -- close side first, so removing it can never shift the open
/// side's own position. The one-char-of-content-is-just-a-shared-pad-space
/// case (`( )`) is guarded so both sides don't independently claim the
/// same position: the close side wins it, the open side falls back to
/// just its own bracket.
pub fn surround_delete_spans(buf: &impl Buffer, kind: TextObjectKind, open_pos: (usize, usize), close_pos: (usize, usize)) -> (MotionRange, MotionRange) {
    let pads = matches!(kind, TextObjectKind::Paren | TextObjectKind::Brace | TextObjectKind::Bracket | TextObjectKind::Angle);
    let mut open_to = open_pos;
    let mut close_from = close_pos;
    if pads {
        if let Some(after_open) = step_forward(buf, open_pos)
            && after_open != close_pos
            && buf.char_at(after_open.0, after_open.1) == Some(' ')
        {
            open_to = after_open;
        }
        if let Some(before_close) = step_backward(buf, close_pos)
            && before_close != open_pos
            && buf.char_at(before_close.0, before_close.1) == Some(' ')
        {
            close_from = before_close;
        }
        if open_to == close_from {
            open_to = open_pos;
        }
    }
    (
        MotionRange { shape: MotionShape::Inclusive, from: open_pos, to: open_to },
        MotionRange { shape: MotionShape::Inclusive, from: close_from, to: close_pos },
    )
}

/// Where `ys`/`yss`/Visual-`S` (`surround_delims`) inserts each half of a
/// new surround pair around `range` -- the close delimiter's own position
/// first (inserting there can never shift `from`, so callers are free to
/// apply both insertions close-then-open with no further adjustment,
/// regardless of shape). `Linewise` follows `yss`'s own definition (see
/// `KeyOutcome::AddSurround`'s doc comment): first non-blank of the first
/// line through the true end of the last line, ignoring leading
/// indentation the way vim-surround's own linewise wrap does.
pub fn surround_insert_points(buf: &impl Buffer, range: &MotionRange) -> ((usize, usize), (usize, usize)) {
    match range.shape {
        MotionShape::Linewise => {
            let open_at = (range.from.0, first_non_blank(buf, range.from.0));
            let close_at = (range.to.0, buf.line_len(range.to.0));
            (open_at, close_at)
        }
        // A block has no surround of its own in vim either; treated as
        // the inclusive span between its corners, which is what a
        // one-row block already is.
        MotionShape::Inclusive | MotionShape::Blockwise => (range.from, (range.to.0, range.to.1 + 1)),
        MotionShape::Exclusive => (range.from, range.to),
    }
}

/// `Ctrl-A`/`Ctrl-X`'s own target: a decimal number found at or after the
/// cursor on the current line -- `from` includes a leading `-` sign if
/// present (`to` is always the last digit, inclusive), `value` is the
/// parsed number, `width` is how many digit characters it has (not
/// counting the sign). `apply_number_delta`'s own zero-padding only
/// kicks in when `leading_zero` is set (the original text itself started
/// with `0`, e.g. `007`) -- `width` alone isn't enough: an ordinary
/// number like `42` also has a width (2), but vim never pads `42 - 42`
/// back out to `00`, only a number that was genuinely zero-padded to
/// begin with keeps that padding.
pub struct NumberMatch {
    pub from: (usize, usize),
    pub to: (usize, usize),
    pub value: i64,
    pub width: usize,
    pub leading_zero: bool,
}

/// Scans forward from `pos` along its own line for the first digit
/// (`pos` itself counts, so a cursor already inside a number finds that
/// same number rather than skipping to the next one), then expands both
/// directions to the number's full extent -- vim's own `Ctrl-A`/`Ctrl-X`
/// targeting rule. Decimal only (no hex/octal `nrformats` support) --
/// deliberately simplified scope, matching `r`'s/`case_transform`'s own
/// ASCII-only precedent elsewhere in this file. `None` if there's no
/// digit anywhere at or after `pos` on this line, or if the number
/// somehow doesn't fit in an `i64` (astronomically unlikely for anything
/// actually typed).
pub fn find_number(buf: &impl Buffer, pos: (usize, usize)) -> Option<NumberMatch> {
    let (line, col) = pos;
    let len = buf.line_len(line);
    let mut start = (col..len).find(|&c| matches!(buf.char_at(line, c), Some(d) if d.is_ascii_digit()))?;
    while start > 0 && matches!(buf.char_at(line, start - 1), Some(d) if d.is_ascii_digit()) {
        start -= 1;
    }
    let negative = start > 0 && buf.char_at(line, start - 1) == Some('-');
    let sign_col = if negative { start - 1 } else { start };
    let mut end = start;
    while end + 1 < len && matches!(buf.char_at(line, end + 1), Some(d) if d.is_ascii_digit()) {
        end += 1;
    }
    let width = end - start + 1;
    let digits: String = (start..=end).filter_map(|c| buf.char_at(line, c)).collect();
    let leading_zero = width > 1 && digits.starts_with('0');
    let magnitude: i64 = digits.parse().ok()?;
    let value = if negative { -magnitude } else { magnitude };
    Some(NumberMatch { from: (line, sign_col), to: (line, end), value, width, leading_zero })
}

/// The replacement text for `m` after adding `delta` to its value --
/// zero-padded back up to `m`'s own original digit width if it was
/// genuinely zero-padded to begin with (`m.leading_zero`) and the result
/// needs fewer digits than that, and re-signed if it crossed zero.
pub fn apply_number_delta(m: &NumberMatch, delta: i64) -> String {
    let new_value = m.value.saturating_add(delta);
    let digits = new_value.unsigned_abs().to_string();
    let padded = if m.leading_zero && digits.len() < m.width { format!("{}{digits}", "0".repeat(m.width - digits.len())) } else { digits };
    if new_value < 0 {
        format!("-{padded}")
    } else {
        padded
    }
}

/// ERE search (via `crate::regex`) for `/`/`?` -- and, via an
/// escaped-literal pattern (see `search_forward_once`'s other callers),
/// for the plain-text word searches (`*`/`#`/`g*`/`g#`) too, so there's
/// only one matching engine for "search" in normal mode. Matches never
/// span line breaks.
fn line_find(buf: &impl Buffer, line: usize, lower_bound: usize, re: &Regex) -> Option<usize> {
    let chars = buf.line_chars(line);
    re.find_at(&chars, lower_bound).map(|(start, _end)| start)
}

/// Every non-overlapping occurrence of `pattern` on `line`, left to right --
/// the same convention vim's own `hlsearch` uses (a match's own end is
/// where the search for the next one starts, so "aa" against "aaaa" finds
/// cols 0 and 2, not 0/1/2). For search-match *highlighting* -- unrelated
/// to, and doesn't share any state with, a live search's own cursor
/// position via search_forward_once/search_backward_once above.
pub fn find_matches_in_line(buf: &impl Buffer, line: usize, pattern: &str) -> Vec<(usize, usize)> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let re = Regex::compile(pattern, buf.search_ignore_case(pattern));
    let chars = buf.line_chars(line);
    let mut matches = Vec::new();
    let mut from = 0;
    while let Some((start, end)) = re.find_at(&chars, from) {
        matches.push((start, end));
        from = end.max(start + 1); // guard against looping forever on a zero-width match
    }
    matches
}

fn line_rfind(buf: &impl Buffer, line: usize, upper_bound: usize, re: &Regex) -> Option<usize> {
    let chars = buf.line_chars(line);
    let upper = upper_bound.min(chars.len());
    (0..upper).rev().find(|&start| re.match_at(&chars, start).is_some())
}

/// Wrapping forward search (matches vim's default 'wrapscan'): tries the
/// rest of the current line, then every subsequent line, then wraps back
/// around to the start of the original line.
fn search_forward_once(buf: &impl Buffer, pos: (usize, usize), re: &Regex) -> Option<(usize, usize)> {
    let total = buf.line_count();
    if let Some(c) = line_find(buf, pos.0, pos.1 + 1, re) {
        return Some((pos.0, c));
    }
    for offset in 1..total {
        let line = (pos.0 + offset) % total;
        if let Some(c) = line_find(buf, line, 0, re) {
            return Some((line, c));
        }
    }
    line_find(buf, pos.0, 0, re).map(|c| (pos.0, c))
}

fn search_backward_once(buf: &impl Buffer, pos: (usize, usize), re: &Regex) -> Option<(usize, usize)> {
    let total = buf.line_count();
    if let Some(c) = line_rfind(buf, pos.0, pos.1, re) {
        return Some((pos.0, c));
    }
    for offset in 1..total {
        let line = (pos.0 + total - offset) % total;
        if let Some(c) = line_rfind(buf, line, usize::MAX, re) {
            return Some((line, c));
        }
    }
    line_rfind(buf, pos.0, usize::MAX, re).map(|c| (pos.0, c))
}

fn is_word_boundary_at(buf: &impl Buffer, line: usize, col: usize) -> bool {
    !matches!(buf.char_at(line, col), Some(c) if is_word_char(c, buf.word_chars()))
}

/// `*`/`#`'s own word-boundary-respecting wrapper around `search_forward_
/// once`/`search_backward_once` -- `re` is always compiled from an
/// *escaped* literal word (see the `Motion::SearchWordForward` arm below),
/// so `word_len` (its char count) doubles as the matched text's length for
/// the boundary check on either side.
fn search_word_forward_once(buf: &impl Buffer, pos: (usize, usize), re: &Regex, word_len: usize) -> Option<(usize, usize)> {
    let first = search_forward_once(buf, pos, re)?;
    let mut candidate = first;
    loop {
        let (l, c) = candidate;
        let before_ok = c == 0 || is_word_boundary_at(buf, l, c - 1);
        let after_ok = is_word_boundary_at(buf, l, c + word_len);
        if before_ok && after_ok {
            return Some(candidate);
        }
        let next = search_forward_once(buf, candidate, re)?;
        if next == first {
            return None;
        }
        candidate = next;
    }
}

fn search_word_backward_once(buf: &impl Buffer, pos: (usize, usize), re: &Regex, word_len: usize) -> Option<(usize, usize)> {
    let first = search_backward_once(buf, pos, re)?;
    let mut candidate = first;
    loop {
        let (l, c) = candidate;
        let before_ok = c == 0 || is_word_boundary_at(buf, l, c - 1);
        let after_ok = is_word_boundary_at(buf, l, c + word_len);
        if before_ok && after_ok {
            return Some(candidate);
        }
        let next = search_backward_once(buf, candidate, re)?;
        if next == first {
            return None;
        }
        candidate = next;
    }
}

/// Applies a single motion to `buf`'s cursor. `count` is the raw count typed
/// before the motion (`None` if the user typed no digits) -- most motions
/// treat it as a repeat count defaulting to 1, but `GotoFirstLine`/
/// `GotoLastLine`/`GotoColumn` treat it as an explicit target, so `None` and
/// `Some(1)` are not interchangeable for those.
pub fn apply_motion(buf: &mut impl Buffer, motion: Motion, count: Option<usize>) {
    let n = count.unwrap_or(1).max(1);
    match motion {
        // Steps by whole grapheme cluster, `n` times, rather than raw
        // char index -- a multi-codepoint cluster (a ZWJ emoji
        // sequence, a base char plus combining marks) is one glyph on
        // screen, and `h`/`l` should cross it in a single press each
        // way, never stopping mid-cluster. `prev_boundary`/
        // `next_boundary` are themselves already idempotent at the
        // line's own start/end (they clamp, don't panic or wrap), so
        // repeating them `n` times naturally saturates there too, same
        // as the old `saturating_sub`/`.min(max_col)` did.
        Motion::Left => {
            let (line, col) = buf.cursor();
            let chars = buf.line_chars(line);
            let mut new_col = col;
            for _ in 0..n {
                new_col = grapheme::prev_boundary(&chars, new_col);
            }
            buf.set_cursor(line, new_col);
        }
        Motion::Right => {
            let (line, col) = buf.cursor();
            let chars = buf.line_chars(line);
            let max_col = last_col(buf, line);
            let mut new_col = col;
            for _ in 0..n {
                new_col = grapheme::next_boundary(&chars, new_col).min(max_col);
            }
            buf.set_cursor(line, new_col);
        }
        Motion::Down => {
            let (line, col) = buf.cursor();
            let new_line = (line + n).min(buf.line_count().saturating_sub(1));
            let new_col = col.min(last_col(buf, new_line));
            buf.set_cursor(new_line, new_col);
        }
        Motion::Up => {
            let (line, col) = buf.cursor();
            let new_line = line.saturating_sub(n);
            let new_col = col.min(last_col(buf, new_line));
            buf.set_cursor(new_line, new_col);
        }
        Motion::LineStart => {
            let (line, _) = buf.cursor();
            buf.set_cursor(line, 0);
        }
        Motion::LineFirstNonBlank => {
            let (line, _) = buf.cursor();
            let col = first_non_blank(buf, line);
            buf.set_cursor(line, col);
        }
        Motion::LineEnd => {
            let (line, _) = buf.cursor();
            let target = logical_line_end(buf, (line + n - 1).min(buf.line_count().saturating_sub(1)));
            buf.set_cursor(target, last_col(buf, target));
        }
        Motion::LineLastNonBlank => {
            let (line, _) = buf.cursor();
            let target = logical_line_end(buf, (line + n - 1).min(buf.line_count().saturating_sub(1)));
            let col = last_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::GotoColumn => {
            let (line, _) = buf.cursor();
            let col = (n - 1).min(last_col(buf, line));
            buf.set_cursor(line, col);
        }
        Motion::GotoFirstLine => {
            let target = count
                .map(|c| c.saturating_sub(1))
                .unwrap_or(0)
                .min(buf.line_count().saturating_sub(1));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::GotoLastLine => {
            let target = count
                .map(|c| c.saturating_sub(1))
                .unwrap_or_else(|| buf.line_count().saturating_sub(1))
                .min(buf.line_count().saturating_sub(1));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        // `{count}%` -- only ever emitted with a count (see vimkeys.rs's
        // own `%` handler); `count.unwrap_or(0)` below just keeps this
        // total rather than mattering in practice. Vim's own formula:
        // `([count] * lines + 99) / 100`, 1-indexed.
        Motion::GotoPercent => {
            let total = buf.line_count();
            let target = count
                .unwrap_or(0)
                .saturating_mul(total)
                .saturating_add(99)
                / 100;
            let target = target.saturating_sub(1).min(total.saturating_sub(1));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::WordForward | Motion::WordForwardBig => {
            let big = motion == Motion::WordForwardBig;
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = word_forward_once(buf, pos, big);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::WordBackward | Motion::WordBackwardBig => {
            let big = motion == Motion::WordBackwardBig;
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = word_backward_once(buf, pos, big);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::WordEnd | Motion::WordEndBig => {
            let big = motion == Motion::WordEndBig;
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = word_end_forward_once(buf, pos, big);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::WordEndBackward | Motion::WordEndBackwardBig => {
            let big = motion == Motion::WordEndBackwardBig;
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = word_end_backward_once(buf, pos, big);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::FindChar { ch, till, forward } => {
            let (line, col) = buf.cursor();
            let mut cur = col;
            let mut found = None;
            for _ in 0..n {
                let next = if forward {
                    find_char_forward_once(buf, line, cur, ch)
                } else {
                    find_char_backward_once(buf, line, cur, ch)
                };
                match next {
                    Some(c) => {
                        cur = c;
                        found = Some(c);
                    }
                    None => {
                        found = None;
                        break;
                    }
                }
            }
            if let Some(c) = found {
                let target = if till {
                    if forward {
                        c.saturating_sub(1)
                    } else {
                        c + 1
                    }
                } else {
                    c
                };
                buf.set_cursor(line, target);
            }
        }
        Motion::ScreenTop => {
            let target = (buf.viewport_top() + n - 1).min(viewport_bottom(buf));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::ScreenMiddle => {
            let target = (buf.viewport_top() + viewport_bottom(buf)) / 2;
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::ScreenBottom => {
            let bottom = viewport_bottom(buf);
            let target = bottom.saturating_sub(n - 1).max(buf.viewport_top());
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::HalfPageDown => {
            let shift = (buf.viewport_height() / 2).max(1) * n;
            let (line, col) = buf.cursor();
            let new_line = (line + shift).min(buf.line_count() - 1);
            let new_top = (buf.viewport_top() + shift).min(buf.line_count() - 1);
            buf.set_viewport_top(new_top);
            buf.set_cursor(new_line, col.min(last_col(buf, new_line)));
        }
        Motion::HalfPageUp => {
            let shift = (buf.viewport_height() / 2).max(1) * n;
            let (line, col) = buf.cursor();
            let new_line = line.saturating_sub(shift);
            let new_top = buf.viewport_top().saturating_sub(shift);
            buf.set_viewport_top(new_top);
            buf.set_cursor(new_line, col.min(last_col(buf, new_line)));
        }
        Motion::PageDown => {
            let shift = buf.viewport_height().saturating_sub(2).max(1) * n;
            let new_top = (buf.viewport_top() + shift).min(buf.line_count() - 1);
            buf.set_viewport_top(new_top);
            let col = first_non_blank(buf, new_top);
            buf.set_cursor(new_top, col);
        }
        Motion::PageUp => {
            let shift = buf.viewport_height().saturating_sub(2).max(1) * n;
            let new_top = buf.viewport_top().saturating_sub(shift);
            buf.set_viewport_top(new_top);
            let target = viewport_bottom(buf);
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::ScrollLineDown => {
            let new_top = (buf.viewport_top() + n).min(buf.line_count() - 1);
            buf.set_viewport_top(new_top);
            let (line, col) = buf.cursor();
            if line < new_top {
                buf.set_cursor(new_top, col.min(last_col(buf, new_top)));
            }
        }
        Motion::ScrollLineUp => {
            let new_top = buf.viewport_top().saturating_sub(n);
            buf.set_viewport_top(new_top);
            let bottom = viewport_bottom(buf);
            let (line, col) = buf.cursor();
            if line > bottom {
                buf.set_cursor(bottom, col.min(last_col(buf, bottom)));
            }
        }
        Motion::ScrollCenter => {
            let (line, _) = buf.cursor();
            buf.set_viewport_top(line.saturating_sub(buf.viewport_height() / 2));
        }
        Motion::ScrollTop => {
            let (line, _) = buf.cursor();
            buf.set_viewport_top(line);
        }
        Motion::ScrollBottom => {
            let (line, _) = buf.cursor();
            buf.set_viewport_top(line.saturating_sub(buf.viewport_height().saturating_sub(1)));
        }
        Motion::ParagraphForward => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = paragraph_forward_once(buf, line);
            }
            buf.set_cursor(line, 0);
        }
        Motion::ParagraphBackward => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = paragraph_backward_once(buf, line);
            }
            buf.set_cursor(line, 0);
        }
        Motion::SentenceForward => {
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = sentence_forward_once(buf, pos);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::SentenceBackward => {
            let mut pos = buf.cursor();
            for _ in 0..n {
                pos = sentence_backward_once(buf, pos);
            }
            buf.set_cursor(pos.0, pos.1);
        }
        Motion::NextLineNonBlank => {
            let (line, _) = buf.cursor();
            let target = (line + n).min(buf.line_count() - 1);
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::PrevLineNonBlank => {
            let (line, _) = buf.cursor();
            let target = line.saturating_sub(n);
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::MatchPair => {
            if let Some(target) = match_pair_once(buf, buf.cursor()) {
                buf.set_cursor(target.0, target.1);
            }
        }
        Motion::SetMark(name) => {
            let pos = buf.cursor();
            buf.set_mark(name, pos);
        }
        Motion::GotoMark(name) => {
            if let Some((l, c)) = buf.get_mark(name) {
                let l = l.min(buf.line_count() - 1);
                let c = c.min(last_col(buf, l));
                buf.set_cursor(l, c);
            }
        }
        Motion::GotoMarkLine(name) => {
            if let Some((l, _)) = buf.get_mark(name) {
                let l = l.min(buf.line_count() - 1);
                let c = first_non_blank(buf, l);
                buf.set_cursor(l, c);
            }
        }
        Motion::SearchForward(pattern) => {
            if !pattern.is_empty() {
                let re = Regex::compile(&pattern, buf.search_ignore_case(&pattern));
                let mut pos = buf.cursor();
                let mut found = None;
                for _ in 0..n {
                    match search_forward_once(buf, pos, &re) {
                        Some(p) => {
                            pos = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        Motion::SearchBackward(pattern) => {
            if !pattern.is_empty() {
                let re = Regex::compile(&pattern, buf.search_ignore_case(&pattern));
                let mut pos = buf.cursor();
                let mut found = None;
                for _ in 0..n {
                    match search_backward_once(buf, pos, &re) {
                        Some(p) => {
                            pos = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        Motion::SearchWordForward => {
            let pos = buf.cursor();
            if let Some(word) = word_under_cursor(buf, pos) {
                let word_len = word.chars().count();
                let re = Regex::compile(&crate::regex::escape(&word), buf.search_ignore_case(&word));
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_word_forward_once(buf, cur, &re, word_len) {
                        Some(p) => {
                            cur = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        Motion::SearchWordBackward => {
            let pos = buf.cursor();
            if let Some(word) = word_under_cursor(buf, pos) {
                let word_len = word.chars().count();
                let re = Regex::compile(&crate::regex::escape(&word), buf.search_ignore_case(&word));
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_word_backward_once(buf, cur, &re, word_len) {
                        Some(p) => {
                            cur = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        // `g*`/`g#`: identical shape to `SearchWordForward`/`SearchWordBackward`
        // above, just via the plain (boundary-free) `search_forward_once`/
        // `search_backward_once` instead of their word-boundary-checked
        // siblings.
        Motion::SearchWordForwardUnbounded => {
            let pos = buf.cursor();
            if let Some(word) = word_under_cursor(buf, pos) {
                let re = Regex::compile(&crate::regex::escape(&word), buf.search_ignore_case(&word));
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_forward_once(buf, cur, &re) {
                        Some(p) => {
                            cur = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        Motion::SearchWordBackwardUnbounded => {
            let pos = buf.cursor();
            if let Some(word) = word_under_cursor(buf, pos) {
                let re = Regex::compile(&crate::regex::escape(&word), buf.search_ignore_case(&word));
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_backward_once(buf, cur, &re) {
                        Some(p) => {
                            cur = p;
                            found = Some(p);
                        }
                        None => break,
                    }
                }
                if let Some((l, c)) = found {
                    buf.set_cursor(l, c);
                }
            }
        }
        Motion::UnmatchedOpenParen => {
            if let Some((l, c)) = unmatched_backward(buf, buf.cursor(), '(', ')', n) {
                buf.set_cursor(l, c);
            }
        }
        Motion::UnmatchedCloseParen => {
            if let Some((l, c)) = unmatched_forward(buf, buf.cursor(), '(', ')', n) {
                buf.set_cursor(l, c);
            }
        }
        Motion::UnmatchedOpenBrace => {
            if let Some((l, c)) = unmatched_backward(buf, buf.cursor(), '{', '}', n) {
                buf.set_cursor(l, c);
            }
        }
        Motion::UnmatchedCloseBrace => {
            if let Some((l, c)) = unmatched_forward(buf, buf.cursor(), '{', '}', n) {
                buf.set_cursor(l, c);
            }
        }
        Motion::SectionForward => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = section_forward_once(buf, line, '{');
            }
            buf.set_cursor(line, 0);
        }
        Motion::SectionForwardEnd => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = section_forward_once(buf, line, '}');
            }
            buf.set_cursor(line, 0);
        }
        Motion::SectionBackward => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = section_backward_once(buf, line, '{');
            }
            buf.set_cursor(line, 0);
        }
        Motion::SectionBackwardEnd => {
            let (mut line, _) = buf.cursor();
            for _ in 0..n {
                line = section_backward_once(buf, line, '}');
            }
            buf.set_cursor(line, 0);
        }
        // Not reached by any wiring in this crate today -- `vimkeys.rs`
        // only ever produces this while an operator is armed, which
        // `motion_range` (below) intercepts before `apply_motion` would be
        // called. Implemented anyway (moves to the object's start, same as
        // `motion_range`'s own contract) so this stays correct rather than
        // silently wrong if a future caller ever does invoke it standalone.
        Motion::TextObject(kind, around) => {
            if let Some(range) = text_object_range(buf, kind, around, count) {
                buf.set_cursor(range.from.0, range.from.1);
            }
        }
    }
}

/// How an operator (today: yank; later: delete/change) treats a motion's
/// resulting range -- vim's own classification (`:help exclusive`), which
/// this mirrors motion-for-motion. `Inclusive`/`Exclusive` differ only in
/// whether the character the cursor lands *on* is part of the range;
/// `Linewise` ignores columns entirely and takes whole lines.
///
/// Known simplification: real vim also has an adjustment where an
/// exclusive motion that ends in column 1, having started at or before the
/// first non-blank, becomes linewise (`:help exclusive-linewise` --
/// prevents e.g. `dw` at the last word of a line from swallowing the
/// newline into the next line's indent). Not implemented here; it would
/// only show up as a subtly-too-long yank in that specific situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionShape {
    Exclusive,
    Inclusive,
    Linewise,
    /// A rectangle between two corners: rows `from.0..=to.0`, and on each
    /// of them the columns between `from.1` and `to.1` inclusive.
    ///
    /// Unlike the other three, `from`/`to` here are *corners* rather than
    /// a start and an end in reading order -- either may hold the smaller
    /// column. `block_columns` is the one place that is untangled.
    Blockwise,
}

/// The inclusive column range a blockwise motion covers, lowest first.
/// `None` for any other shape, which has no such thing.
pub fn block_columns(range: &MotionRange) -> Option<(usize, usize)> {
    (range.shape == MotionShape::Blockwise).then(|| (range.from.1.min(range.to.1), range.from.1.max(range.to.1)))
}

/// Whether `m` is one of vim's own `:help jump-motions` -- the ones that
/// record where the cursor was *before* they ran, so `Ctrl-O`/`Ctrl-I` (see
/// `vimkeys::VimKeys::push_jump`'s own doc comment) can step back through
/// them. Deliberately excludes the small, local motions (`h`/`j`/`k`/`l`,
/// word motions, `f`/`t`, scrolling, sentence text-object-adjacent detail)
/// even though several of them (e.g. `f`) can technically move the cursor
/// a long way on a long line -- matching vim's own distinction between
/// "a motion" and "a jump" by which commands are on this specific list,
/// not by how far any given use of one happens to move the cursor.
pub fn is_jump(m: &Motion) -> bool {
    matches!(
        m,
        Motion::GotoFirstLine
            | Motion::GotoLastLine
            | Motion::GotoPercent
            | Motion::SearchForward(_)
            | Motion::SearchBackward(_)
            | Motion::SearchWordForward
            | Motion::SearchWordBackward
            | Motion::SearchWordForwardUnbounded
            | Motion::SearchWordBackwardUnbounded
            | Motion::MatchPair
            | Motion::GotoMark(_)
            | Motion::GotoMarkLine(_)
            | Motion::ParagraphForward
            | Motion::ParagraphBackward
            | Motion::SentenceForward
            | Motion::SentenceBackward
            | Motion::SectionForward
            | Motion::SectionForwardEnd
            | Motion::SectionBackward
            | Motion::SectionBackwardEnd
            | Motion::ScreenTop
            | Motion::ScreenMiddle
            | Motion::ScreenBottom
    )
}

/// `None` means `m` isn't a valid operator target at all -- either it's not
/// really a motion (`SetMark` just records a position, it doesn't move
/// anything), or it's one of the viewport/scroll commands (Ctrl-D/U/F/B,
/// Ctrl-E/Y, zz/zt/zb) that real vim documents separately from motions and
/// doesn't accept as operator targets either.
fn motion_shape(m: &Motion) -> Option<MotionShape> {
    use MotionShape::*;
    Some(match m {
        Motion::Left | Motion::Right => Exclusive,
        Motion::Down | Motion::Up => Linewise,
        Motion::LineStart | Motion::LineFirstNonBlank => Exclusive,
        Motion::LineEnd | Motion::LineLastNonBlank => Inclusive,
        Motion::GotoColumn => Exclusive,
        Motion::GotoFirstLine | Motion::GotoLastLine | Motion::GotoPercent => Linewise,
        Motion::WordForward | Motion::WordForwardBig | Motion::WordBackward | Motion::WordBackwardBig => Exclusive,
        Motion::WordEnd | Motion::WordEndBig | Motion::WordEndBackward | Motion::WordEndBackwardBig => Inclusive,
        Motion::FindChar { forward, .. } => {
            if *forward {
                Inclusive
            } else {
                Exclusive
            }
        }
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => Linewise,
        Motion::HalfPageDown
        | Motion::HalfPageUp
        | Motion::PageDown
        | Motion::PageUp
        | Motion::ScrollLineDown
        | Motion::ScrollLineUp
        | Motion::ScrollCenter
        | Motion::ScrollTop
        | Motion::ScrollBottom => return None,
        Motion::ParagraphForward | Motion::ParagraphBackward => Exclusive,
        Motion::SentenceForward | Motion::SentenceBackward => Exclusive,
        Motion::NextLineNonBlank | Motion::PrevLineNonBlank => Linewise,
        Motion::MatchPair => Inclusive,
        Motion::SetMark(_) => return None,
        Motion::GotoMark(_) => Exclusive,
        Motion::GotoMarkLine(_) => Linewise,
        Motion::SearchForward(_) | Motion::SearchBackward(_) => Exclusive,
        Motion::SearchWordForward | Motion::SearchWordBackward => Exclusive,
        Motion::SearchWordForwardUnbounded | Motion::SearchWordBackwardUnbounded => Exclusive,
        Motion::UnmatchedOpenParen
        | Motion::UnmatchedCloseParen
        | Motion::UnmatchedOpenBrace
        | Motion::UnmatchedCloseBrace => Exclusive,
        // Same classification `ParagraphForward`/`ParagraphBackward` already
        // use above -- vim's own section motions are their sibling, moving
        // to a boundary line's own start rather than tracking columns.
        Motion::SectionForward | Motion::SectionForwardEnd | Motion::SectionBackward | Motion::SectionBackwardEnd => Exclusive,
        // Never actually consulted -- `motion_range` special-cases
        // `TextObject` before this function is called at all (see
        // `Motion::TextObject`'s own doc comment). Kept correct anyway,
        // matching `text_object_range`'s own per-kind shape, for whatever
        // future caller might reasonably expect `motion_shape` to be total.
        Motion::TextObject(TextObjectKind::Paragraph, _) => Linewise,
        Motion::TextObject(..) => Inclusive,
    })
}

/// A resolved operator target: `from`/`to` are already ordered (`from <=
/// to`, regardless of which direction the motion actually moved the
/// cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionRange {
    pub shape: MotionShape,
    pub from: (usize, usize),
    pub to: (usize, usize),
}

/// Resolves `motion` against `buf` the same way any other motion would
/// (via the existing `apply_motion`, unchanged -- this adds zero regression
/// risk to plain navigation), then reports the range it covered instead of
/// just leaving the cursor at the destination. Per vim's own rule, the
/// cursor is left at `from` (the earlier of the two endpoints) rather than
/// wherever `apply_motion` put it -- the one exception, `yy`/`Y`, isn't a
/// `Motion` at all and is handled by `whole_lines` instead, which leaves
/// the cursor untouched.
///
/// Returns `None` if `m` isn't a valid operator target (see `motion_shape`)
/// or if it didn't actually move the cursor (a failed find/search, or a
/// motion that's already at its clamped boundary) -- either way, there's
/// nothing to yank and the cursor is restored to where it started.
pub fn motion_range(buf: &mut impl Buffer, motion: Motion, count: Option<usize>) -> Option<MotionRange> {
    // Text objects don't fit the "apply the motion, diff the cursor
    // before/after" trick below -- the cursor usually sits *inside* the
    // target range, not at one of its ends -- so they're resolved directly
    // via `text_object_range` instead, then the cursor is parked at
    // `range.from` to match every other motion's own contract here.
    if let Motion::TextObject(kind, around) = motion {
        let range = text_object_range(buf, kind, around, count)?;
        buf.set_cursor(range.from.0, range.from.1);
        return Some(range);
    }
    let shape = motion_shape(&motion)?;
    // Checked before the move, since `apply_motion` consumes it.
    let cannot_fail = matches!(motion, Motion::LineEnd | Motion::LineLastNonBlank);
    let start = buf.cursor();
    apply_motion(buf, motion, count);
    let end = buf.cursor();
    if start == end {
        // A motion that did not move usually covers nothing, and an
        // operator on it must do nothing -- `dfx` with no `x` ahead is
        // not "delete one character", it is a find that failed.
        //
        // The line-end motions are the exception, because they cannot
        // fail: `$` with the cursor already on the last character has
        // *succeeded*, and being inclusive it still covers that
        // character. Without this, `D`, `d$` and `y$` all silently did
        // nothing at the one position where they are most often used
        // -- reached by `$` itself, which is how you get there.
        //
        // Guarded on the line having any characters at all, so `d$` on
        // an empty line still deletes nothing rather than inventing a
        // character to remove.
        if cannot_fail && shape == MotionShape::Inclusive && buf.line_len(start.0) > 0 {
            return Some(MotionRange { shape, from: start, to: start });
        }
        return None;
    }
    let (from, to) = if pos_lt(start, end) { (start, end) } else { (end, start) };
    buf.set_cursor(from.0, from.1);
    Some(MotionRange { shape, from, to })
}

/// The literal text a `MotionRange` covers. `Linewise` joins whole lines
/// with `\n`, including a trailing one (so the result is always a sequence
/// of complete lines, ready to be spliced back in as-is by a linewise
/// put) -- except across a `line_wraps` boundary, where no line actually
/// ended there, so no `\n` is inserted either. `Inclusive`/`Exclusive`
/// walk character-by-character via the same `step_forward` plain motions
/// use, inserting `\n` exactly when a step crosses a real line boundary
/// (again, not a `line_wraps` one) -- `Exclusive` stops one character
/// short of `to`, `Inclusive` includes it.
pub fn extract_text(buf: &impl Buffer, range: &MotionRange) -> String {
    // A block yanks as one line per row, each cut to the block's own
    // columns -- a short line contributing whatever of it falls inside,
    // which may be nothing at all.
    if let Some((left, right)) = block_columns(range) {
        let rows: Vec<String> = (range.from.0..=range.to.0)
            .map(|line| {
                let chars = buf.line_chars(line);
                chars.iter().skip(left).take(right + 1 - left).collect()
            })
            .collect();
        return rows.join("\n");
    }
    if range.shape == MotionShape::Linewise {
        let mut s = String::new();
        for l in range.from.0..=range.to.0 {
            s.push_str(&buf.line_chars(l).into_iter().collect::<String>());
            if l == range.to.0 || !buf.line_wraps(l) {
                s.push('\n');
            }
        }
        return s;
    }
    let mut s = String::new();
    let mut cur = range.from;
    loop {
        let reached_to = cur == range.to;
        if reached_to && range.shape == MotionShape::Exclusive {
            break;
        }
        if let Some(c) = buf.char_at(cur.0, cur.1) {
            s.push(c);
        }
        if reached_to {
            break;
        }
        let next = match step_forward(buf, cur) {
            Some(n) => n,
            None => break,
        };
        if next.0 != cur.0 && !buf.line_wraps(cur.0) {
            s.push('\n');
        }
        cur = next;
    }
    s
}

/// `yy`/`Y`: the current line plus `count - 1` more, linewise -- vim's
/// double-tap-the-operator shorthand, defined operationally as "this
/// operator, this line" rather than as a real cursor motion (there's no
/// `Motion` variant for it, and unlike every other yank the cursor doesn't
/// move at all, so this takes `&impl Buffer` rather than `&mut impl
/// Buffer`). "This line" always means the *whole* line: if the cursor
/// happens to sit on a `line_wraps` continuation row, the range is
/// widened back to that run's first row first, so `yy` never yanks just a
/// column-width fragment of whatever wrapped there.
pub fn whole_lines(buf: &impl Buffer, count: usize) -> String {
    let (cursor_line, _) = buf.cursor();
    let mut line = cursor_line;
    while line > 0 && buf.line_wraps(line - 1) {
        line -= 1;
    }
    // `count` logical lines, not physical rows -- each iteration widens
    // `last` out to the end of whichever logical line it currently sits
    // at the start of, then (unless this was the last one wanted) steps
    // onto the first row of the next logical line.
    let count = count.max(1);
    let mut last = line;
    for i in 0..count {
        while buf.line_wraps(last) && last + 1 < buf.line_count() {
            last += 1;
        }
        if i + 1 < count {
            if last + 1 >= buf.line_count() {
                break;
            }
            last += 1;
        }
    }
    let mut s = String::new();
    for l in line..=last {
        s.push_str(&buf.line_chars(l).into_iter().collect::<String>());
        if l == last || !buf.line_wraps(l) {
            s.push('\n');
        }
    }
    s
}


/// The canonical name of a motion, as `::bish map` prints it.
///
/// Kebab-case of the variant, deliberately mechanical rather than
/// invented: a vocabulary someone has to memorize is worse than one
/// they can predict, and a derived name cannot fall out of step with
/// the variant it describes the way a hand-chosen synonym can. The
/// parameterized ones append what they carry, since `find-char` without
/// the character would not say which mapping you were looking at.
pub fn describe_motion(motion: &Motion) -> String {
    use Motion::*;
    let plain = match motion {
        Left => "left",
        Right => "right",
        Down => "down",
        Up => "up",
        LineStart => "line-start",
        LineFirstNonBlank => "line-first-non-blank",
        LineEnd => "line-end",
        LineLastNonBlank => "line-last-non-blank",
        GotoColumn => "goto-column",
        GotoFirstLine => "goto-first-line",
        GotoLastLine => "goto-last-line",
        GotoPercent => "goto-percent",
        WordForward => "word-forward",
        WordForwardBig => "word-forward-big",
        WordBackward => "word-backward",
        WordBackwardBig => "word-backward-big",
        WordEnd => "word-end",
        WordEndBig => "word-end-big",
        WordEndBackward => "word-end-backward",
        WordEndBackwardBig => "word-end-backward-big",
        ScreenTop => "screen-top",
        ScreenMiddle => "screen-middle",
        ScreenBottom => "screen-bottom",
        HalfPageDown => "half-page-down",
        HalfPageUp => "half-page-up",
        PageDown => "page-down",
        PageUp => "page-up",
        ScrollLineDown => "scroll-line-down",
        ScrollLineUp => "scroll-line-up",
        ScrollCenter => "scroll-center",
        ScrollTop => "scroll-top",
        ScrollBottom => "scroll-bottom",
        ParagraphForward => "paragraph-forward",
        ParagraphBackward => "paragraph-backward",
        SentenceForward => "sentence-forward",
        SentenceBackward => "sentence-backward",
        NextLineNonBlank => "next-line-non-blank",
        PrevLineNonBlank => "prev-line-non-blank",
        MatchPair => "match-pair",
        SearchWordForward => "search-word-forward",
        SearchWordBackward => "search-word-backward",
        SearchWordForwardUnbounded => "search-word-forward-unbounded",
        SearchWordBackwardUnbounded => "search-word-backward-unbounded",
        UnmatchedOpenParen => "unmatched-open-paren",
        UnmatchedCloseParen => "unmatched-close-paren",
        UnmatchedOpenBrace => "unmatched-open-brace",
        UnmatchedCloseBrace => "unmatched-close-brace",
        SectionForward => "section-forward",
        SectionForwardEnd => "section-forward-end",
        SectionBackward => "section-backward",
        SectionBackwardEnd => "section-backward-end",
        // The rest carry something, and say so.
        FindChar { ch, till, forward } => {
            let verb = if *till { "till-char" } else { "find-char" };
            let dir = if *forward { "" } else { "-backward" };
            return format!("{verb}{dir} {ch:?}");
        }
        SetMark(c) => return format!("set-mark {c:?}"),
        GotoMark(c) => return format!("goto-mark {c:?}"),
        GotoMarkLine(c) => return format!("goto-mark-line {c:?}"),
        SearchForward(p) => return format!("search-forward {p:?}"),
        SearchBackward(p) => return format!("search-backward {p:?}"),
        TextObject(kind, around) => {
            let scope = if *around { "around" } else { "inner" };
            return format!("text-object {scope} {}", describe_text_object(kind));
        }
    };
    plain.to_string()
}

/// The object half of a text-object motion's name -- see describe_motion.
pub fn describe_text_object(kind: &TextObjectKind) -> &'static str {
    use TextObjectKind::*;
    match kind {
        Word => "word",
        WordBig => "word-big",
        Sentence => "sentence",
        Paragraph => "paragraph",
        Paren => "paren",
        Brace => "brace",
        Bracket => "bracket",
        Angle => "angle",
        DoubleQuote => "double-quote",
        SingleQuote => "single-quote",
        Backtick => "backtick",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBuffer {
        lines: Vec<Vec<char>>,
        cursor: (usize, usize),
        vtop: usize,
        vheight: usize,
        marks: std::collections::HashMap<char, (usize, usize)>,
        // Simulates a ScreenBuffer's `line_wraps` -- which lines were "cut"
        // by column-width autowrap rather than a real line break, for
        // tests exercising that (empty for every other test here).
        wraps: std::collections::HashSet<usize>,
        // What `Buffer::search_ignore_case` answers, so a search test can
        // exercise folding without a Shell or a bishopt anywhere in sight.
        icase: bool,
    }

    impl TestBuffer {
        fn new(text: &str) -> Self {
            let lines = text.split('\n').map(|l| l.chars().collect()).collect();
            TestBuffer {
                lines,
                cursor: (0, 0),
                vtop: 0,
                vheight: 24,
                marks: std::collections::HashMap::new(),
                wraps: std::collections::HashSet::new(),
                icase: false,
            }
        }

        // Builds a buffer from "logical lines" that autowrap: each `&str`
        // is one real line's full text, pre-split into `width`-wide
        // storage rows exactly like a terminal grid would, with every row
        // but the last of each logical line marked as wrapped.
        fn new_wrapped(logical_lines: &[&str], width: usize) -> Self {
            let mut lines = Vec::new();
            let mut wraps = std::collections::HashSet::new();
            for logical in logical_lines {
                let chars: Vec<char> = logical.chars().collect();
                if chars.is_empty() {
                    lines.push(Vec::new());
                    continue;
                }
                let mut start = 0;
                while start < chars.len() {
                    let end = (start + width).min(chars.len());
                    lines.push(chars[start..end].to_vec());
                    if end < chars.len() {
                        wraps.insert(lines.len() - 1);
                    }
                    start = end;
                }
            }
            TestBuffer {
                lines,
                cursor: (0, 0),
                vtop: 0,
                vheight: 24,
                marks: std::collections::HashMap::new(),
                wraps,
                icase: false,
            }
        }
    }

    impl Buffer for TestBuffer {
        fn search_ignore_case(&self, _pattern: &str) -> bool {
            self.icase
        }

        fn line_count(&self) -> usize {
            self.lines.len()
        }
        fn line_len(&self, line: usize) -> usize {
            self.lines[line].len()
        }
        fn char_at(&self, line: usize, col: usize) -> Option<char> {
            self.lines[line].get(col).copied()
        }
        fn cursor(&self) -> (usize, usize) {
            self.cursor
        }
        fn set_cursor(&mut self, line: usize, col: usize) {
            self.cursor = (line, col);
        }
        fn viewport_top(&self) -> usize {
            self.vtop
        }
        fn set_viewport_top(&mut self, line: usize) {
            self.vtop = line;
        }
        fn viewport_height(&self) -> usize {
            self.vheight
        }
        fn set_mark(&mut self, name: char, pos: (usize, usize)) {
            self.marks.insert(name, pos);
        }
        fn get_mark(&self, name: char) -> Option<(usize, usize)> {
            self.marks.get(&name).copied()
        }
        fn line_wraps(&self, line: usize) -> bool {
            self.wraps.contains(&line)
        }
    }

    fn go(buf: &mut TestBuffer, motion: Motion, count: Option<usize>) -> (usize, usize) {
        apply_motion(buf, motion, count);
        buf.cursor()
    }

    #[test]
    fn hjkl_basic() {
        let mut buf = TestBuffer::new("abc\ndef\ng");
        buf.set_cursor(0, 1);
        assert_eq!(go(&mut buf, Motion::Right, None), (0, 2));
        assert_eq!(go(&mut buf, Motion::Right, None), (0, 2)); // clamped at last col
        assert_eq!(go(&mut buf, Motion::Left, None), (0, 1));
        assert_eq!(go(&mut buf, Motion::Down, None), (1, 1));
        assert_eq!(go(&mut buf, Motion::Down, None), (2, 0)); // clamped: "g" has only col 0
        assert_eq!(go(&mut buf, Motion::Up, None), (1, 0));
    }

    #[test]
    fn h_and_l_do_not_cross_lines() {
        let mut buf = TestBuffer::new("ab\ncd");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::Left, None), (0, 0));
        buf.set_cursor(0, 1);
        assert_eq!(go(&mut buf, Motion::Right, Some(5)), (0, 1));
    }

    #[test]
    fn right_crosses_a_whole_grapheme_cluster_in_one_press() {
        // 'a' + MAN+ZWJ+WOMAN (a 3-char cluster) + 'b'
        let mut buf = TestBuffer::new("a\u{1F468}\u{200D}\u{1F469}b");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::Right, None), (0, 1), "lands on the cluster's own start, not its second codepoint");
        assert_eq!(go(&mut buf, Motion::Right, None), (0, 4), "one more press crosses the whole cluster to 'b'");
    }

    #[test]
    fn left_crosses_a_whole_grapheme_cluster_in_one_press() {
        let mut buf = TestBuffer::new("a\u{1F468}\u{200D}\u{1F469}b");
        buf.set_cursor(0, 4); // on 'b'
        assert_eq!(go(&mut buf, Motion::Left, None), (0, 1), "lands on the cluster's own start, not its last codepoint");
        assert_eq!(go(&mut buf, Motion::Left, None), (0, 0));
    }

    #[test]
    fn right_never_lands_mid_cluster_even_with_a_count() {
        let mut buf = TestBuffer::new("a\u{1F468}\u{200D}\u{1F469}b");
        buf.set_cursor(0, 0);
        // A count of 2 should land exactly on the cluster start then on
        // 'b' -- never at index 2 or 3, which sit mid-cluster.
        assert_eq!(go(&mut buf, Motion::Right, Some(2)), (0, 4));
    }

    #[test]
    fn last_col_clamps_to_a_trailing_clusters_own_start() {
        // A line ending in a 3-char cluster: the last valid Normal-mode
        // column is where that cluster *starts* (index 1), not its last
        // raw char index (3).
        let mut buf = TestBuffer::new("a\u{1F468}\u{200D}\u{1F469}");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::Right, Some(99)), (0, 1));
    }

    #[test]
    fn ordinary_ascii_left_right_is_unaffected() {
        // Every char here is its own single-char cluster -- must behave
        // exactly like the old raw-char-index arithmetic.
        let mut buf = TestBuffer::new("hello");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::Right, Some(3)), (0, 3));
        assert_eq!(go(&mut buf, Motion::Left, Some(2)), (0, 1));
    }

    #[test]
    fn line_start_and_ends() {
        let mut buf = TestBuffer::new("  hi there  \n");
        buf.set_cursor(0, 5);
        assert_eq!(go(&mut buf, Motion::LineStart, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::LineFirstNonBlank, None), (0, 2));
        assert_eq!(go(&mut buf, Motion::LineEnd, None), (0, 11));
        assert_eq!(go(&mut buf, Motion::LineLastNonBlank, None), (0, 9));
    }

    #[test]
    fn goto_column() {
        let mut buf = TestBuffer::new("abcdef");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::GotoColumn, Some(4)), (0, 3));
        assert_eq!(go(&mut buf, Motion::GotoColumn, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::GotoColumn, Some(99)), (0, 5));
    }

    #[test]
    fn gg_and_g() {
        let mut buf = TestBuffer::new("one\ntwo\n  three\nfour");
        buf.set_cursor(2, 2);
        assert_eq!(go(&mut buf, Motion::GotoLastLine, None), (3, 0));
        assert_eq!(go(&mut buf, Motion::GotoFirstLine, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::GotoFirstLine, Some(3)), (2, 2));
        assert_eq!(go(&mut buf, Motion::GotoLastLine, Some(1)), (0, 0));
        assert_eq!(go(&mut buf, Motion::GotoLastLine, Some(100)), (3, 0));
    }

    #[test]
    fn goto_percent() {
        let mut buf = TestBuffer::new("l0\nl1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::GotoPercent, Some(50)), (4, 0)); // (50*10+99)/100 = 5, 1-indexed
        assert_eq!(go(&mut buf, Motion::GotoPercent, Some(1)), (0, 0));
        assert_eq!(go(&mut buf, Motion::GotoPercent, Some(100)), (9, 0));
        assert_eq!(go(&mut buf, Motion::GotoPercent, Some(1000)), (9, 0)); // clamped
    }

    #[test]
    fn word_forward_basic() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 4)); // foo -> bar
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 7)); // bar -> .
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 8)); // . -> baz
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 13)); // baz -> qux
    }

    #[test]
    fn word_forward_big_ignores_punctuation() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordForwardBig, None), (0, 4));
        assert_eq!(go(&mut buf, Motion::WordForwardBig, None), (0, 13));
    }

    #[test]
    fn word_forward_crosses_lines_and_stops_on_empty_line() {
        let mut buf = TestBuffer::new("foo\n\nbar");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordForward, None), (1, 0)); // empty line is its own word
        assert_eq!(go(&mut buf, Motion::WordForward, None), (2, 0));
    }

    #[test]
    fn word_forward_with_count() {
        let mut buf = TestBuffer::new("one two three four");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordForward, Some(3)), (0, 14));
    }

    #[test]
    fn word_forward_stops_at_end_of_buffer() {
        let mut buf = TestBuffer::new("last");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 3));
        assert_eq!(go(&mut buf, Motion::WordForward, None), (0, 3));
    }

    #[test]
    fn word_backward_basic() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 13); // at 'q' of qux
        assert_eq!(go(&mut buf, Motion::WordBackward, None), (0, 8)); // baz
        assert_eq!(go(&mut buf, Motion::WordBackward, None), (0, 7)); // .
        assert_eq!(go(&mut buf, Motion::WordBackward, None), (0, 4)); // bar
        assert_eq!(go(&mut buf, Motion::WordBackward, None), (0, 0)); // foo
        assert_eq!(go(&mut buf, Motion::WordBackward, None), (0, 0)); // clamped
    }

    #[test]
    fn word_backward_big() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 13);
        assert_eq!(go(&mut buf, Motion::WordBackwardBig, None), (0, 4));
        assert_eq!(go(&mut buf, Motion::WordBackwardBig, None), (0, 0));
    }

    #[test]
    fn word_end_forward() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 2)); // end of foo
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 6)); // end of bar
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 7)); // the '.'
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 10)); // end of baz
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 15)); // end of qux
    }

    #[test]
    fn word_end_forward_big() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordEndBig, None), (0, 2));
        assert_eq!(go(&mut buf, Motion::WordEndBig, None), (0, 10));
        assert_eq!(go(&mut buf, Motion::WordEndBig, None), (0, 15));
    }

    #[test]
    fn word_end_backward() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 15); // 'x' of qux, the last char
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 10)); // end of baz
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 7)); // the '.'
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 6)); // end of bar
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 2)); // end of foo
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 2)); // clamped: no earlier word
    }

    #[test]
    fn word_end_backward_big() {
        let mut buf = TestBuffer::new("foo bar.baz  qux");
        buf.set_cursor(0, 15);
        assert_eq!(go(&mut buf, Motion::WordEndBackwardBig, None), (0, 10));
        assert_eq!(go(&mut buf, Motion::WordEndBackwardBig, None), (0, 2));
    }

    #[test]
    fn word_motions_cross_lines() {
        let mut buf = TestBuffer::new("foo\nbar");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (0, 2));
        assert_eq!(go(&mut buf, Motion::WordEnd, None), (1, 2)); // crosses into next line
        buf.set_cursor(1, 2);
        assert_eq!(go(&mut buf, Motion::WordEndBackward, None), (0, 2));
    }

    #[test]
    fn find_char_forward() {
        let mut buf = TestBuffer::new("abcXdefXghi");
        buf.set_cursor(0, 0);
        let f = Motion::FindChar { ch: 'X', till: false, forward: true };
        assert_eq!(go(&mut buf, f.clone(), None), (0, 3));
        assert_eq!(go(&mut buf, f.clone(), None), (0, 7));
        // no third X: motion fails, cursor stays put
        assert_eq!(go(&mut buf, f, None), (0, 7));
    }

    #[test]
    fn find_char_till_forward() {
        let mut buf = TestBuffer::new("abcXdefXghi");
        buf.set_cursor(0, 0);
        let t = Motion::FindChar { ch: 'X', till: true, forward: true };
        assert_eq!(go(&mut buf, t, None), (0, 2));
    }

    #[test]
    fn find_char_backward_and_till() {
        let mut buf = TestBuffer::new("abcXdefXghi");
        buf.set_cursor(0, 10);
        let big_f = Motion::FindChar { ch: 'X', till: false, forward: false };
        assert_eq!(go(&mut buf, big_f.clone(), None), (0, 7));
        assert_eq!(go(&mut buf, big_f, None), (0, 3));
        buf.set_cursor(0, 10);
        let big_t = Motion::FindChar { ch: 'X', till: true, forward: false };
        assert_eq!(go(&mut buf, big_t, None), (0, 8));
    }

    #[test]
    fn find_char_with_count() {
        let mut buf = TestBuffer::new("abcXdefXghi");
        buf.set_cursor(0, 0);
        let f = Motion::FindChar { ch: 'X', till: false, forward: true };
        assert_eq!(go(&mut buf, f, Some(2)), (0, 7));
    }

    #[test]
    fn find_char_not_found_is_a_no_op() {
        let mut buf = TestBuffer::new("abcXdefXghi");
        buf.set_cursor(0, 5);
        let f = Motion::FindChar { ch: 'Z', till: false, forward: true };
        assert_eq!(go(&mut buf, f, None), (0, 5));
    }

    #[test]
    fn paragraph_motions() {
        let mut buf = TestBuffer::new("a\nb\n\nc\nd\n\n\ne");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::ParagraphForward, None), (2, 0));
        assert_eq!(go(&mut buf, Motion::ParagraphForward, None), (5, 0));
        assert_eq!(go(&mut buf, Motion::ParagraphForward, None), (7, 0)); // clamped at last line
        assert_eq!(go(&mut buf, Motion::ParagraphForward, None), (7, 0));

        assert_eq!(go(&mut buf, Motion::ParagraphBackward, None), (6, 0));
        assert_eq!(go(&mut buf, Motion::ParagraphBackward, None), (2, 0));
        assert_eq!(go(&mut buf, Motion::ParagraphBackward, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::ParagraphBackward, None), (0, 0)); // clamped
    }

    #[test]
    fn sentence_forward_and_backward() {
        let mut buf = TestBuffer::new("One. Two. Three.");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SentenceForward, None), (0, 5));
        assert_eq!(go(&mut buf, Motion::SentenceForward, None), (0, 10));
        assert_eq!(go(&mut buf, Motion::SentenceForward, None), (0, 15)); // clamped on the last '.'

        assert_eq!(go(&mut buf, Motion::SentenceBackward, None), (0, 10));
        assert_eq!(go(&mut buf, Motion::SentenceBackward, None), (0, 5));
        assert_eq!(go(&mut buf, Motion::SentenceBackward, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::SentenceBackward, None), (0, 0)); // clamped
    }

    #[test]
    fn sentence_forward_ignores_decimal_points() {
        let mut buf = TestBuffer::new("3.14 is pi. Ok.");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SentenceForward, None), (0, 12));
    }

    #[test]
    fn next_and_prev_line_non_blank() {
        let mut buf = TestBuffer::new("  a\n\n   b\nc");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::NextLineNonBlank, None), (1, 0));
        assert_eq!(go(&mut buf, Motion::NextLineNonBlank, None), (2, 3));
        assert_eq!(go(&mut buf, Motion::NextLineNonBlank, None), (3, 0));
        assert_eq!(go(&mut buf, Motion::PrevLineNonBlank, None), (2, 3));
        assert_eq!(go(&mut buf, Motion::PrevLineNonBlank, None), (1, 0));
        assert_eq!(go(&mut buf, Motion::PrevLineNonBlank, None), (0, 2));
    }

    fn numbered_lines(n: usize) -> String {
        (0..n)
            .map(|i| format!("l{}", i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn screen_top_middle_bottom() {
        let mut buf = TestBuffer::new(&numbered_lines(20));
        buf.vheight = 5;
        buf.vtop = 0;
        assert_eq!(go(&mut buf, Motion::ScreenTop, None), (0, 0));
        assert_eq!(go(&mut buf, Motion::ScreenTop, Some(3)), (2, 0));
        assert_eq!(go(&mut buf, Motion::ScreenMiddle, None), (2, 0));
        assert_eq!(go(&mut buf, Motion::ScreenBottom, None), (4, 0));
        assert_eq!(go(&mut buf, Motion::ScreenBottom, Some(2)), (3, 0));
    }

    // A block's corners are corners, not a start and an end in reading
    // order: dragging up-and-right is an ordinary thing to do.
    #[test]
    fn block_columns_sorts_the_corners_and_only_for_a_block() {
        let block = |a: (usize, usize), b: (usize, usize)| MotionRange { shape: MotionShape::Blockwise, from: a, to: b };
        assert_eq!(block_columns(&block((0, 2), (3, 7))), Some((2, 7)));
        assert_eq!(block_columns(&block((0, 7), (3, 2))), Some((2, 7)), "right-to-left drags the same rectangle");
        assert_eq!(block_columns(&block((0, 4), (3, 4))), Some((4, 4)), "one column wide");
        assert_eq!(block_columns(&MotionRange { shape: MotionShape::Inclusive, from: (0, 2), to: (3, 7) }), None);
    }

    #[test]
    fn extracting_a_block_cuts_each_row_to_the_same_columns() {
        let buf = TestBuffer::new("aaa111\nbbb222\nccc333");
        let range = MotionRange { shape: MotionShape::Blockwise, from: (0, 1), to: (2, 3) };
        assert_eq!(extract_text(&buf, &range), "aa1\nbb2\ncc3");

        // A row shorter than the block contributes whatever of it falls
        // inside -- which can be nothing at all.
        let buf = TestBuffer::new("aaa111\nb\nccc333");
        let range = MotionRange { shape: MotionShape::Blockwise, from: (0, 3), to: (2, 5) };
        assert_eq!(extract_text(&buf, &range), "111\n\n333");
    }

    // The engine folds; this is the wiring that decides whether it does.
    #[test]
    fn a_search_folds_case_only_when_the_buffer_says_so() {
        let lines = "alpha\nbeta\nBETA\ngamma";
        let mut buf = TestBuffer::new(lines);
        buf.set_cursor(0, 0);
        // Case-sensitive: `BETA` is on line 2 (0-based).
        assert_eq!(go(&mut buf, Motion::SearchForward("BETA".to_string()), None), (2, 0));

        let mut buf = TestBuffer::new(lines);
        buf.icase = true;
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchForward("BETA".to_string()), None), (1, 0), "folded, so `beta` on line 1 comes first");

        // Backwards, and the highlight pass, go through the same answer.
        let mut buf = TestBuffer::new(lines);
        buf.icase = true;
        buf.set_cursor(3, 0);
        assert_eq!(go(&mut buf, Motion::SearchBackward("BETA".to_string()), None), (2, 0));
        assert_eq!(find_matches_in_line(&buf, 1, "BETA"), vec![(0, 4)]);

        let buf = TestBuffer::new(lines);
        assert!(find_matches_in_line(&buf, 1, "BETA").is_empty(), "and case-sensitively it does not match at all");
    }

    // `*` searches for the word under the cursor, and vim folds it the
    // same way it folds a typed pattern.
    #[test]
    fn the_word_under_the_cursor_folds_too() {
        let lines = "beta\ngamma\nBETA";
        let mut buf = TestBuffer::new(lines);
        buf.icase = true;
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForward, None), (2, 0));

        let mut buf = TestBuffer::new(lines);
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForward, None), (0, 0), "case-sensitively there is nowhere else to go");
    }

    #[test]
    fn half_and_full_page_scroll() {
        let mut buf = TestBuffer::new(&numbered_lines(20));
        buf.vheight = 5;
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::HalfPageDown, None), (2, 0));
        assert_eq!(buf.viewport_top(), 2);
        assert_eq!(go(&mut buf, Motion::HalfPageUp, None), (0, 0));
        assert_eq!(buf.viewport_top(), 0);

        assert_eq!(go(&mut buf, Motion::PageDown, None), (3, 0));
        assert_eq!(buf.viewport_top(), 3);
        assert_eq!(go(&mut buf, Motion::PageUp, None), (4, 0));
        assert_eq!(buf.viewport_top(), 0);
    }

    #[test]
    fn scroll_line_up_and_down() {
        let mut buf = TestBuffer::new(&numbered_lines(20));
        buf.vheight = 5;
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::ScrollLineDown, None), (1, 0));
        assert_eq!(buf.viewport_top(), 1);
        assert_eq!(go(&mut buf, Motion::ScrollLineUp, None), (1, 0)); // cursor still in view
        assert_eq!(buf.viewport_top(), 0);
    }

    // One notch of the wheel moves the view by one line, which is what
    // the terminal would have done with it before bish started
    // handling the wheel itself. Three lines a notch turned a
    // trackpad's stream of fine-grained notches into a lurch -- see
    // `fileeditor::MOUSE_WHEEL_LINES`, which is the number this pins.
    #[test]
    fn one_wheel_notch_scrolls_exactly_one_line() {
        let notch = Some(crate::fileeditor::MOUSE_WHEEL_LINES);
        let mut buf = TestBuffer::new(&numbered_lines(40));
        buf.vheight = 10;
        buf.set_cursor(0, 0);
        go(&mut buf, Motion::ScrollLineDown, notch);
        assert_eq!(buf.viewport_top(), 1, "one notch down is one line");
        for _ in 0..4 {
            go(&mut buf, Motion::ScrollLineDown, notch);
        }
        assert_eq!(buf.viewport_top(), 5, "and five notches are five lines, not fifteen");
        go(&mut buf, Motion::ScrollLineUp, notch);
        assert_eq!(buf.viewport_top(), 4);
    }

    #[test]
    fn scroll_center_top_bottom() {
        let mut buf = TestBuffer::new(&numbered_lines(20));
        buf.vheight = 5;
        buf.set_cursor(10, 0);
        apply_motion(&mut buf, Motion::ScrollCenter, None);
        assert_eq!(buf.viewport_top(), 8);
        assert_eq!(buf.cursor(), (10, 0)); // cursor never moves for zz/zt/zb
        apply_motion(&mut buf, Motion::ScrollTop, None);
        assert_eq!(buf.viewport_top(), 10);
        apply_motion(&mut buf, Motion::ScrollBottom, None);
        assert_eq!(buf.viewport_top(), 6);
    }

    #[test]
    fn match_pair_nested_brackets() {
        let mut buf = TestBuffer::new("foo(bar[baz]qux)end");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::MatchPair, None), (0, 15)); // scans forward to '(', lands on ')'
        buf.set_cursor(0, 15);
        assert_eq!(go(&mut buf, Motion::MatchPair, None), (0, 3));
        buf.set_cursor(0, 7);
        assert_eq!(go(&mut buf, Motion::MatchPair, None), (0, 11));
        buf.set_cursor(0, 11);
        assert_eq!(go(&mut buf, Motion::MatchPair, None), (0, 7));
    }

    #[test]
    fn match_pair_no_bracket_is_a_no_op() {
        let mut buf = TestBuffer::new("hello");
        buf.set_cursor(0, 2);
        assert_eq!(go(&mut buf, Motion::MatchPair, None), (0, 2));
    }

    #[test]
    fn unmatched_paren_motions_skip_balanced_pairs() {
        let mut buf = TestBuffer::new("(a (b) c)");
        buf.set_cursor(0, 4); // inside the nested "(b)"
        assert_eq!(go(&mut buf, Motion::UnmatchedOpenParen, None), (0, 3));
        assert_eq!(go(&mut buf, Motion::UnmatchedOpenParen, None), (0, 0));
        buf.set_cursor(0, 4);
        assert_eq!(go(&mut buf, Motion::UnmatchedCloseParen, None), (0, 5));
        assert_eq!(go(&mut buf, Motion::UnmatchedCloseParen, None), (0, 8));
    }

    #[test]
    fn unmatched_paren_motion_count_repeats() {
        let mut buf = TestBuffer::new("(a (b (c) d) e)");
        buf.set_cursor(0, 7); // inside "(c)"
        assert_eq!(go(&mut buf, Motion::UnmatchedOpenParen, Some(2)), (0, 3));
    }

    #[test]
    fn unmatched_paren_motion_with_no_enclosing_bracket_is_a_no_op() {
        let mut buf = TestBuffer::new("abc");
        buf.set_cursor(0, 1);
        assert_eq!(go(&mut buf, Motion::UnmatchedOpenParen, None), (0, 1));
        assert_eq!(go(&mut buf, Motion::UnmatchedCloseParen, None), (0, 1));
    }

    #[test]
    fn unmatched_brace_motions() {
        let mut buf = TestBuffer::new("{ a { b } c }");
        buf.set_cursor(0, 6); // inside the nested "{ b }"
        assert_eq!(go(&mut buf, Motion::UnmatchedOpenBrace, None), (0, 4));
        assert_eq!(go(&mut buf, Motion::UnmatchedCloseBrace, None), (0, 8));
    }

    #[test]
    fn section_motions_find_lines_starting_with_brace() {
        let mut buf = TestBuffer::new("a\n{\nb\nc\n}\nd");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SectionForward, None), (1, 0));
        assert_eq!(go(&mut buf, Motion::SectionForwardEnd, None), (4, 0));
        buf.set_cursor(5, 0);
        assert_eq!(go(&mut buf, Motion::SectionBackward, None), (1, 0));
        buf.set_cursor(4, 0);
        // searches strictly backward from the current line, same as vim --
        // sitting on a '}' line itself doesn't count as "already there".
        assert_eq!(go(&mut buf, Motion::SectionBackwardEnd, None), (0, 0));
    }

    #[test]
    fn section_motions_clamp_at_buffer_edges() {
        let mut buf = TestBuffer::new("a\nb\nc");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SectionForward, None), (2, 0)); // no '{' anywhere -- clamps to last line
        buf.set_cursor(2, 0);
        assert_eq!(go(&mut buf, Motion::SectionBackward, None), (0, 0)); // clamps to first line
    }

    #[test]
    fn marks_set_and_goto() {
        let mut buf = TestBuffer::new("abc\ndef\nghi");
        buf.set_cursor(1, 1);
        apply_motion(&mut buf, Motion::SetMark('a'), None);
        buf.set_cursor(2, 2);
        assert_eq!(go(&mut buf, Motion::GotoMark('a'), None), (1, 1));
        buf.set_cursor(2, 2);
        assert_eq!(go(&mut buf, Motion::GotoMarkLine('a'), None), (1, 0));
        // an unset mark is a no-op
        assert_eq!(go(&mut buf, Motion::GotoMark('z'), None), (1, 0));
    }

    #[test]
    fn search_forward_and_backward_wrap() {
        let mut buf = TestBuffer::new("foo bar foo baz foo");
        buf.set_cursor(0, 0);
        let fwd = Motion::SearchForward("foo".to_string());
        assert_eq!(go(&mut buf, fwd.clone(), None), (0, 8));
        assert_eq!(go(&mut buf, fwd.clone(), None), (0, 16));
        assert_eq!(go(&mut buf, fwd, None), (0, 0)); // wraps around

        buf.set_cursor(0, 16);
        let back = Motion::SearchBackward("foo".to_string());
        assert_eq!(go(&mut buf, back.clone(), None), (0, 8));
        assert_eq!(go(&mut buf, back.clone(), None), (0, 0));
        assert_eq!(go(&mut buf, back, None), (0, 16)); // wraps around
    }

    #[test]
    fn search_forward_with_count() {
        let mut buf = TestBuffer::new("foo bar foo baz foo");
        buf.set_cursor(0, 0);
        assert_eq!(
            go(&mut buf, Motion::SearchForward("foo".to_string()), Some(2)),
            (0, 16)
        );
    }

    #[test]
    fn search_not_found_is_a_no_op() {
        let mut buf = TestBuffer::new("foo bar");
        buf.set_cursor(0, 2);
        assert_eq!(go(&mut buf, Motion::SearchForward("xyz".to_string()), None), (0, 2));
    }

    #[test]
    fn search_forward_and_backward_are_full_ere_regex() {
        // `/`/`?` now run through crate::regex, so a class/quantifier
        // pattern like this finds "foo1"/"foo22"/"foo333" as whole matches,
        // not just a literal substring.
        let mut buf = TestBuffer::new("foo1 foo22 foo333");
        buf.set_cursor(0, 0);
        let fwd = Motion::SearchForward("foo[0-9]+".to_string());
        assert_eq!(go(&mut buf, fwd.clone(), None), (0, 5));
        assert_eq!(go(&mut buf, fwd, None), (0, 11));

        let mut buf = TestBuffer::new("foo1 foo22 foo333");
        buf.set_cursor(0, 17);
        assert_eq!(go(&mut buf, Motion::SearchBackward("foo[0-9]+".to_string()), None), (0, 11));
    }

    #[test]
    fn search_forward_honors_anchors_per_line() {
        let mut buf = TestBuffer::new("xfoo\nfoo\nfooo");
        buf.set_cursor(0, 0);
        // ^foo only matches a line that *starts* with "foo" -- skips the
        // first line's "foo" (it's at column 1, not 0) for the second's.
        assert_eq!(go(&mut buf, Motion::SearchForward("^foo".to_string()), None), (1, 0));
    }

    #[test]
    fn find_matches_in_line_reports_every_regex_match() {
        let buf = TestBuffer::new("a1 a22 a333");
        assert_eq!(find_matches_in_line(&buf, 0, "a[0-9]+"), vec![(0, 2), (3, 6), (7, 11)]);
    }

    #[test]
    fn search_word_forward_and_backward_respect_word_boundaries() {
        // "category" contains "cat" as a substring but not as a whole word --
        // * must skip it and land on the next real "cat".
        let mut buf = TestBuffer::new("cat dog category cat");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForward, None), (0, 17));
        assert_eq!(go(&mut buf, Motion::SearchWordBackward, None), (0, 0));
    }

    #[test]
    fn g_star_and_g_hash_ignore_word_boundaries() {
        // Same buffer as the bounded test above, but g*/g# should land on
        // "category"'s embedded "cat" (starting at column 8) instead of
        // skipping past it to the next whole-word match.
        let mut buf = TestBuffer::new("cat dog category cat");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForwardUnbounded, None), (0, 8));
        buf.set_cursor(0, 17);
        assert_eq!(go(&mut buf, Motion::SearchWordBackwardUnbounded, None), (0, 8));
    }

    #[test]
    fn is_jump_classifies_vims_own_jump_motions() {
        let jumps: &[Motion] = &[
            Motion::GotoFirstLine,
            Motion::GotoLastLine,
            Motion::GotoPercent,
            Motion::SearchForward("x".to_string()),
            Motion::SearchBackward("x".to_string()),
            Motion::SearchWordForward,
            Motion::SearchWordBackward,
            Motion::SearchWordForwardUnbounded,
            Motion::SearchWordBackwardUnbounded,
            Motion::MatchPair,
            Motion::GotoMark('a'),
            Motion::GotoMarkLine('a'),
            Motion::ParagraphForward,
            Motion::ParagraphBackward,
            Motion::SentenceForward,
            Motion::SentenceBackward,
            Motion::SectionForward,
            Motion::SectionForwardEnd,
            Motion::SectionBackward,
            Motion::SectionBackwardEnd,
            Motion::ScreenTop,
            Motion::ScreenMiddle,
            Motion::ScreenBottom,
        ];
        for m in jumps {
            assert!(is_jump(m), "{m:?} should be classified as a jump");
        }
        let not_jumps: &[Motion] = &[
            Motion::Left,
            Motion::WordForward,
            Motion::FindChar { ch: 'x', till: false, forward: true },
            Motion::HalfPageDown,
            Motion::LineEnd,
            Motion::UnmatchedOpenParen,
            Motion::TextObject(TextObjectKind::Word, false),
        ];
        for m in not_jumps {
            assert!(!is_jump(m), "{m:?} should not be classified as a jump");
        }
    }

    #[test]
    fn motion_shape_table() {
        let cases: &[(Motion, Option<MotionShape>)] = &[
            (Motion::Left, Some(MotionShape::Exclusive)),
            (Motion::Right, Some(MotionShape::Exclusive)),
            (Motion::Down, Some(MotionShape::Linewise)),
            (Motion::Up, Some(MotionShape::Linewise)),
            (Motion::LineStart, Some(MotionShape::Exclusive)),
            (Motion::LineFirstNonBlank, Some(MotionShape::Exclusive)),
            (Motion::LineEnd, Some(MotionShape::Inclusive)),
            (Motion::LineLastNonBlank, Some(MotionShape::Inclusive)),
            (Motion::GotoColumn, Some(MotionShape::Exclusive)),
            (Motion::GotoFirstLine, Some(MotionShape::Linewise)),
            (Motion::GotoLastLine, Some(MotionShape::Linewise)),
            (Motion::WordForward, Some(MotionShape::Exclusive)),
            (Motion::WordForwardBig, Some(MotionShape::Exclusive)),
            (Motion::WordBackward, Some(MotionShape::Exclusive)),
            (Motion::WordBackwardBig, Some(MotionShape::Exclusive)),
            (Motion::WordEnd, Some(MotionShape::Inclusive)),
            (Motion::WordEndBig, Some(MotionShape::Inclusive)),
            (Motion::WordEndBackward, Some(MotionShape::Inclusive)),
            (Motion::WordEndBackwardBig, Some(MotionShape::Inclusive)),
            (Motion::FindChar { ch: 'x', till: false, forward: true }, Some(MotionShape::Inclusive)),
            (Motion::FindChar { ch: 'x', till: true, forward: true }, Some(MotionShape::Inclusive)),
            (Motion::FindChar { ch: 'x', till: false, forward: false }, Some(MotionShape::Exclusive)),
            (Motion::FindChar { ch: 'x', till: true, forward: false }, Some(MotionShape::Exclusive)),
            (Motion::ScreenTop, Some(MotionShape::Linewise)),
            (Motion::ScreenMiddle, Some(MotionShape::Linewise)),
            (Motion::ScreenBottom, Some(MotionShape::Linewise)),
            (Motion::HalfPageDown, None),
            (Motion::HalfPageUp, None),
            (Motion::PageDown, None),
            (Motion::PageUp, None),
            (Motion::ScrollLineDown, None),
            (Motion::ScrollLineUp, None),
            (Motion::ScrollCenter, None),
            (Motion::ScrollTop, None),
            (Motion::ScrollBottom, None),
            (Motion::ParagraphForward, Some(MotionShape::Exclusive)),
            (Motion::ParagraphBackward, Some(MotionShape::Exclusive)),
            (Motion::SentenceForward, Some(MotionShape::Exclusive)),
            (Motion::SentenceBackward, Some(MotionShape::Exclusive)),
            (Motion::NextLineNonBlank, Some(MotionShape::Linewise)),
            (Motion::PrevLineNonBlank, Some(MotionShape::Linewise)),
            (Motion::MatchPair, Some(MotionShape::Inclusive)),
            (Motion::SetMark('a'), None),
            (Motion::GotoMark('a'), Some(MotionShape::Exclusive)),
            (Motion::GotoMarkLine('a'), Some(MotionShape::Linewise)),
            (Motion::SearchForward("x".to_string()), Some(MotionShape::Exclusive)),
            (Motion::SearchBackward("x".to_string()), Some(MotionShape::Exclusive)),
            (Motion::SearchWordForward, Some(MotionShape::Exclusive)),
            (Motion::SearchWordBackward, Some(MotionShape::Exclusive)),
        ];
        for (motion, expected) in cases {
            assert_eq!(motion_shape(motion), *expected, "{:?}", motion);
        }
    }

    #[test]
    fn word_chars_decide_where_a_word_ends() {
        // `iskeyword`: letters and digits are always word characters,
        // and the buffer says which punctuation joins them.
        struct Kw(TestBuffer, &'static str);
        impl Buffer for Kw {
            fn line_count(&self) -> usize { self.0.line_count() }
            fn line_len(&self, l: usize) -> usize { self.0.line_len(l) }
            fn char_at(&self, l: usize, c: usize) -> Option<char> { self.0.char_at(l, c) }
            fn cursor(&self) -> (usize, usize) { self.0.cursor() }
            fn set_cursor(&mut self, l: usize, c: usize) { self.0.set_cursor(l, c) }
            fn viewport_top(&self) -> usize { self.0.viewport_top() }
            fn set_viewport_top(&mut self, l: usize) { self.0.set_viewport_top(l) }
            fn viewport_height(&self) -> usize { self.0.viewport_height() }
            fn set_mark(&mut self, name: char, pos: (usize, usize)) { self.0.set_mark(name, pos) }
            fn get_mark(&self, name: char) -> Option<(usize, usize)> { self.0.get_mark(name) }
            fn word_chars(&self) -> &str { self.1 }
        }
        let end_of_first_word = |extra: &'static str| {
            let mut buf = Kw(TestBuffer::new("foo-bar baz"), extra);
            buf.set_cursor(0, 0);
            apply_motion(&mut buf, Motion::WordForward, None);
            buf.cursor()
        };
        // Default: `-` is punctuation, so `w` stops at it.
        assert_eq!(end_of_first_word("_"), (0, 3));
        // With `-` a word character, `foo-bar` is one word.
        assert_eq!(end_of_first_word("_-"), (0, 8));
    }

    #[test]
    fn a_line_end_motion_that_is_already_there_still_covers_that_character() {
        // `D`, `d$` and `y$` are most often used from exactly where `$`
        // leaves you, and did nothing there: the cursor had not moved,
        // so the range came back empty.
        let mut buf = TestBuffer::new("abxcd");
        buf.set_cursor(0, 4);
        let r = motion_range(&mut buf, Motion::LineEnd, None).unwrap();
        assert_eq!(r.shape, MotionShape::Inclusive);
        assert_eq!((r.from, r.to), ((0, 4), (0, 4)));
        assert_eq!(extract_text(&buf, &r), "d");
    }

    #[test]
    fn an_empty_line_has_no_character_for_a_line_end_operator_to_take() {
        let mut buf = TestBuffer::new("");
        buf.set_cursor(0, 0);
        assert!(motion_range(&mut buf, Motion::LineEnd, None).is_none());
    }

    #[test]
    fn a_motion_that_failed_still_covers_nothing() {
        // The distinction the fix turns on: `$` not moving means it is
        // already there, while `f` not moving means no match was found.
        // An operator must do nothing for the second.
        let mut buf = TestBuffer::new("abxcd");
        buf.set_cursor(0, 4);
        assert!(motion_range(&mut buf, Motion::FindChar { ch: 'z', till: false, forward: true }, None).is_none());
        assert!(motion_range(&mut buf, Motion::MatchPair, None).is_none());
        // ...and an exclusive motion with nowhere to go, likewise.
        buf.set_cursor(0, 0);
        assert!(motion_range(&mut buf, Motion::LineStart, None).is_none());
    }

    #[test]
    fn motion_range_exclusive_word_forward() {
        let mut buf = TestBuffer::new("foo bar baz");
        buf.set_cursor(0, 0);
        let r = motion_range(&mut buf, Motion::WordForward, None).unwrap();
        assert_eq!(r.shape, MotionShape::Exclusive);
        assert_eq!((r.from, r.to), ((0, 0), (0, 4)));
        assert_eq!(buf.cursor(), (0, 0)); // cursor left at `from`
        assert_eq!(extract_text(&buf, &r), "foo ");
    }

    #[test]
    fn motion_range_inclusive_word_end() {
        let mut buf = TestBuffer::new("foo bar baz");
        buf.set_cursor(0, 0);
        let r = motion_range(&mut buf, Motion::WordEnd, None).unwrap();
        assert_eq!(r.shape, MotionShape::Inclusive);
        assert_eq!(extract_text(&buf, &r), "foo");
    }

    #[test]
    fn motion_range_backward_motion_normalizes_from_and_to() {
        let mut buf = TestBuffer::new("foo bar baz");
        buf.set_cursor(0, 8); // 'b' of baz
        let r = motion_range(&mut buf, Motion::WordBackward, None).unwrap();
        assert_eq!((r.from, r.to), ((0, 4), (0, 8)));
        assert_eq!(buf.cursor(), (0, 4));
        assert_eq!(extract_text(&buf, &r), "bar ");
    }

    #[test]
    fn motion_range_linewise_spans_whole_lines() {
        let mut buf = TestBuffer::new("one\ntwo\nthree");
        buf.set_cursor(0, 1);
        let r = motion_range(&mut buf, Motion::Down, Some(1)).unwrap();
        assert_eq!(r.shape, MotionShape::Linewise);
        assert_eq!(extract_text(&buf, &r), "one\ntwo\n");
    }

    #[test]
    fn motion_range_multiline_charwise_inserts_newline_at_boundary() {
        let mut buf = TestBuffer::new("ab\ncd");
        buf.set_cursor(0, 0);
        let r = motion_range(&mut buf, Motion::LineEnd, Some(2)).unwrap();
        assert_eq!(r.shape, MotionShape::Inclusive);
        assert_eq!(extract_text(&buf, &r), "ab\ncd");
    }

    #[test]
    fn motion_range_returns_none_for_a_no_op_motion() {
        let mut buf = TestBuffer::new("abc");
        buf.set_cursor(0, 0);
        assert!(motion_range(&mut buf, Motion::Left, None).is_none());
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn motion_range_returns_none_for_a_failed_find() {
        let mut buf = TestBuffer::new("abc");
        buf.set_cursor(0, 0);
        let f = Motion::FindChar { ch: 'z', till: false, forward: true };
        assert!(motion_range(&mut buf, f, None).is_none());
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn motion_range_returns_none_for_non_motion_targets() {
        let mut buf = TestBuffer::new("abc");
        buf.set_cursor(0, 0);
        assert!(motion_range(&mut buf, Motion::SetMark('a'), None).is_none());
        assert!(motion_range(&mut buf, Motion::ScrollCenter, None).is_none());
    }

    #[test]
    fn whole_lines_yy_single_and_multi_count() {
        let mut buf = TestBuffer::new("one\ntwo\nthree");
        buf.set_cursor(1, 2);
        assert_eq!(whole_lines(&buf, 1), "two\n");
        assert_eq!(buf.cursor(), (1, 2)); // never moves
        assert_eq!(whole_lines(&buf, 2), "two\nthree\n");
    }

    #[test]
    fn whole_lines_clamps_count_past_the_end_of_the_buffer() {
        let mut buf = TestBuffer::new("one\ntwo");
        buf.set_cursor(0, 0);
        assert_eq!(whole_lines(&buf, 99), "one\ntwo\n");
    }

    #[test]
    fn extract_text_linewise_spans_an_empty_line() {
        let mut buf = TestBuffer::new("a\n\nb");
        buf.set_cursor(0, 0);
        let r = motion_range(&mut buf, Motion::GotoLastLine, None).unwrap();
        assert_eq!(r.shape, MotionShape::Linewise);
        assert_eq!(extract_text(&buf, &r), "a\n\nb\n");
    }

    #[test]
    fn whole_lines_yy_joins_a_wrapped_line_without_a_newline() {
        // A single long logical line, "helloworldXYZ", autowrapped into
        // three 5-wide storage rows: "hello", "world", "XYZ".
        let mut buf = TestBuffer::new_wrapped(&["helloworldXYZ", "next"], 5);
        buf.set_cursor(0, 0);
        assert_eq!(whole_lines(&buf, 1), "helloworldXYZ\n");

        // Same, but starting from the *middle* row of the wrapped run --
        // yy still yanks the whole logical line, not just that fragment.
        buf.set_cursor(1, 0);
        assert_eq!(whole_lines(&buf, 1), "helloworldXYZ\n");

        // count > 1 from the run's start also picks up the real next
        // line whole, with a real newline separating the two.
        buf.set_cursor(0, 0);
        assert_eq!(whole_lines(&buf, 2), "helloworldXYZ\nnext\n");
    }

    #[test]
    fn extract_text_linewise_does_not_garble_a_wrapped_run() {
        let mut buf = TestBuffer::new_wrapped(&["helloworldXYZ"], 5);
        buf.set_cursor(0, 0);
        let r = MotionRange { shape: MotionShape::Linewise, from: (0, 0), to: (2, 0) };
        assert_eq!(extract_text(&buf, &r), "helloworldXYZ\n");
    }

    #[test]
    fn line_end_reaches_the_true_end_of_a_wrapped_line() {
        let mut buf = TestBuffer::new_wrapped(&["helloworldXYZ"], 5);
        buf.set_cursor(0, 0);
        // "$" from the first storage row lands on the run's last row, at
        // its own last real character -- not at column 4 of row 0.
        assert_eq!(go(&mut buf, Motion::LineEnd, None), (2, 2));
    }

    #[test]
    fn extract_text_charwise_crosses_an_empty_line() {
        let mut buf = TestBuffer::new("a\n\nb");
        buf.set_cursor(0, 0);
        // "a" -> the empty line (its own word) -> "b": exclusive, lands on
        // 'b' without including it.
        let r = motion_range(&mut buf, Motion::WordForward, Some(2)).unwrap();
        assert_eq!(r.shape, MotionShape::Exclusive);
        assert_eq!(r.to, (2, 0));
        assert_eq!(extract_text(&buf, &r), "a\n\n");
    }

    #[test]
    fn find_matches_in_line_no_match() {
        let buf = TestBuffer::new("foo bar baz");
        assert_eq!(find_matches_in_line(&buf, 0, "xyz"), Vec::new());
    }

    #[test]
    fn find_matches_in_line_single_match() {
        let buf = TestBuffer::new("foo bar baz");
        assert_eq!(find_matches_in_line(&buf, 0, "bar"), vec![(4, 7)]);
    }

    #[test]
    fn find_matches_in_line_multiple_non_overlapping_matches() {
        let buf = TestBuffer::new("foo foo foo");
        assert_eq!(find_matches_in_line(&buf, 0, "foo"), vec![(0, 3), (4, 7), (8, 11)]);
    }

    #[test]
    fn find_matches_in_line_does_not_report_overlapping_matches() {
        let buf = TestBuffer::new("aaaa");
        assert_eq!(find_matches_in_line(&buf, 0, "aa"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn find_matches_in_line_empty_pattern_is_empty() {
        let buf = TestBuffer::new("foo bar");
        assert_eq!(find_matches_in_line(&buf, 0, ""), Vec::new());
    }

    #[test]
    fn find_matches_in_line_match_at_very_end_does_not_loop_forever() {
        let buf = TestBuffer::new("foobar");
        assert_eq!(find_matches_in_line(&buf, 0, "bar"), vec![(3, 6)]);
    }

    fn object_text(buf: &mut TestBuffer, cursor: (usize, usize), kind: TextObjectKind, around: bool, count: Option<usize>) -> Option<String> {
        buf.set_cursor(cursor.0, cursor.1);
        let range = text_object_range(buf, kind, around, count)?;
        Some(extract_text(buf, &range))
    }

    #[test]
    fn inner_and_around_word() {
        let mut buf = TestBuffer::new("foo bar baz");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Word, false, None), Some("foo".to_string()));
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Word, true, None), Some("foo ".to_string()));
        // no trailing whitespace at buffer end -- `aw` falls back to
        // leading whitespace instead.
        assert_eq!(object_text(&mut buf, (0, 9), TextObjectKind::Word, true, None), Some(" baz".to_string()));
    }

    #[test]
    fn word_object_on_punctuation_and_whitespace_runs() {
        let mut buf = TestBuffer::new("foo, bar");
        assert_eq!(object_text(&mut buf, (0, 3), TextObjectKind::Word, false, None), Some(",".to_string()));
        assert_eq!(object_text(&mut buf, (0, 4), TextObjectKind::Word, false, None), Some(" ".to_string()));
    }

    #[test]
    fn word_object_big_ignores_punctuation_boundaries() {
        let mut buf = TestBuffer::new("foo.bar baz");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Word, false, None), Some("foo".to_string()));
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::WordBig, false, None), Some("foo.bar".to_string()));
    }

    #[test]
    fn word_object_count_extends_through_following_chunks() {
        let mut buf = TestBuffer::new("foo bar baz");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Word, false, Some(3)), Some("foo bar".to_string()));
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Word, false, Some(4)), Some("foo bar ".to_string()));
    }

    #[test]
    fn quote_objects_inner_and_around() {
        let mut buf = TestBuffer::new(r#"say "hello world" now"#);
        assert_eq!(object_text(&mut buf, (0, 7), TextObjectKind::DoubleQuote, false, None), Some("hello world".to_string()));
        assert_eq!(object_text(&mut buf, (0, 7), TextObjectKind::DoubleQuote, true, None), Some(r#""hello world" "#.to_string()));
        // cursor before the quotes still finds them
        assert_eq!(object_text(&mut buf, (0, 0), TextObjectKind::DoubleQuote, false, None), Some("hello world".to_string()));
    }

    #[test]
    fn quote_object_empty_pair_has_no_inner_but_has_around() {
        let mut buf = TestBuffer::new(r#"x = "";"#);
        assert_eq!(object_text(&mut buf, (0, 5), TextObjectKind::DoubleQuote, false, None), None);
        // No whitespace trails the closing quote (`;` isn't blank), so `a"`
        // falls back to the leading whitespace before the opening quote --
        // same "prefer trailing, fall back to leading" rule as `aw`.
        assert_eq!(object_text(&mut buf, (0, 5), TextObjectKind::DoubleQuote, true, None), Some(r#" """#.to_string()));
    }

    #[test]
    fn quote_object_cursor_past_all_quotes_fails() {
        let mut buf = TestBuffer::new(r#""a" done"#);
        assert_eq!(object_text(&mut buf, (0, 6), TextObjectKind::DoubleQuote, false, None), None);
    }

    #[test]
    fn paren_object_inner_and_around() {
        let mut buf = TestBuffer::new("foo(bar, baz)qux");
        assert_eq!(object_text(&mut buf, (0, 6), TextObjectKind::Paren, false, None), Some("bar, baz".to_string()));
        assert_eq!(object_text(&mut buf, (0, 6), TextObjectKind::Paren, true, None), Some("(bar, baz)".to_string()));
    }

    #[test]
    fn paren_object_cursor_on_delimiter_counts_as_inside() {
        let mut buf = TestBuffer::new("(x)");
        assert_eq!(object_text(&mut buf, (0, 0), TextObjectKind::Paren, false, None), Some("x".to_string()));
        assert_eq!(object_text(&mut buf, (0, 2), TextObjectKind::Paren, false, None), Some("x".to_string()));
    }

    #[test]
    fn paren_object_empty_has_no_inner() {
        let mut buf = TestBuffer::new("f()");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Paren, false, None), None);
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Paren, true, None), Some("()".to_string()));
    }

    #[test]
    fn paren_object_nesting_and_count_widens_outward() {
        let mut buf = TestBuffer::new("(a (b) c)");
        assert_eq!(object_text(&mut buf, (0, 4), TextObjectKind::Paren, false, None), Some("b".to_string()));
        assert_eq!(object_text(&mut buf, (0, 4), TextObjectKind::Paren, false, Some(2)), Some("a (b) c".to_string()));
    }

    #[test]
    fn paren_object_spans_multiple_lines() {
        // The open/close brackets themselves sit at the very end of line 0
        // and the very start of line 2 -- `step_forward`/`step_backward`
        // never land on the (virtual) newline between them, so the inner
        // range is exactly line 1's own content, same convention every
        // other line-crossing motion in this module already follows.
        let mut buf = TestBuffer::new("foo(\nbar\n)baz");
        assert_eq!(object_text(&mut buf, (1, 1), TextObjectKind::Paren, false, None), Some("bar".to_string()));
    }

    #[test]
    fn brace_and_bracket_and_angle_objects() {
        let mut buf = TestBuffer::new("{a}[b]<c>");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Brace, false, None), Some("a".to_string()));
        assert_eq!(object_text(&mut buf, (0, 4), TextObjectKind::Bracket, false, None), Some("b".to_string()));
        assert_eq!(object_text(&mut buf, (0, 7), TextObjectKind::Angle, false, None), Some("c".to_string()));
    }

    #[test]
    fn paragraph_object_inner_and_around() {
        let mut buf = TestBuffer::new("one\ntwo\n\nthree\nfour\n\nfive");
        assert_eq!(object_text(&mut buf, (0, 0), TextObjectKind::Paragraph, false, None), Some("one\ntwo\n".to_string()));
        assert_eq!(object_text(&mut buf, (0, 0), TextObjectKind::Paragraph, true, None), Some("one\ntwo\n\n".to_string()));
        assert_eq!(object_text(&mut buf, (3, 0), TextObjectKind::Paragraph, false, None), Some("three\nfour\n".to_string()));
        // last paragraph has no trailing blank run -- `ap` falls back to
        // the leading one instead.
        assert_eq!(object_text(&mut buf, (6, 0), TextObjectKind::Paragraph, true, None), Some("\nfive\n".to_string()));
    }

    #[test]
    fn sentence_object_inner_and_around() {
        let mut buf = TestBuffer::new("One sentence. Two sentence. Three.");
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Sentence, false, None), Some("One sentence.".to_string()));
        assert_eq!(object_text(&mut buf, (0, 1), TextObjectKind::Sentence, true, None), Some("One sentence. ".to_string()));
        assert_eq!(object_text(&mut buf, (0, 14), TextObjectKind::Sentence, false, None), Some("Two sentence.".to_string()));
        assert_eq!(object_text(&mut buf, (0, 29), TextObjectKind::Sentence, false, None), Some("Three.".to_string()));
    }

    #[test]
    fn word_object_line_gets_left_untouched_by_text_object_variant_in_motion_shape() {
        // `TextObject` is never dispatched through `apply_motion`'s normal
        // callers in this crate (see the variant's own doc comment), but it
        // must still be handled correctly for `motion_shape`/`apply_motion`
        // to stay total -- exercised here directly rather than left
        // entirely to the "never reached" claim.
        let mut buf = TestBuffer::new("foo bar");
        buf.set_cursor(0, 5);
        apply_motion(&mut buf, Motion::TextObject(TextObjectKind::Word, false), None);
        assert_eq!(buf.cursor(), (0, 4));
    }

    #[test]
    fn case_transform_lower_upper_toggle() {
        assert_eq!(case_transform('a', CaseKind::Lower), 'a');
        assert_eq!(case_transform('A', CaseKind::Lower), 'a');
        assert_eq!(case_transform('a', CaseKind::Upper), 'A');
        assert_eq!(case_transform('A', CaseKind::Upper), 'A');
        assert_eq!(case_transform('a', CaseKind::Toggle), 'A');
        assert_eq!(case_transform('A', CaseKind::Toggle), 'a');
    }

    #[test]
    fn case_transform_leaves_non_letters_untouched() {
        assert_eq!(case_transform('5', CaseKind::Toggle), '5');
        assert_eq!(case_transform(' ', CaseKind::Upper), ' ');
        assert_eq!(case_transform('_', CaseKind::Lower), '_');
    }

    #[test]
    fn find_number_at_cursor_and_after_cursor() {
        let mut buf = TestBuffer::new("abc 42 def");
        buf.set_cursor(0, 5); // on the '2' of "42"
        let m = find_number(&buf, buf.cursor()).unwrap();
        assert_eq!((m.from, m.to, m.value, m.width, m.leading_zero), ((0, 4), (0, 5), 42, 2, false));

        buf.set_cursor(0, 0); // before the number -- scans forward
        let m = find_number(&buf, buf.cursor()).unwrap();
        assert_eq!((m.from, m.to, m.value, m.width, m.leading_zero), ((0, 4), (0, 5), 42, 2, false));
    }

    #[test]
    fn find_number_detects_a_genuine_leading_zero() {
        let buf = TestBuffer::new("id 007 done");
        let m = find_number(&buf, (0, 0)).unwrap();
        assert_eq!((m.value, m.width, m.leading_zero), (7, 3, true));
    }

    #[test]
    fn find_number_no_digits_on_the_line_is_none() {
        let buf = TestBuffer::new("no digits here");
        assert!(find_number(&buf, (0, 0)).is_none());
    }

    #[test]
    fn find_number_never_crosses_a_line_break() {
        let mut buf = TestBuffer::new("abc\n42");
        buf.set_cursor(0, 0);
        assert!(find_number(&buf, buf.cursor()).is_none());
    }

    #[test]
    fn find_number_includes_a_leading_minus_sign() {
        let buf = TestBuffer::new("x = -17;");
        let m = find_number(&buf, (0, 6)).unwrap();
        assert_eq!((m.from, m.to, m.value, m.width), ((0, 4), (0, 6), -17, 2));
    }

    #[test]
    fn apply_number_delta_increments_and_decrements() {
        let m = NumberMatch { from: (0, 0), to: (0, 1), value: 42, width: 2, leading_zero: false };
        assert_eq!(apply_number_delta(&m, 1), "43");
        assert_eq!(apply_number_delta(&m, -1), "41");
        // An ordinary (non-zero-padded) number never gets padded back
        // out, even once it shrinks below its own original width.
        assert_eq!(apply_number_delta(&m, -42), "0");
        assert_eq!(apply_number_delta(&m, -50), "-8");
    }

    #[test]
    fn apply_number_delta_preserves_leading_zero_width() {
        let m = NumberMatch { from: (0, 0), to: (0, 2), value: 7, width: 3, leading_zero: true };
        assert_eq!(apply_number_delta(&m, 1), "008");
        // growing past the original width drops the padding, matching vim.
        let m = NumberMatch { from: (0, 0), to: (0, 2), value: 999, width: 3, leading_zero: true };
        assert_eq!(apply_number_delta(&m, 1), "1000");
    }

    #[test]
    fn surround_target_kind_maps_every_trigger_including_aliases() {
        assert_eq!(surround_target_kind('('), Some(TextObjectKind::Paren));
        assert_eq!(surround_target_kind(')'), Some(TextObjectKind::Paren));
        assert_eq!(surround_target_kind('b'), Some(TextObjectKind::Paren));
        assert_eq!(surround_target_kind('{'), Some(TextObjectKind::Brace));
        assert_eq!(surround_target_kind('}'), Some(TextObjectKind::Brace));
        assert_eq!(surround_target_kind('B'), Some(TextObjectKind::Brace));
        assert_eq!(surround_target_kind('['), Some(TextObjectKind::Bracket));
        assert_eq!(surround_target_kind(']'), Some(TextObjectKind::Bracket));
        assert_eq!(surround_target_kind('r'), Some(TextObjectKind::Bracket));
        assert_eq!(surround_target_kind('<'), Some(TextObjectKind::Angle));
        assert_eq!(surround_target_kind('>'), Some(TextObjectKind::Angle));
        assert_eq!(surround_target_kind('"'), Some(TextObjectKind::DoubleQuote));
        assert_eq!(surround_target_kind('\''), Some(TextObjectKind::SingleQuote));
        assert_eq!(surround_target_kind('`'), Some(TextObjectKind::Backtick));
        // `w`/`s`/`p` name text objects but never a surround target --
        // ds/cs only ever act on pair-shaped delimiters.
        assert_eq!(surround_target_kind('w'), None);
        assert_eq!(surround_target_kind('s'), None);
        assert_eq!(surround_target_kind('p'), None);
    }

    #[test]
    fn surround_delims_pads_the_opening_bracket_variant_only() {
        assert_eq!(surround_delims('('), Some(("( ".to_string(), " )".to_string())));
        assert_eq!(surround_delims(')'), Some(("(".to_string(), ")".to_string())));
        assert_eq!(surround_delims('b'), Some(("( ".to_string(), " )".to_string())));
        assert_eq!(surround_delims('{'), Some(("{ ".to_string(), " }".to_string())));
        assert_eq!(surround_delims('}'), Some(("{".to_string(), "}".to_string())));
        assert_eq!(surround_delims('['), Some(("[ ".to_string(), " ]".to_string())));
        assert_eq!(surround_delims(']'), Some(("[".to_string(), "]".to_string())));
        assert_eq!(surround_delims('<'), Some(("< ".to_string(), " >".to_string())));
        assert_eq!(surround_delims('>'), Some(("<".to_string(), ">".to_string())));
    }

    #[test]
    fn surround_delims_quotes_and_literal_fallback_never_pad() {
        assert_eq!(surround_delims('"'), Some(("\"".to_string(), "\"".to_string())));
        assert_eq!(surround_delims('\''), Some(("'".to_string(), "'".to_string())));
        assert_eq!(surround_delims('`'), Some(("`".to_string(), "`".to_string())));
        assert_eq!(surround_delims('*'), Some(("*".to_string(), "*".to_string())));
        assert_eq!(surround_delims('_'), Some(("_".to_string(), "_".to_string())));
    }

    #[test]
    fn surround_delims_rejects_letters_and_whitespace() {
        // Only `b`/`B`/`r` are recognized letter aliases -- anything else
        // (a typo, or vim-surround's own unsupported tag/function-call
        // triggers) is rejected rather than silently becoming a literal
        // letter wrap.
        assert_eq!(surround_delims('t'), None);
        assert_eq!(surround_delims('x'), None);
        assert_eq!(surround_delims(' '), None);
    }

    #[test]
    fn surround_pair_positions_finds_the_enclosing_bracket_pair() {
        let mut buf = TestBuffer::new("foo (bar) baz");
        buf.set_cursor(0, 6);
        assert_eq!(surround_pair_positions(&buf, TextObjectKind::Paren), Some(((0, 4), (0, 8))));
        buf.set_cursor(0, 0);
        assert_eq!(surround_pair_positions(&buf, TextObjectKind::Paren), None);
    }

    #[test]
    fn surround_pair_positions_finds_the_enclosing_quote_pair() {
        let mut buf = TestBuffer::new(r#"say "hello" now"#);
        buf.set_cursor(0, 7);
        assert_eq!(surround_pair_positions(&buf, TextObjectKind::DoubleQuote), Some(((0, 4), (0, 10))));
    }

    #[test]
    fn surround_pair_positions_word_kinds_are_never_valid_surround_targets() {
        let mut buf = TestBuffer::new("foo bar");
        buf.set_cursor(0, 1);
        assert_eq!(surround_pair_positions(&buf, TextObjectKind::Word), None);
        assert_eq!(surround_pair_positions(&buf, TextObjectKind::Paragraph), None);
    }

    #[test]
    fn surround_delete_spans_strips_one_adjacent_pad_space_for_brackets() {
        let buf = TestBuffer::new("( foo )");
        let (open, close) = surround_delete_spans(&buf, TextObjectKind::Paren, (0, 0), (0, 6));
        assert_eq!(open, MotionRange { shape: MotionShape::Inclusive, from: (0, 0), to: (0, 1) });
        assert_eq!(close, MotionRange { shape: MotionShape::Inclusive, from: (0, 5), to: (0, 6) });
    }

    #[test]
    fn surround_delete_spans_leaves_a_tight_bracket_pair_untouched() {
        let buf = TestBuffer::new("(foo)");
        let (open, close) = surround_delete_spans(&buf, TextObjectKind::Paren, (0, 0), (0, 4));
        assert_eq!(open, MotionRange { shape: MotionShape::Inclusive, from: (0, 0), to: (0, 0) });
        assert_eq!(close, MotionRange { shape: MotionShape::Inclusive, from: (0, 4), to: (0, 4) });
    }

    #[test]
    fn surround_delete_spans_never_strips_padding_for_quotes() {
        let buf = TestBuffer::new(r#"" foo ""#);
        let (open, close) = surround_delete_spans(&buf, TextObjectKind::DoubleQuote, (0, 0), (0, 6));
        assert_eq!(open, MotionRange { shape: MotionShape::Inclusive, from: (0, 0), to: (0, 0) });
        assert_eq!(close, MotionRange { shape: MotionShape::Inclusive, from: (0, 6), to: (0, 6) });
    }

    #[test]
    fn surround_delete_spans_a_single_shared_pad_space_is_claimed_only_once() {
        // "( )" -- the lone character between the brackets would
        // otherwise be claimed as padding by both sides at once.
        let buf = TestBuffer::new("( )");
        let (open, close) = surround_delete_spans(&buf, TextObjectKind::Paren, (0, 0), (0, 2));
        assert_eq!(open, MotionRange { shape: MotionShape::Inclusive, from: (0, 0), to: (0, 0) });
        assert_eq!(close, MotionRange { shape: MotionShape::Inclusive, from: (0, 1), to: (0, 2) });
    }

    #[test]
    fn surround_insert_points_inclusive_wraps_exactly_the_range() {
        let buf = TestBuffer::new("foo bar baz");
        let range = MotionRange { shape: MotionShape::Inclusive, from: (0, 4), to: (0, 6) };
        assert_eq!(surround_insert_points(&buf, &range), ((0, 4), (0, 7)));
    }

    #[test]
    fn surround_insert_points_exclusive_wraps_up_to_but_not_including_to() {
        let buf = TestBuffer::new("foo bar baz");
        let range = MotionRange { shape: MotionShape::Exclusive, from: (0, 4), to: (0, 7) };
        assert_eq!(surround_insert_points(&buf, &range), ((0, 4), (0, 7)));
    }

    #[test]
    fn surround_insert_points_linewise_skips_leading_indentation() {
        let buf = TestBuffer::new("  foo bar");
        let range = MotionRange { shape: MotionShape::Linewise, from: (0, 0), to: (0, 0) };
        assert_eq!(surround_insert_points(&buf, &range), ((0, 2), (0, 9)));
    }
}
