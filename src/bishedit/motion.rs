use super::Buffer;
use super::grapheme;
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
    GotoColumn,         // | (count is the 1-indexed target column, default 1)
    GotoFirstLine,      // gg (count is the 1-indexed target line, default first)
    GotoLastLine,       // G  (count is the 1-indexed target line, default last)
    GotoPercent,        // {count}% (only emitted when a count precedes '%' -- see vimkeys.rs)
    WordForward,        // w
    WordForwardBig,     // W
    WordBackward,       // b
    WordBackwardBig,    // B
    WordEnd,            // e
    WordEndBig,         // E
    WordEndBackward,    // ge
    WordEndBackwardBig, // gE
    FindChar {
        // `f F t T`. vimkeys repeats them with `;` and `,`.
        ch: char,
        till: bool,
        forward: bool,
    },
    ScreenTop,          // H (count = line offset from the top, default 1)
    ScreenMiddle,       // M (count ignored)
    ScreenBottom,       // L (count = line offset from the bottom, default 1)
    HalfPageDown,       // Ctrl-D
    HalfPageUp,         // Ctrl-U
    PageDown,           // Ctrl-F
    PageUp,             // Ctrl-B
    ScrollLineDown,     // Ctrl-E (count = lines to scroll, default 1)
    ScrollLineUp,       // Ctrl-Y (count = lines to scroll, default 1)
    ScrollCenter,       // zz (count ignored; centers the current line)
    ScrollTop,          // zt (count ignored; current line becomes the top)
    ScrollBottom,       // zb (count ignored; current line becomes the bottom)
    ParagraphForward,   // }
    ParagraphBackward,  // {
    SentenceForward,    // )
    SentenceBackward,   // (
    NextLineNonBlank,   // + or Enter
    PrevLineNonBlank,   // -
    MatchPair,          // %
    SetMark(char),      // m{a-z}
    GotoMark(char),     // `{mark}
    GotoMarkLine(char), // '{mark}
    // vimkeys repeats searches with `n` and `N`, re-issuing the last one
    // (flipping direction for `N`). For `*` and `#` that works because the
    // word under the cursor after a match is the word that was searched for.
    SearchForward(String),  // /pattern<Enter>
    SearchBackward(String), // ?pattern<Enter>
    SearchWordForward,      // *
    SearchWordBackward,     // #
    // `g*`/`g#`: like `*`/`#` without the word-boundary requirement, so
    // `foobar` matches when the word under the cursor is `foo`.
    SearchWordForwardUnbounded,
    SearchWordBackwardUnbounded,
    UnmatchedOpenParen,  // [(
    UnmatchedCloseParen, // ])
    UnmatchedOpenBrace,  // [{
    UnmatchedCloseBrace, // ]}
    SectionForward,      // ]] (next line starting with '{')
    SectionForwardEnd,   // ][ (next line starting with '}')
    SectionBackward,     // [[ (previous line starting with '{')
    SectionBackwardEnd,  // [] (previous line starting with '}')
    // `iw`, `aw`, `i(`, `a"` and the rest: text objects, only ever an
    // operator's target. The cursor usually sits inside the range, so
    // `motion_range` can't diff the cursor before and after, and calls
    // `text_object_range` directly. The `bool` is `around` (`a`) vs inner
    // (`i`).
    TextObject(TextObjectKind, bool),
    /// `zj`: down to where the next fold starts.
    FoldNextStart,
    /// `zk`: up to where the previous fold ends.
    FoldPreviousEnd,
    /// `[z`: to the start of the open fold the cursor is in.
    FoldStart,
    /// `]z`: to its end.
    FoldEnd,
}

/// The object an `i`/`a` text object names. `b` and `B` are vim's aliases
/// for `(` and `{`. No tag objects (`it`/`at`).
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

// Whether `c` is part of a word, given the buffer's `iskeyword` (see
// `Buffer::word_chars`). Letters and digits always are.
fn is_word_char(c: char, extra: &str) -> bool {
    c.is_alphanumeric() || extra.contains(c)
}

/// Which way `~`, `gu`, `gU` and `g~` change case. Non-letters are left
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseKind {
    Lower,
    Upper,
    Toggle,
}

/// One character's case change, which every case command is built from.
/// ASCII only, like `r`: a full Unicode case mapping can turn one character
/// into several (`ß` to `SS`), which these commands can't take. Non-ASCII
/// letters pass through.
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

