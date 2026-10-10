// The lexical layer: where roff's control lines, requests, escapes and
// comments are in the source. Interprets nothing (no macros, no
// conditionals, no registers): its consumer is syntax highlighting,
// which colours the source as written.
//
// The escape scanner is shared with the interpreter, so the highlighter
// and the parser agree on how long `\f(CW`, `\s[+2]` or `\h'|4n'` is.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    // `.\"` and `'\"` whole-line comments, and a `\"` that starts one
    // mid-line. Includes the escape itself.
    Comment,
    // The leading `.` or `'` of a control line.
    Control,
    // The request or macro name right after it.
    Request,
    // One argument of a control line, quotes included when it had them.
    Argument,
    // Any escape sequence: `\fB`, `\(bu`, `\*[foo]`, `\-`.
    Escape,
    // Ordinary text.
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
}

// Every token in `source`, in order, in char offsets, as the editor's
// highlighting indexes.
pub fn lex(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < chars.len() {
        if line_start {
            let mut j = i;
            // A control line may be indented by spaces or tabs.
            while matches!(chars.get(j), Some(' ') | Some('\t')) {
                j += 1;
            }
            if matches!(chars.get(j), Some('.') | Some('\'')) {
                i = lex_control_line(&chars, j, &mut out);
                line_start = true;
                continue;
            }
            line_start = false;
        }
        match chars[i] {
            '\n' => {
                i += 1;
                line_start = true;
            }
            '\\' if is_comment_escape(&chars, i) => {
                let end = end_of_line(&chars, i);
                out.push(Token { kind: TokenKind::Comment, span: i..end });
                i = end;
            }
            '\\' => {
                let len = escape_len(&chars, i);
                out.push(Token { kind: TokenKind::Escape, span: i..i + len });
                i += len;
            }
            _ => {
                let start = i;
                while i < chars.len() && chars[i] != '\n' && chars[i] != '\\' {
                    i += 1;
                }
                out.push(Token { kind: TokenKind::Text, span: start..i });
            }
        }
    }
    out
}

// A control line: the `.`, the request name, then arguments,
// whitespace-separated except that a `"` opens one running to the next
// `"`.
fn lex_control_line(chars: &[char], start: usize, out: &mut Vec<Token>) -> usize {
    out.push(Token { kind: TokenKind::Control, span: start..start + 1 });
    let mut i = start + 1;
    // `. SH` is legal: whitespace may follow the control character.
    while matches!(chars.get(i), Some(' ') | Some('\t')) {
        i += 1;
    }
    let name_start = i;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() || c == '\\' {
            break;
        }
        i += 1;
    }
    if i > name_start {
        out.push(Token { kind: TokenKind::Request, span: name_start..i });
    }
    // `.\"`: the comment request, whose "name" is the escape itself.
    if chars.get(i) == Some(&'\\') && is_comment_escape(chars, i) {
        let end = end_of_line(chars, i);
        out.push(Token { kind: TokenKind::Comment, span: i..end });
        return skip_newline(chars, end);
    }
    loop {
        while matches!(chars.get(i), Some(' ') | Some('\t')) {
            i += 1;
        }
        match chars.get(i) {
            None | Some('\n') => return skip_newline(chars, i),
            Some('\\') if is_comment_escape(chars, i) => {
                let end = end_of_line(chars, i);
                out.push(Token { kind: TokenKind::Comment, span: i..end });
                return skip_newline(chars, end);
            }
            Some('"') => {
                let arg_start = i;
                i += 1;
                while let Some(&c) = chars.get(i) {
                    if c == '\n' {
                        break;
                    }
                    if c == '"' {
                        i += 1;
                        break;
                    }
                    i += if c == '\\' { escape_len(chars, i) } else { 1 };
                }
                out.push(Token { kind: TokenKind::Argument, span: arg_start..i });
            }
            Some(_) => {
                let arg_start = i;
                while let Some(&c) = chars.get(i) {
                    if c.is_whitespace() {
                        break;
                    }
                    if c == '\\' && is_comment_escape(chars, i) {
                        break;
                    }
                    i += if c == '\\' { escape_len(chars, i) } else { 1 };
                }
                if i == arg_start {
                    return skip_newline(chars, end_of_line(chars, i));
                }
                out.push(Token { kind: TokenKind::Argument, span: arg_start..i });
            }
        }
    }
}

