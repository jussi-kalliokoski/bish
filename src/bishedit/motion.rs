use super::Buffer;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn classify(buf: &impl Buffer, line: usize, col: usize, big: bool) -> Class {
    let ch = match buf.char_at(line, col) {
        Some(c) => c,
        None => return Class::Blank,
    };
    if ch.is_whitespace() {
        Class::Blank
    } else if big || is_word_char(ch) {
        Class::Word
    } else {
        Class::Punct
    }
}

fn last_col(buf: &impl Buffer, line: usize) -> usize {
    buf.line_len(line).saturating_sub(1)
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
    if buf.line_len(p.0) == 0 || !matches!(buf.char_at(p.0, p.1), Some(c) if is_word_char(c)) {
        let next = word_forward_once(buf, p, false);
        if next == p {
            return None;
        }
        p = next;
    }
    if buf.line_len(p.0) == 0 {
        return None;
    }
    if !matches!(buf.char_at(p.0, p.1), Some(c) if is_word_char(c)) {
        return None;
    }
    let line = p.0;
    let mut start_col = p.1;
    while start_col > 0 && matches!(buf.char_at(line, start_col - 1), Some(c) if is_word_char(c)) {
        start_col -= 1;
    }
    let mut end_col = p.1;
    while end_col + 1 < buf.line_len(line)
        && matches!(buf.char_at(line, end_col + 1), Some(c) if is_word_char(c))
    {
        end_col += 1;
    }
    Some((start_col..=end_col).filter_map(|c| buf.char_at(line, c)).collect())
}

/// Literal (non-regex) substring search -- deliberately simple, matching
/// this milestone's "nothing fancy" scope. Matches never span line breaks.
fn line_find(buf: &impl Buffer, line: usize, lower_bound: usize, chars: &[char]) -> Option<usize> {
    let len = buf.line_len(line);
    let plen = chars.len();
    if plen == 0 || plen > len {
        return None;
    }
    let max_start = len - plen;
    if lower_bound > max_start {
        return None;
    }
    (lower_bound..=max_start).find(|&start| {
        (0..plen).all(|i| buf.char_at(line, start + i) == Some(chars[i]))
    })
}

/// Every non-overlapping occurrence of `pattern` on `line`, left to right --
/// the same convention vim's own `hlsearch` uses (a match's own end is
/// where the search for the next one starts, so "aa" against "aaaa" finds
/// cols 0 and 2, not 0/1/2). For search-match *highlighting* -- unrelated
/// to, and doesn't share any state with, a live search's own cursor
/// position via search_forward_once/search_backward_once above.
pub fn find_matches_in_line(buf: &impl Buffer, line: usize, pattern: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = pattern.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut matches = Vec::new();
    let mut from = 0;
    while let Some(start) = line_find(buf, line, from, &chars) {
        let end = start + chars.len();
        matches.push((start, end));
        from = end;
    }
    matches
}

fn line_rfind(buf: &impl Buffer, line: usize, upper_bound: usize, chars: &[char]) -> Option<usize> {
    let len = buf.line_len(line);
    let plen = chars.len();
    if plen == 0 || plen > len {
        return None;
    }
    let max_start = len - plen;
    (0..=max_start).rev().filter(|&start| start < upper_bound).find(|&start| {
        (0..plen).all(|i| buf.char_at(line, start + i) == Some(chars[i]))
    })
}

/// Wrapping forward search (matches vim's default 'wrapscan'): tries the
/// rest of the current line, then every subsequent line, then wraps back
/// around to the start of the original line.
fn search_forward_once(buf: &impl Buffer, pos: (usize, usize), chars: &[char]) -> Option<(usize, usize)> {
    if chars.is_empty() {
        return None;
    }
    let total = buf.line_count();
    if let Some(c) = line_find(buf, pos.0, pos.1 + 1, chars) {
        return Some((pos.0, c));
    }
    for offset in 1..total {
        let line = (pos.0 + offset) % total;
        if let Some(c) = line_find(buf, line, 0, chars) {
            return Some((line, c));
        }
    }
    line_find(buf, pos.0, 0, chars).map(|c| (pos.0, c))
}

fn search_backward_once(buf: &impl Buffer, pos: (usize, usize), chars: &[char]) -> Option<(usize, usize)> {
    if chars.is_empty() {
        return None;
    }
    let total = buf.line_count();
    if let Some(c) = line_rfind(buf, pos.0, pos.1, chars) {
        return Some((pos.0, c));
    }
    for offset in 1..total {
        let line = (pos.0 + total - offset) % total;
        if let Some(c) = line_rfind(buf, line, usize::MAX, chars) {
            return Some((line, c));
        }
    }
    line_rfind(buf, pos.0, usize::MAX, chars).map(|c| (pos.0, c))
}

fn is_word_boundary_at(buf: &impl Buffer, line: usize, col: usize) -> bool {
    !matches!(buf.char_at(line, col), Some(c) if is_word_char(c))
}