fn classify(buf: &dyn Buffer, line: usize, col: usize, big: bool) -> Class {
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

// The rightmost column a Normal-mode cursor can sit at on `line`, the
// clamp nearly every motion uses. If the last char belongs to a
// multi-char cluster (a ZWJ emoji sequence), that's the cluster's start,
// so the cursor never lands inside a glyph.
fn last_col(buf: &dyn Buffer, line: usize) -> usize {
    let len = buf.line_len(line);
    if len == 0 {
        return 0;
    }
    let chars = buf.line_chars(line);
    grapheme::prev_boundary(&chars, len)
}

pub fn first_non_blank(buf: &dyn Buffer, line: usize) -> usize {
    for col in 0..buf.line_len(line) {
        if let Some(c) = buf.char_at(line, col) {
            if !c.is_whitespace() {
                return col;
            }
        }
    }
    0
}

fn last_non_blank(buf: &dyn Buffer, line: usize) -> usize {
    for col in (0..buf.line_len(line)).rev() {
        if let Some(c) = buf.char_at(line, col) {
            if !c.is_whitespace() {
                return col;
            }
        }
    }
    0
}

/// `$`'s target row: past every `line_wraps` row to the end of the
/// logical line. For an ordinary buffer, just `line`.
fn logical_line_end(buf: &dyn Buffer, line: usize) -> usize {
    let mut l = line;
    while buf.line_wraps(l) && l + 1 < buf.line_count() {
        l += 1;
    }
    l
}

/// One character forward, skipping the slot past a line's last character
/// straight to the next line's column 0: vim's word motions treat the line
/// break itself as the boundary.
fn step_forward(buf: &dyn Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
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

/// The mirror of `step_forward`: back from column 0 to the previous line's
/// last character (or column 0 if it's empty).
fn step_backward(buf: &dyn Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
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

fn word_forward_once(buf: &dyn Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
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

fn word_backward_once(buf: &dyn Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
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

fn word_end_forward_once(buf: &dyn Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
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

/// `ge`/`gE`: end of the previous word. With none (the cursor is in the
/// buffer's first word) the cursor stays, as vim fails the motion.
fn word_end_backward_once(buf: &dyn Buffer, pos: (usize, usize), big: bool) -> (usize, usize) {
    let (oline, ocol) = pos;
    let orig_is_word = buf.line_len(oline) > 0 && classify(buf, oline, ocol, big) != Class::Blank;
    let orig_class = if orig_is_word { Some(classify(buf, oline, ocol, big)) } else { None };
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

fn find_char_forward_once(buf: &dyn Buffer, line: usize, from_col: usize, ch: char) -> Option<usize> {
    ((from_col + 1)..buf.line_len(line)).find(|&c| buf.char_at(line, c) == Some(ch))
}

fn find_char_backward_once(buf: &dyn Buffer, line: usize, from_col: usize, ch: char) -> Option<usize> {
    (0..from_col).rev().find(|&c| buf.char_at(line, c) == Some(ch))
}

/// Where `f`/`F`/`t`/`T` would land, or `None` with no `n`th `ch` that
/// way. Separate from `apply_motion` because `motion_range` needs to know
/// whether the find succeeded, which isn't whether the cursor moved (see
/// `dt,` there).
fn find_char_landing(buf: &dyn Buffer, line: usize, col: usize, ch: char, till: bool, forward: bool, n: usize) -> Option<usize> {
    let mut c = col;
    for _ in 0..n {
        c = if forward { find_char_forward_once(buf, line, c, ch)? } else { find_char_backward_once(buf, line, c, ch)? };
    }
    Some(if till { if forward { c.saturating_sub(1) } else { c + 1 } } else { c })
}

fn is_sentence_end_punct(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
}

fn is_closing_punct(c: char) -> bool {
    matches!(c, ')' | ']' | '"' | '\'')
}

fn is_blank_line(buf: &dyn Buffer, line: usize) -> bool {
    buf.line_len(line) == 0
}

/// `pos` holds sentence-ending punctuation. Returns the position after it
/// (and any closing quotes or brackets on the same line) if that's
/// whitespace, end of line or end of buffer, i.e. `pos` ends a sentence;
/// `None` otherwise (`3.14`).
fn sentence_boundary_after(buf: &dyn Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
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

fn skip_blank_forward(buf: &dyn Buffer, pos: (usize, usize)) -> (usize, usize) {
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

fn sentence_forward_once(buf: &dyn Buffer, pos: (usize, usize)) -> (usize, usize) {
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

/// Sentence starts in document order, from repeated
/// `sentence_forward_once`. `(` uses this list rather than a mirrored
/// backward scan, which would be harder to keep correct.
fn sentence_starts(buf: &dyn Buffer) -> Vec<(usize, usize)> {
    let first = if buf.line_len(0) == 0 { (0, 0) } else { (0, first_non_blank(buf, 0)) };
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

fn sentence_backward_once(buf: &dyn Buffer, pos: (usize, usize)) -> (usize, usize) {
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

fn paragraph_forward_once(buf: &dyn Buffer, line: usize) -> usize {
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
    if l < last { l + 1 } else { last }
}

fn paragraph_backward_once(buf: &dyn Buffer, line: usize) -> usize {
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
    if l > 0 { l - 1 } else { 0 }
}

fn viewport_bottom(buf: &dyn Buffer) -> usize {
    (buf.viewport_top() + buf.viewport_height().saturating_sub(1)).min(buf.line_count() - 1)
}

/// For a bracket: its partner, and whether `c` is the opening one.
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

/// `%`: off a bracket, finds the first one on the line (as vim does), then
/// walks the buffer tracking depth to its partner.
fn match_pair_once(buf: &dyn Buffer, pos: (usize, usize)) -> Option<(usize, usize)> {
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
    partner_of(buf, start?, None)
}

// How far `bracket_pair_at` looks. `%` has no budget, since it's typed
// once, but the mark is computed on every redraw, where an unmatched
// bracket would otherwise be chased through the whole file.
const MATCH_LIMIT: usize = 20_000;

// The bracket that closes the one at `start`, or opens it for a closing
// one. `None` when there's none, or none within `budget` characters.
fn partner_of(buf: &dyn Buffer, start: (usize, usize), budget: Option<usize>) -> Option<(usize, usize)> {
    let ch0 = buf.char_at(start.0, start.1)?;
    let (partner_char, is_opening) = bracket_partner(ch0)?;
    let mut depth = 1;
    let mut cur = start;
    let mut steps = 0;
    loop {
        cur = if is_opening { step_forward(buf, cur)? } else { step_backward(buf, cur)? };
        steps += 1;
        if budget.is_some_and(|budget| steps > budget) {
            return None;
        }
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

/// The bracket under the cursor and its partner, for showing both, as vim
/// does. Not `%`'s rule: vim marks nothing unless the cursor is on a
/// bracket. `None` when unmatched or past `MATCH_LIMIT`.
pub fn bracket_pair_at(buf: &dyn Buffer, pos: (usize, usize)) -> Option<((usize, usize), (usize, usize))> {
    bracket_partner(buf.char_at(pos.0, pos.1)?)?;
    Some((pos, partner_of(buf, pos, Some(MATCH_LIMIT))?))
}

/// The word at or after `pos`, advancing to the next word if `pos` isn't
/// on one, as `*` and `#` search from the nearest word forward. Public so
/// search highlighting can recover a `*` search's pattern: the word under
/// the cursor after a match is the word searched for.
pub fn word_under_cursor(buf: &dyn Buffer, pos: (usize, usize)) -> Option<String> {
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
    while end_col + 1 < buf.line_len(line) && matches!(buf.char_at(line, end_col + 1), Some(c) if is_word_char(c, buf.word_chars())) {
        end_col += 1;
    }
    Some((start_col..=end_col).filter_map(|c| buf.char_at(line, c)).collect())
}

/// The chunk (run of one `Class`) at `col`, extended `count - 1` chunks
/// forward: vim's `[count]iw` is the word, punctuation run or blank run
/// under the cursor plus the next `count - 1` chunks of any class.
fn word_object_range(buf: &dyn Buffer, pos: (usize, usize), big: bool, around: bool, count: Option<usize>) -> Option<MotionRange> {
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

/// The two `quote` characters `quote_object_range` works around, for
/// `ds"`/`cs"`, which act on the delimiters alone.
fn quote_pair_positions(buf: &dyn Buffer, pos: (usize, usize), quote: char) -> Option<((usize, usize), (usize, usize))> {
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

/// `i"`, `a'`, `` i` `` and the rest, on one line only, as in vim. The
/// line's quotes pair up left to right (1st and 2nd, 3rd and 4th, ...), and
/// the first pair the cursor is at or before the close of is chosen: inside
/// a pair selects it, before all pairs the first, between two the next.
fn quote_object_range(buf: &dyn Buffer, pos: (usize, usize), quote: char, around: bool) -> Option<MotionRange> {
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
        // `""` has nothing between: `di"` does nothing, as in vim.
        None
    }
}

/// Forward from just after `open_pos` (which holds `open`), tracking
/// depth, to its matching `close`.
fn scan_matching_forward(buf: &dyn Buffer, open_pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
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

/// Backward from just before `close_pos` to its matching `open`.
fn scan_matching_backward(buf: &dyn Buffer, close_pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
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

/// Backward from `pos` (not itself a bracket) to the nearest `open` not
/// balanced by a `close` on the way: the innermost enclosing bracket.
fn scan_unmatched_open_backward(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
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

/// The innermost bracket pair enclosing `pos`; sitting on either bracket
/// counts, as for `%`.
fn find_enclosing_pair(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char) -> Option<((usize, usize), (usize, usize))> {
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

/// Forward from `pos` to the nearest unbalanced `close`, for `])`/`]}`.
fn scan_unmatched_close_forward(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char) -> Option<(usize, usize)> {
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

/// `[(`/`[{`: `count` unmatched opening brackets back from the cursor.
/// Each repeat starts before the last find, since the scan never looks at
/// `pos` itself.
fn unmatched_backward(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char, count: usize) -> Option<(usize, usize)> {
    let mut cur = pos;
    let mut found = None;
    for _ in 0..count.max(1) {
        cur = scan_unmatched_open_backward(buf, cur, open, close)?;
        found = Some(cur);
    }
    found
}

/// `])`/`]}`: `count` unmatched closing brackets forward from the cursor.
fn unmatched_forward(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char, count: usize) -> Option<(usize, usize)> {
    let mut cur = pos;
    let mut found = None;
    for _ in 0..count.max(1) {
        cur = scan_unmatched_close_forward(buf, cur, open, close)?;
        found = Some(cur);
    }
    found
}

/// A line starting with `ch`: vim's default section boundary, `{` opening
/// one and `}` closing one. No 'sections' option or form feeds.
fn line_starts_with(buf: &dyn Buffer, line: usize, ch: char) -> bool {
    buf.line_len(line) > 0 && buf.char_at(line, 0) == Some(ch)
}

/// `]]`/`][`: the next line starting with `boundary`, or the last line
/// when there's none, like `paragraph_forward_once`.
fn section_forward_once(buf: &dyn Buffer, line: usize, boundary: char) -> usize {
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
fn section_backward_once(buf: &dyn Buffer, line: usize, boundary: char) -> usize {
    let mut l = line;
    while l > 0 {
        l -= 1;
        if line_starts_with(buf, l, boundary) {
            return l;
        }
    }
    0
}

/// `i(`, `a{`, `ib` and the rest: across lines, via `find_enclosing_pair`.
/// Each extra count widens one enclosing level, as `2i(` does in vim.
fn pair_object_range(buf: &dyn Buffer, pos: (usize, usize), open: char, close: char, around: bool, count: Option<usize>) -> Option<MotionRange> {
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
            // `()` has nothing between: `di(` does nothing, as in vim.
            None
        } else {
            Some(MotionRange { shape: MotionShape::Inclusive, from: inner_start, to: inner_end })
        }
    }
}

/// `ip`/`ap`: the run of lines that are blank or not like the cursor's,
/// plus `count - 1` more runs. `around` takes one trailing blank run, or a
/// leading one when there's none after (the buffer's last paragraph), as
/// vim does.
fn paragraph_object_range(buf: &dyn Buffer, pos: (usize, usize), around: bool, count: Option<usize>) -> Option<MotionRange> {
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

/// `is`/`as`: the sentence containing the cursor, plus `count - 1` more.
/// `around` runs through the whitespace to the next sentence's start (or
/// the buffer's end); inner stops at the last non-blank.
fn sentence_object_range(buf: &dyn Buffer, pos: (usize, usize), around: bool, count: Option<usize>) -> Option<MotionRange> {
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
    // `sentence_starts` can end with a start on the buffer's last character,
    // where `sentence_forward_once` clamps. A sentence can't end and begin on
    // one character, so that's no next sentence.
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

/// Resolves a text object against the cursor without moving it; see
/// `Motion::TextObject` for why `motion_range` calls this directly.
pub fn text_object_range(buf: &dyn Buffer, kind: TextObjectKind, around: bool, count: Option<usize>) -> Option<MotionRange> {
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

/// vim-surround's `ds`/`cs` target: which pair kind a character names.
/// Only kinds with single-character delimiters (`w`, `s`, `p` aren't
/// targets in the plugin either). `b`, `B` and `r` alias `(`, `{` and `[`,
/// and either half of a pair names it.
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

/// The delimiters vim-surround inserts for a character. An opening
/// bracket (`(`, `{`, `[`, `<`, or `b`/`B`/`r`) pads with spaces
/// (`ysiw(` gives `( word )`), its closing one doesn't, and quotes never
/// do. Any other printable non-alphanumeric character is its own unpadded
/// pair. Other letters are rejected, unlike in vim-surround, so a typo for
/// an unsupported tag or function surround (`t`, `f`) fails instead of
/// wrapping in letters.
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

/// The delimiter pair `ds`/`cs` acts on: `find_enclosing_pair` for
/// brackets, `quote_pair_positions` for quotes (one line only, as in vim).
/// Word, sentence and paragraph never get here (see `surround_target_kind`)
/// but are listed, so a new pair kind can't slip past.
pub fn surround_pair_positions(buf: &dyn Buffer, kind: TextObjectKind) -> Option<((usize, usize), (usize, usize))> {
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

/// What `ds` deletes on each side of a pair: the delimiter, plus one
/// adjacent space inside a bracket kind, as vim-surround strips the padding
/// `ys` adds. Returned as (open, close); delete the close side first so
/// the open side doesn't shift. In `( )` the single space goes to the
/// close side.
pub fn surround_delete_spans(
    buf: &dyn Buffer,
    kind: TextObjectKind,
    open_pos: (usize, usize),
    close_pos: (usize, usize),
) -> (MotionRange, MotionRange) {
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

/// Where `ys`, `yss` and Visual `S` insert each half of a new pair around
/// `range`, close first, so inserting there never shifts `from`. `Linewise`
/// is `yss`'s: from the first non-blank of the first line to the end of the
/// last, ignoring indentation as vim-surround does.
pub fn surround_insert_points(buf: &dyn Buffer, range: &MotionRange) -> ((usize, usize), (usize, usize)) {
    match range.shape {
        MotionShape::Linewise => {
            let open_at = (range.from.0, first_non_blank(buf, range.from.0));
            let close_at = (range.to.0, buf.line_len(range.to.0));
            (open_at, close_at)
        }
        // A block has no surround in vim either; taken as the span between
        // its corners.
        MotionShape::Inclusive | MotionShape::Blockwise => (range.from, (range.to.0, range.to.1 + 1)),
        MotionShape::Exclusive => (range.from, range.to),
    }
}

/// `Ctrl-A`/`Ctrl-X`'s target: a decimal number at or after the cursor.
/// `from` includes a leading `-`, `to` is the last digit, `width` counts
/// digits. Padding back to `width` happens only with `leading_zero` (`007`):
/// vim doesn't pad `42 - 42` to `00`.
pub struct NumberMatch {
    pub from: (usize, usize),
    pub to: (usize, usize),
    pub value: i64,
    pub width: usize,
    pub leading_zero: bool,
}

/// From `pos` along the line to the first digit (`pos` counts, so a
/// cursor inside a number finds it), then out to the number's extent: vim's
/// `Ctrl-A` rule. Decimal only, no `nrformats`. `None` with no digit ahead,
/// or a number that doesn't fit an `i64`.
pub fn find_number(buf: &dyn Buffer, pos: (usize, usize)) -> Option<NumberMatch> {
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

/// `m`'s replacement after adding `delta`: zero-padded to its width when it
/// was zero-padded, and re-signed if it crossed zero.
pub fn apply_number_delta(m: &NumberMatch, delta: i64) -> String {
    let new_value = m.value.saturating_add(delta);
    let digits = new_value.unsigned_abs().to_string();
    let padded = if m.leading_zero && digits.len() < m.width { format!("{}{digits}", "0".repeat(m.width - digits.len())) } else { digits };
    if new_value < 0 { format!("-{padded}") } else { padded }
}

/// Regex search (`crate::regex`) for `/` and `?`, and with an escaped
/// literal for `*`, `#`, `g*` and `g#`, so normal mode has one search
/// engine. Matches never span lines.
fn line_find(buf: &dyn Buffer, line: usize, lower_bound: usize, re: &Regex) -> Option<usize> {
    let chars = buf.line_chars(line);
    re.find_at(&chars, lower_bound).map(|(start, _end)| start)
}

/// Every non-overlapping match of `pattern` on `line`, left to right, as
/// vim's `hlsearch` finds them (`aa` in `aaaa` at 0 and 2), for match
/// highlighting.
pub fn find_matches_in_line(buf: &dyn Buffer, line: usize, pattern: &str) -> Vec<(usize, usize)> {
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

fn line_rfind(buf: &dyn Buffer, line: usize, upper_bound: usize, re: &Regex) -> Option<usize> {
    let chars = buf.line_chars(line);
    let upper = upper_bound.min(chars.len());
    (0..upper).rev().find(|&start| re.match_at(&chars, start).is_some())
}

/// Wrapping forward search, vim's default `wrapscan`: the rest of the line,
/// the lines after, then back round to the start of this one.
fn search_forward_once(buf: &dyn Buffer, pos: (usize, usize), re: &Regex) -> Option<(usize, usize)> {
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

fn search_backward_once(buf: &dyn Buffer, pos: (usize, usize), re: &Regex) -> Option<(usize, usize)> {
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

fn is_word_boundary_at(buf: &dyn Buffer, line: usize, col: usize) -> bool {
    !matches!(buf.char_at(line, col), Some(c) if is_word_char(c, buf.word_chars()))
}

/// `*`/`#` with word boundaries. `re` is an escaped literal word, so
/// `word_len` is the match length for the boundary checks.
fn search_word_forward_once(buf: &dyn Buffer, pos: (usize, usize), re: &Regex, word_len: usize) -> Option<(usize, usize)> {
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

fn search_word_backward_once(buf: &dyn Buffer, pos: (usize, usize), re: &Regex, word_len: usize) -> Option<(usize, usize)> {
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

/// The line `n` rows below `line`, a closed fold counting as one row,
/// never past the last line.
pub fn rows_down(buf: &dyn Buffer, line: usize, n: usize) -> usize {
    let last = buf.line_count().saturating_sub(1);
    let mut at = line;
    for _ in 0..n {
        let end = buf.fold_span(at).1;
        if end >= last {
            break;
        }
        at = end + 1;
    }
    buf.fold_span(at).0
}

/// The line `n` rows above `line`, a closed fold counting as one row.
pub fn rows_up(buf: &dyn Buffer, line: usize, n: usize) -> usize {
    let mut at = buf.fold_span(line).0;
    for _ in 0..n {
        if at == 0 {
            break;
        }
        at = buf.fold_span(at - 1).0;
    }
    at
}

/// The first and last line of the `count` rows starting at the cursor's:
/// what `dd`, `yy`, `>>` and every other doubled operator act on. A
/// closed fold is one row, and goes whole.
pub fn line_span(buf: &dyn Buffer, count: usize) -> (usize, usize) {
    let (line, _) = buf.cursor();
    let last_line = buf.line_count().saturating_sub(1);
    let (first, mut last) = buf.fold_span(line);
    for _ in 1..count.max(1) {
        if last >= last_line {
            break;
        }
        last = buf.fold_span(last + 1).1;
    }
    (first, last)
}

/// Whether landing in a closed fold opens it: vim's default `foldopen`.
/// Moving by rows (`j`, `G`, a page, `zj`) stops on a closed fold; a
/// search, a mark, `%`, `l` and the rest were looking for something inside
/// it.
fn opens_folds(m: &Motion) -> bool {
    !matches!(
        m,
        Motion::Down
            | Motion::Up
            | Motion::GotoFirstLine
            | Motion::GotoLastLine
            | Motion::GotoPercent
            | Motion::ScreenTop
            | Motion::ScreenMiddle
            | Motion::ScreenBottom
            | Motion::HalfPageDown
            | Motion::HalfPageUp
            | Motion::PageDown
            | Motion::PageUp
            | Motion::ScrollLineDown
            | Motion::ScrollLineUp
            | Motion::ScrollCenter
            | Motion::ScrollTop
            | Motion::ScrollBottom
            | Motion::NextLineNonBlank
            | Motion::PrevLineNonBlank
            | Motion::SetMark(_)
            | Motion::FoldNextStart
            | Motion::FoldPreviousEnd
            | Motion::FoldStart
            | Motion::FoldEnd
    )
}

/// Applies one motion to the cursor. `count` is as typed (`None` without
/// digits): most motions repeat, defaulting to 1, but `GotoFirstLine`,
/// `GotoLastLine` and `GotoColumn` take it as a target, so `None` and
/// `Some(1)` differ there.
///
/// Ending inside a closed fold either opens it or moves to its row (see
/// `opens_folds`). An operator's motion does neither, since `motion_range`
/// takes a closed fold whole, so it uses `move_cursor`.
pub fn apply_motion(buf: &mut dyn Buffer, motion: Motion, count: Option<usize>) {
    let opens = opens_folds(&motion);
    move_cursor(buf, motion, count);
    let (line, col) = buf.cursor();
    // `closed_at` rather than `fold_span`: a one-line closed fold spans only
    // itself, and `l` still opens it.
    let Some((start, _)) = buf.folds().and_then(|folds| folds.closed_at(line)) else {
        return;
    };
    if opens {
        if let Some(folds) = buf.folds_mut() {
            folds.open_all(line);
        }
    } else {
        let col = col.min(last_col(buf, start));
        buf.set_cursor(start, col);
    }
}

fn move_cursor(buf: &mut dyn Buffer, motion: Motion, count: Option<usize>) {
    let n = count.unwrap_or(1).max(1);
    match motion {
        // By whole grapheme cluster, `n` times: `h` and `l` cross a ZWJ
        // sequence or a char with combining marks in one press. The
        // boundary functions clamp at the line's ends.
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
            let new_line = rows_down(buf, line, n);
            let new_col = col.min(last_col(buf, new_line));
            buf.set_cursor(new_line, new_col);
        }
        Motion::Up => {
            let (line, col) = buf.cursor();
            let new_line = rows_up(buf, line, n);
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
            let target = count.map(|c| c.saturating_sub(1)).unwrap_or(0).min(buf.line_count().saturating_sub(1));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::GotoLastLine => {
            let target =
                count.map(|c| c.saturating_sub(1)).unwrap_or_else(|| buf.line_count().saturating_sub(1)).min(buf.line_count().saturating_sub(1));
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        // `{count}%`, only emitted with a count (see vimkeys.rs); the
        // `unwrap_or(0)` just keeps this total. vim's formula:
        // `([count] * lines + 99) / 100`, 1-indexed.
        Motion::GotoPercent => {
            let total = buf.line_count();
            let target = count.unwrap_or(0).saturating_mul(total).saturating_add(99) / 100;
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
            if let Some(target) = find_char_landing(buf, line, col, ch, till, forward, n) {
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
            let target = rows_down(buf, line, n);
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::PrevLineNonBlank => {
            let (line, _) = buf.cursor();
            let target = rows_up(buf, line, n);
            let col = first_non_blank(buf, target);
            buf.set_cursor(target, col);
        }
        Motion::FoldNextStart | Motion::FoldPreviousEnd | Motion::FoldStart | Motion::FoldEnd => {
            let (line, col) = buf.cursor();
            let mut at = line;
            for _ in 0..n {
                let next = buf.folds().and_then(|folds| match motion {
                    Motion::FoldNextStart => folds.next_start(at),
                    Motion::FoldPreviousEnd => folds.previous_end(at),
                    Motion::FoldStart => folds.edge(at, false),
                    _ => folds.edge(at, true),
                });
                match next {
                    Some(next) => at = next,
                    None => break,
                }
            }
            if at != line {
                buf.set_cursor(at, col.min(last_col(buf, at)));
            }
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
        // `g*`/`g#`: like `SearchWordForward`/`SearchWordBackward` without
        // the word-boundary check.
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
        // Unreachable: vimkeys produces this only with an operator armed, and
        // `motion_range` intercepts it. Moves to the object's start, as
        // `motion_range` does, in case a caller ever uses it.
        Motion::TextObject(kind, around) => {
            if let Some(range) = text_object_range(buf, kind, around, count) {
                buf.set_cursor(range.from.0, range.from.1);
            }
        }
    }
}

/// How an operator treats a motion's range, per vim's `:help exclusive`.
/// `Inclusive` and `Exclusive` differ in whether the character the cursor
/// lands on is included; `Linewise` takes whole lines.
///
/// Not implemented: vim's `exclusive-linewise` adjustment, where an
/// exclusive motion ending in column 1 after starting at or before the
/// first non-blank becomes linewise. Without it, that one case yanks
/// slightly too much.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionShape {
    Exclusive,
    Inclusive,
    Linewise,
    /// A rectangle: rows `from.0..=to.0`, and on each the columns between
    /// `from.1` and `to.1` inclusive. `from` and `to` are corners, not start
    /// and end, so either may hold the smaller column; `block_columns` sorts
    /// that out.
    Blockwise,
}

/// The inclusive columns a blockwise range covers, lowest first; `None` for
/// other shapes.
pub fn block_columns(range: &MotionRange) -> Option<(usize, usize)> {
    (range.shape == MotionShape::Blockwise).then(|| (range.from.1.min(range.to.1), range.from.1.max(range.to.1)))
}

/// Whether `m` is one of vim's `:help jump-motions`, which record the
/// position before they run so `Ctrl-O`/`Ctrl-I` can step back (see
/// `vimkeys::VimKeys::push_jump`). Membership is vim's list, not distance:
/// `f` can move far and still isn't one.
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

/// `None` when `m` isn't an operator target: `SetMark` doesn't move, and
/// vim doesn't accept the scroll commands (Ctrl-D/U/F/B, Ctrl-E/Y,
/// zz/zt/zb) either.
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
        Motion::UnmatchedOpenParen | Motion::UnmatchedCloseParen | Motion::UnmatchedOpenBrace | Motion::UnmatchedCloseBrace => Exclusive,
        // Like the paragraph motions: to a boundary line's start, columns not
        // tracked.
        Motion::SectionForward | Motion::SectionForwardEnd | Motion::SectionBackward | Motion::SectionBackwardEnd => Exclusive,
        // Not consulted (`motion_range` handles text objects first), but kept
        // consistent with `text_object_range`.
        Motion::TextObject(TextObjectKind::Paragraph, _) => Linewise,
        Motion::TextObject(..) => Inclusive,
        // vim's classification: `dzj` takes whole lines, `d]z` doesn't.
        Motion::FoldNextStart | Motion::FoldPreviousEnd => Linewise,
        Motion::FoldStart | Motion::FoldEnd => Exclusive,
    })
}

/// A resolved operator target, ordered `from <= to` whichever way the
/// motion moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionRange {
    pub shape: MotionShape,
    pub from: (usize, usize),
    pub to: (usize, usize),
}

/// Applies `motion` with `apply_motion` and reports the range it covered.
/// The cursor is left at `from`, as vim does. `yy`/`Y` isn't a `Motion`;
/// see `whole_lines`.
///
/// `None` if `m` isn't an operator target (see `motion_shape`) or didn't
/// move the cursor (a failed find, or a motion already at its limit); the
/// cursor is then restored.
pub fn motion_range(buf: &mut dyn Buffer, motion: Motion, count: Option<usize>) -> Option<MotionRange> {
    // The cursor usually sits inside a text object's range, so diffing the
    // cursor doesn't work; resolve it directly and park the cursor at
    // `range.from`.
    if let Motion::TextObject(kind, around) = motion {
        let range = text_object_range(buf, kind, around, count)?;
        buf.set_cursor(range.from.0, range.from.1);
        return Some(range);
    }
    let shape = motion_shape(&motion)?;
    // Checked before the move, since `apply_motion` consumes it.
    let cannot_fail = match motion {
        Motion::LineEnd | Motion::LineLastNonBlank => true,
        // `dt,` with the comma right ahead: `t` lands before its target,
        // which is where the cursor already is, so nothing moves. But the
        // find succeeded, and being inclusive it covers that character,
        // which vim deletes. So this asks where the motion would land
        // instead of reading the cursor.
        Motion::FindChar { ch, till, forward } => {
            let (line, col) = buf.cursor();
            find_char_landing(buf, line, col, ch, till, forward, count.unwrap_or(1).max(1)) == Some(col)
        }
        _ => false,
    };
    let start = buf.cursor();
    move_cursor(buf, motion, count);
    let end = buf.cursor();
    if start == end {
        // A motion that didn't move usually covers nothing: `dfx` with no
        // `x` ahead is a failed find, not one deleted character. The
        // line-end motions can't fail: `$` already on the last character
        // succeeded and, being inclusive, covers it, so `D`, `d$` and `y$`
        // work there. Not on an empty line, which has no character to
        // cover.
        if cannot_fail && shape == MotionShape::Inclusive && buf.line_len(start.0) > 0 {
            return Some(MotionRange { shape, from: start, to: start });
        }
        return None;
    }
    let (mut from, mut to) = if pos_lt(start, end) { (start, end) } else { (end, start) };
    // A closed fold at either end is taken whole, as in vim: `dj` on a fold
    // deletes it and the line after, and `dl` on one empties every line in
    // it. The far end counts only if the motion reaches into that line; an
    // exclusive motion stopping at its column 0 doesn't.
    if let Some(folds) = buf.folds() {
        let head = folds.closed_at(from.0);
        let tail = folds.closed_at(to.0).filter(|_| shape != MotionShape::Exclusive || to.1 > 0);
        if let Some((first, _)) = head {
            from = (first, 0);
        }
        // The end has to be a position `extract_text` can step onto. An
        // exclusive range ends where the next line starts, taking the line
        // break and so every line of the fold (vim's `dl` on one); an
        // inclusive one ends on the fold's last character.
        if let Some((_, last)) = tail {
            to = match shape {
                MotionShape::Exclusive if last + 1 < buf.line_count() => (last + 1, 0),
                MotionShape::Exclusive => (last, buf.line_len(last)),
                _ => (last, buf.line_len(last).saturating_sub(1)),
            };
        }
    }
    buf.set_cursor(from.0, from.1);
    Some(MotionRange { shape, from, to })
}

/// The text a range covers. `Linewise` joins whole lines with `\n`,
/// including a trailing one, ready for a linewise put. The others walk with
/// `step_forward`, adding `\n` at each real line break; `Exclusive` stops
/// before `to`, `Inclusive` includes it. No `\n` is added across a
/// `line_wraps` boundary, where no line really ended.
pub fn extract_text(buf: &dyn Buffer, range: &MotionRange) -> String {
    // A block yanks one line per row, each cut to the block's columns; a
    // short line gives what falls inside, possibly nothing.
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

/// `yy`/`Y`: this line plus `count - 1` more, linewise. Not a motion, and
/// the cursor doesn't move. A whole line even on a `line_wraps`
/// continuation row: the range is widened back to the run's first row.
pub fn whole_lines(buf: &dyn Buffer, count: usize) -> String {
    let (cursor_line, _) = buf.cursor();
    let mut line = buf.fold_span(cursor_line).0;
    while line > 0 && buf.line_wraps(line - 1) {
        line -= 1;
    }
    // `count` logical lines, not rows. A closed fold counts as one, so `yy`
    // on it yanks it whole, as `dd` deletes it (see `line_span`).
    let count = count.max(1);
    let mut last = line;
    for i in 0..count {
        last = buf.fold_span(last).1;
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

/// A motion's canonical name, as `::bish map` prints it: the variant in
/// kebab-case, so it's predictable and can't drift from the variant.
/// Parameterized motions append what they carry (`find-char` needs the
/// character).
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
        FoldNextStart => "fold-next-start",
        FoldPreviousEnd => "fold-previous-end",
        FoldStart => "fold-start",
        FoldEnd => "fold-end",
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

/// The object half of a text-object motion's name; see `describe_motion`.
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
        // Rows cut by autowrap rather than a real line break, as in a
        // ScreenBuffer. Empty for most tests.
        wraps: std::collections::HashSet<usize>,
        // What `Buffer::search_ignore_case` answers.
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

        // Each `&str` is one logical line, split into `width`-wide rows
        // like a terminal grid, every row but the last marked as wrapped.
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
            TestBuffer { lines, cursor: (0, 0), vtop: 0, vheight: 24, marks: std::collections::HashMap::new(), wraps, icase: false }
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
        // `a`, MAN+ZWJ+WOMAN (a 3-char cluster), `b`.
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
        // A count of 2 lands on the cluster's start, then `b`: never 2 or 3,
        // inside the cluster.
        assert_eq!(go(&mut buf, Motion::Right, Some(2)), (0, 4));
    }

    #[test]
    fn last_col_clamps_to_a_trailing_clusters_own_start() {
        // A line ending in a 3-char cluster: the last valid column is the
        // cluster's start (1), not its last char (3).
        let mut buf = TestBuffer::new("a\u{1F468}\u{200D}\u{1F469}");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::Right, Some(99)), (0, 1));
    }

    #[test]
    fn ordinary_ascii_left_right_is_unaffected() {
        // Single-char clusters only: plain char arithmetic.
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
        // No third X: the motion fails and the cursor stays.
        assert_eq!(go(&mut buf, f, None), (0, 7));
    }

    // `dt,` with the comma right ahead: the find succeeds and lands where the
    // cursor is, which differs from a find with nowhere to go.
    #[test]
    fn a_till_motion_onto_the_cursors_own_column_still_covers_that_character() {
        let mut buf = TestBuffer::new("a,b,c");
        let t = Motion::FindChar { ch: ',', till: true, forward: true };
        let range = motion_range(&mut buf, t, None).expect("the find succeeded, so there is a range");
        assert_eq!((range.shape, range.from, range.to), (MotionShape::Inclusive, (0, 0), (0, 0)));

        // ...and a find with no target covers nothing.
        let mut buf = TestBuffer::new("abc");
        assert_eq!(motion_range(&mut buf, Motion::FindChar { ch: ',', till: true, forward: true }, None), None);
        assert_eq!(motion_range(&mut buf, Motion::FindChar { ch: ',', till: false, forward: true }, None), None);
        // Backward `T` is exclusive, so landing on the cursor's column covers
        // nothing, and vim deletes nothing.
        let mut buf = TestBuffer::new("a,b");
        buf.set_cursor(0, 2);
        assert_eq!(motion_range(&mut buf, Motion::FindChar { ch: ',', till: true, forward: false }, None), None);
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
        (0..n).map(|i| format!("l{}", i)).collect::<Vec<_>>().join("\n")
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

    // A block's corners aren't a start and an end: dragging up and right is
    // ordinary.
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

        // A row shorter than the block gives what falls inside, possibly
        // nothing.
        let buf = TestBuffer::new("aaa111\nb\nccc333");
        let range = MotionRange { shape: MotionShape::Blockwise, from: (0, 3), to: (2, 5) };
        assert_eq!(extract_text(&buf, &range), "111\n\n333");
    }

    // This checks the wiring that decides whether the engine folds case.
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

        // Backward search and the highlight pass use the same answer.
        let mut buf = TestBuffer::new(lines);
        buf.icase = true;
        buf.set_cursor(3, 0);
        assert_eq!(go(&mut buf, Motion::SearchBackward("BETA".to_string()), None), (2, 0));
        assert_eq!(find_matches_in_line(&buf, 1, "BETA"), vec![(0, 4)]);

        let buf = TestBuffer::new(lines);
        assert!(find_matches_in_line(&buf, 1, "BETA").is_empty(), "and case-sensitively it does not match at all");
    }

    // `*` folds case as vim does for a typed pattern.
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

    // A wheel event moves the cursor one line, and the viewport follows only
    // when it must, like the arrow key per cell the terminal itself sends for
    // a wheel. See `fileeditor::MOUSE_WHEEL_LINES`.
    #[test]
    fn a_wheel_event_moves_the_cursor_and_the_view_only_follows() {
        let notch = Some(crate::fileeditor::MOUSE_WHEEL_LINES);
        let mut buf = TestBuffer::new(&numbered_lines(40));
        buf.vheight = 10;
        buf.set_cursor(0, 0);

        // Nine events with a ten-row view: the cursor walks down and the view
        // stays.
        for _ in 0..9 {
            go(&mut buf, Motion::Down, notch);
        }
        assert_eq!(buf.cursor().0, 9, "the cursor moved one line per event");
        assert_eq!(buf.viewport_top(), 0, "and the view did not move at all");

        // Dragging the view along is `scroll_to_show_cursor`'s job, run after
        // every motion. What matters here is that the wheel doesn't move the
        // viewport itself.
        for _ in 0..9 {
            go(&mut buf, Motion::Up, notch);
        }
        assert_eq!(buf.cursor().0, 0, "and back, one line per event");
        assert_eq!(buf.viewport_top(), 0);
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
    fn a_bracket_pair_is_found_from_a_bracket_the_cursor_is_on_and_from_nothing_else() {
        let mut buf = TestBuffer::new("foo(bar[baz]qux)end");
        // `%` from here would scan forward to the `(`, but nothing is
        // marked until the cursor is on a bracket, as in vim.
        buf.set_cursor(0, 0);
        assert_eq!(bracket_pair_at(&buf, buf.cursor()), None);
        buf.set_cursor(0, 3);
        assert_eq!(bracket_pair_at(&buf, buf.cursor()), Some(((0, 3), (0, 15))), "past the nested pair");
        buf.set_cursor(0, 15);
        assert_eq!(bracket_pair_at(&buf, buf.cursor()), Some(((0, 15), (0, 3))), "and back from the closing one");
        buf.set_cursor(0, 7);
        assert_eq!(bracket_pair_at(&buf, buf.cursor()), Some(((0, 7), (0, 11))));

        // Nothing closes it, so nothing is marked.
        let mut open = TestBuffer::new("foo(bar");
        open.set_cursor(0, 3);
        assert_eq!(bracket_pair_at(&open, open.cursor()), None);
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
        // Searches strictly before the current line, as vim does.
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
        // An unset mark is a no-op.
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
        assert_eq!(go(&mut buf, Motion::SearchForward("foo".to_string()), Some(2)), (0, 16));
    }

    #[test]
    fn search_not_found_is_a_no_op() {
        let mut buf = TestBuffer::new("foo bar");
        buf.set_cursor(0, 2);
        assert_eq!(go(&mut buf, Motion::SearchForward("xyz".to_string()), None), (0, 2));
    }

    #[test]
    fn search_forward_and_backward_are_full_ere_regex() {
        // `/` takes a regex: `foo[0-9]+` matches `foo1`, `foo22` and `foo333`
        // whole.
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
        // `^foo` skips the first line's `foo` at column 1 for the second's.
        assert_eq!(go(&mut buf, Motion::SearchForward("^foo".to_string()), None), (1, 0));
    }

    #[test]
    fn find_matches_in_line_reports_every_regex_match() {
        let buf = TestBuffer::new("a1 a22 a333");
        assert_eq!(find_matches_in_line(&buf, 0, "a[0-9]+"), vec![(0, 2), (3, 6), (7, 11)]);
    }

    #[test]
    fn search_word_forward_and_backward_respect_word_boundaries() {
        // `category` contains `cat` but not as a word, so `*` skips it.
        let mut buf = TestBuffer::new("cat dog category cat");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForward, None), (0, 17));
        assert_eq!(go(&mut buf, Motion::SearchWordBackward, None), (0, 0));
    }

    #[test]
    fn g_star_and_g_hash_ignore_word_boundaries() {
        // `g*` lands on the `cat` inside `category`, at column 8.
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
        struct Kw(TestBuffer, &'static str);
        impl Buffer for Kw {
            fn line_count(&self) -> usize {
                self.0.line_count()
            }
            fn line_len(&self, l: usize) -> usize {
                self.0.line_len(l)
            }
            fn char_at(&self, l: usize, c: usize) -> Option<char> {
                self.0.char_at(l, c)
            }
            fn cursor(&self) -> (usize, usize) {
                self.0.cursor()
            }
            fn set_cursor(&mut self, l: usize, c: usize) {
                self.0.set_cursor(l, c)
            }
            fn viewport_top(&self) -> usize {
                self.0.viewport_top()
            }
            fn set_viewport_top(&mut self, l: usize) {
                self.0.set_viewport_top(l)
            }
            fn viewport_height(&self) -> usize {
                self.0.viewport_height()
            }
            fn set_mark(&mut self, name: char, pos: (usize, usize)) {
                self.0.set_mark(name, pos)
            }
            fn get_mark(&self, name: char) -> Option<(usize, usize)> {
                self.0.get_mark(name)
            }
            fn word_chars(&self) -> &str {
                self.1
            }
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
        // `D`, `d$` and `y$` from where `$` leaves the cursor: it hasn't
        // moved, but the range isn't empty.
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
        // `$` not moving means it's already there; `f` not moving means no
        // match, and an operator must do nothing.
        let mut buf = TestBuffer::new("abxcd");
        buf.set_cursor(0, 4);
        assert!(motion_range(&mut buf, Motion::FindChar { ch: 'z', till: false, forward: true }, None).is_none());
        assert!(motion_range(&mut buf, Motion::MatchPair, None).is_none());
        // ...and likewise an exclusive motion with nowhere to go.
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
        // One logical line, `helloworldXYZ`, wrapped into three 5-wide rows.
        let mut buf = TestBuffer::new_wrapped(&["helloworldXYZ", "next"], 5);
        buf.set_cursor(0, 0);
        assert_eq!(whole_lines(&buf, 1), "helloworldXYZ\n");

        // From the middle row, `yy` still yanks the whole logical line.
        buf.set_cursor(1, 0);
        assert_eq!(whole_lines(&buf, 1), "helloworldXYZ\n");

        // A count past the run picks up the next real line, after a real
        // newline.
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
        // `$` from the first row lands on the run's last row at its last real
        // character, not at column 4 of row 0.
        assert_eq!(go(&mut buf, Motion::LineEnd, None), (2, 2));
    }

    #[test]
    fn extract_text_charwise_crosses_an_empty_line() {
        let mut buf = TestBuffer::new("a\n\nb");
        buf.set_cursor(0, 0);
        // `a`, then the empty line (a word of its own), then `b`:
        // exclusive, so `b` isn't included.
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
        // No trailing whitespace at the end, so `aw` takes the leading.
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
        // A cursor before the quotes still finds them.
        assert_eq!(object_text(&mut buf, (0, 0), TextObjectKind::DoubleQuote, false, None), Some("hello world".to_string()));
    }

    #[test]
    fn quote_object_empty_pair_has_no_inner_but_has_around() {
        let mut buf = TestBuffer::new(r#"x = "";"#);
        assert_eq!(object_text(&mut buf, (0, 5), TextObjectKind::DoubleQuote, false, None), None);
        // No whitespace after the closing quote (`;` isn't blank), so `a"`
        // takes the leading whitespace, as `aw` does.
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
        // The brackets end line 0 and start line 2. The steps never land on
        // the newline between, so the inner range is exactly line 1.
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
        // The last paragraph has no trailing blank run, so `ap` takes the
        // leading one.
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
        // `TextObject` never reaches `apply_motion` from vimkeys, but is
        // handled there; tested directly.
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
        // A number that wasn't zero-padded is never padded, even when it
        // shrinks.
        assert_eq!(apply_number_delta(&m, -42), "0");
        assert_eq!(apply_number_delta(&m, -50), "-8");
    }

    #[test]
    fn apply_number_delta_preserves_leading_zero_width() {
        let m = NumberMatch { from: (0, 0), to: (0, 2), value: 7, width: 3, leading_zero: true };
        assert_eq!(apply_number_delta(&m, 1), "008");
        // Growing past the original width drops the padding, as in vim.
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
        // `w`, `s` and `p` are text objects but never surround targets.
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
        // Only `b`, `B` and `r` are letter aliases; other letters are
        // rejected rather than wrapping in a literal letter.
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
        // `( )`: the lone space would otherwise be claimed as padding by both
        // sides.
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