fn skip_newline(chars: &[char], i: usize) -> usize {
    if chars.get(i) == Some(&'\n') { i + 1 } else { i }
}

fn end_of_line(chars: &[char], from: usize) -> usize {
    let mut i = from;
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    i
}

// `\"` starts a comment; so does `\#`, which additionally swallows the
// newline. Both are escapes, so a preceding backslash matters.
pub fn is_comment_escape(chars: &[char], i: usize) -> bool {
    chars.get(i) == Some(&'\\') && matches!(chars.get(i + 1), Some('"') | Some('#'))
}

// How many characters the escape at `i` takes, backslash included.
// Never zero, so a caller always progresses.
//
// The shapes real pages use: a bare two-character escape (`\-`, `\&`,
// `\e`); a one-argument escape taking one character, `(` plus two, or
// `[` plus a name (`\fB`, `\f(CW`, `\f[bold]`, likewise `\*`, `\n`,
// `\g`, `\$`); a special (`\(bu`, `\[u2014]`); a size change with an
// optional sign and one or two digits (`\s+2`, `\s[-2]`); and the
// quote-delimited motion and drawing escapes (`\h'|4n'`, `\D'l 1n 0'`).
pub fn escape_len(chars: &[char], i: usize) -> usize {
    if chars.get(i) != Some(&'\\') {
        return 1;
    }
    let Some(&c) = chars.get(i + 1) else { return 1 };
    match c {
        // Escapes whose argument may be one char, (xx, or [name].
        'f' | '*' | 'n' | 'g' | '$' | 'V' | 'F' | 'm' | 'M' | 'Y' | 'k' => 2 + argument_len(chars, i + 2),
        '(' => 4.min(chars.len().saturating_sub(i)).max(2),
        '[' => match close_bracket(chars, i + 2, ']') {
            Some(end) => end + 1 - i,
            None => 2,
        },
        's' => 2 + size_argument_len(chars, i + 2),
        // Quote-delimited: the character after the escape is the delimiter.
        'h' | 'v' | 'l' | 'L' | 'o' | 'w' | 'A' | 'b' | 'C' | 'D' | 'H' | 'N' | 'R' | 'S' | 'x' | 'X' | 'Z' => match chars.get(i + 2) {
            Some(&delim) => match close_bracket(chars, i + 3, delim) {
                Some(end) => end + 1 - i,
                None => 3,
            },
            None => 2,
        },
        _ => 2,
    }
}

// The `X` / `(XX` / `[NAME]` argument of `\f`, `\*`, `\n` and the rest.
fn argument_len(chars: &[char], at: usize) -> usize {
    match chars.get(at) {
        Some('(') => 3.min(chars.len().saturating_sub(at)).max(1),
        Some('[') => match close_bracket(chars, at + 1, ']') {
            Some(end) => end + 1 - at,
            None => 1,
        },
        Some(_) => 1,
        None => 0,
    }
}

fn size_argument_len(chars: &[char], at: usize) -> usize {
    let mut i = at;
    if matches!(chars.get(i), Some('+') | Some('-')) {
        i += 1;
    }
    match chars.get(i) {
        Some('(') => return i + 3 - at,
        Some('[') => {
            return match close_bracket(chars, i + 1, ']') {
                Some(end) => end + 1 - at,
                None => i + 1 - at,
            };
        }
        _ => {}
    }
    while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
    }
    (i - at).max(1)
}

fn close_bracket(chars: &[char], from: usize, close: char) -> Option<usize> {
    (from..chars.len()).take_while(|&i| chars[i] != '\n').find(|&i| chars[i] == close)
}