fn search_word_forward_once(buf: &impl Buffer, pos: (usize, usize), chars: &[char]) -> Option<(usize, usize)> {
    let first = search_forward_once(buf, pos, chars)?;
    let mut candidate = first;
    loop {
        let (l, c) = candidate;
        let before_ok = c == 0 || is_word_boundary_at(buf, l, c - 1);
        let after_ok = is_word_boundary_at(buf, l, c + chars.len());
        if before_ok && after_ok {
            return Some(candidate);
        }
        let next = search_forward_once(buf, candidate, chars)?;
        if next == first {
            return None;
        }
        candidate = next;
    }
}

fn search_word_backward_once(buf: &impl Buffer, pos: (usize, usize), chars: &[char]) -> Option<(usize, usize)> {
    let first = search_backward_once(buf, pos, chars)?;
    let mut candidate = first;
    loop {
        let (l, c) = candidate;
        let before_ok = c == 0 || is_word_boundary_at(buf, l, c - 1);
        let after_ok = is_word_boundary_at(buf, l, c + chars.len());
        if before_ok && after_ok {
            return Some(candidate);
        }
        let next = search_backward_once(buf, candidate, chars)?;
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
        Motion::Left => {
            let (line, col) = buf.cursor();
            buf.set_cursor(line, col.saturating_sub(n));
        }
        Motion::Right => {
            let (line, col) = buf.cursor();
            let max_col = last_col(buf, line);
            buf.set_cursor(line, (col + n).min(max_col));
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
            let target = (line + n - 1).min(buf.line_count().saturating_sub(1));
            buf.set_cursor(target, last_col(buf, target));
        }
        Motion::LineLastNonBlank => {
            let (line, _) = buf.cursor();
            let target = (line + n - 1).min(buf.line_count().saturating_sub(1));
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
            let chars: Vec<char> = pattern.chars().collect();
            let mut pos = buf.cursor();
            let mut found = None;
            for _ in 0..n {
                match search_forward_once(buf, pos, &chars) {
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
        Motion::SearchBackward(pattern) => {
            let chars: Vec<char> = pattern.chars().collect();
            let mut pos = buf.cursor();
            let mut found = None;
            for _ in 0..n {
                match search_backward_once(buf, pos, &chars) {
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
        Motion::SearchWordForward => {
            let pos = buf.cursor();
            if let Some(word) = word_under_cursor(buf, pos) {
                let chars: Vec<char> = word.chars().collect();
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_word_forward_once(buf, cur, &chars) {
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
                let chars: Vec<char> = word.chars().collect();
                let mut cur = pos;
                let mut found = None;
                for _ in 0..n {
                    match search_word_backward_once(buf, cur, &chars) {
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
        Motion::GotoFirstLine | Motion::GotoLastLine => Linewise,
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
    let shape = motion_shape(&motion)?;
    let start = buf.cursor();
    apply_motion(buf, motion, count);
    let end = buf.cursor();
    if start == end {
        return None;
    }
    let (from, to) = if pos_lt(start, end) { (start, end) } else { (end, start) };
    buf.set_cursor(from.0, from.1);
    Some(MotionRange { shape, from, to })
}

/// The literal text a `MotionRange` covers. `Linewise` joins whole lines
/// with `\n`, including a trailing one (so the result is always a sequence
/// of complete lines, ready to be spliced back in as-is by a linewise
/// put). `Inclusive`/`Exclusive` walk character-by-character via the same
/// `step_forward` plain motions use, inserting `\n` exactly when a step
/// crosses a line boundary -- `Exclusive` stops one character short of
/// `to`, `Inclusive` includes it.
pub fn extract_text(buf: &impl Buffer, range: &MotionRange) -> String {
    if range.shape == MotionShape::Linewise {
        let mut s = String::new();
        for l in range.from.0..=range.to.0 {
            s.push_str(&buf.line_chars(l).into_iter().collect::<String>());
            s.push('\n');
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
        if next.0 != cur.0 {
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
/// Buffer`).
pub fn whole_lines(buf: &impl Buffer, count: usize) -> String {
    let (line, _) = buf.cursor();
    let last = (line + count.max(1) - 1).min(buf.line_count().saturating_sub(1));
    let mut s = String::new();
    for l in line..=last {
        s.push_str(&buf.line_chars(l).into_iter().collect::<String>());
        s.push('\n');
    }
    s
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
            }
        }
    }

    impl Buffer for TestBuffer {
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
    fn search_word_forward_and_backward_respect_word_boundaries() {
        // "category" contains "cat" as a substring but not as a whole word --
        // * must skip it and land on the next real "cat".
        let mut buf = TestBuffer::new("cat dog category cat");
        buf.set_cursor(0, 0);
        assert_eq!(go(&mut buf, Motion::SearchWordForward, None), (0, 17));
        assert_eq!(go(&mut buf, Motion::SearchWordBackward, None), (0, 0));
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
}
