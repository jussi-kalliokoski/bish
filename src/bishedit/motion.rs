use super::Buffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    }

    impl TestBuffer {
        fn new(text: &str) -> Self {
            let lines = text.split('\n').map(|l| l.chars().collect()).collect();
            TestBuffer {
                lines,
                cursor: (0, 0),
                vtop: 0,
                vheight: 24,
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
        assert_eq!(go(&mut buf, f, None), (0, 3));
        assert_eq!(go(&mut buf, f, None), (0, 7));
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
        assert_eq!(go(&mut buf, big_f, None), (0, 7));
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
}
