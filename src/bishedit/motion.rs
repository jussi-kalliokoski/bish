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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestBuffer {
        lines: Vec<Vec<char>>,
        cursor: (usize, usize),
        vtop: usize,
    }

    impl TestBuffer {
        fn new(text: &str) -> Self {
            let lines = text.split('\n').map(|l| l.chars().collect()).collect();
            TestBuffer {
                lines,
                cursor: (0, 0),
                vtop: 0,
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
            24
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
}
