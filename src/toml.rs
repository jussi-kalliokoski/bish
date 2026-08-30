// TOML (v1.0.0), tokenized for highlighting.
//
// Where `ini.rs` could be line-oriented -- INI has no construct that
// spans a line, so every line is independently interpretable -- TOML
// has three that do: multi-line strings (`"""`/`'''`), multi-line
// arrays, and inline tables. So this is a real lexer over the whole
// input rather than a per-line pass.
//
// The one piece of *grammar* it carries is which position it is in,
// because TOML spells a key and a value with the same characters:
// `title` is a key, `inf` is a float, and `true` is a boolean only in
// value position -- `true = 1` is a perfectly good key/value pair. A
// small context stack (are we inside an array, an inline table, or at
// the top level; before or after this pair's `=`) is what tells them
// apart, and it is the thing a scan for `=` on each line would get
// wrong the moment an array spanned two lines.
//
// Tokenizing never fails: anything unreadable becomes `Invalid` and the
// scan continues, so a buffer mid-keystroke still produces something a
// highlighter can colour up to and past the mistake -- the same
// contract `json::tokens` documents, and for the same reason.
//
// Offsets are **char** offsets, matching `highlight::HighlightSpan`.
#![allow(dead_code)]

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Comment,
    /// A whole table header, brackets included: `[table]` or
    /// `[[array.of.tables]]`. Emitted *before* the tokens for anything
    /// quoted inside it, so a highlighter can paint the header as one
    /// thing and let those show through -- the same layering
    /// `IniHighlighter` uses for `[remote "origin"]`.
    TableHeader,
    /// One segment of a key: `a` and `b` in `a.b = 1`, bare or quoted.
    Key,
    /// A string of any of TOML's four kinds, quotes included. `escapes`
    /// are spans within it, and are always empty for a literal
    /// (`'`-quoted) string, where a backslash is a backslash.
    Str { escapes: Vec<Range<usize>> },
    /// An integer or float in any of TOML's spellings, `inf`/`nan`
    /// included.
    Number,
    Bool,
    /// An offset date-time, local date-time, local date or local time.
    /// Its own kind rather than a `Number` because it is not one, and
    /// its own rather than a `Str` because nobody quoted it.
    DateTime,
    /// `=` `.` `,` and the brackets and braces of arrays and inline
    /// tables. A table header's own brackets are part of `TableHeader`
    /// instead.
    Punctuation,
    /// Anything that isn't TOML. Carried rather than raised.
    Invalid,
}

pub fn tokens(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    Lexer { chars: &chars, pos: 0, out: Vec::new(), stack: Vec::new(), expect: Expect::Key, line_start: true }.run()
}

// Which of the two things the same characters would mean here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    Key,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Array,
    InlineTable,
}

struct Lexer<'a> {
    chars: &'a [char],
    pos: usize,
    out: Vec<Token>,
    stack: Vec<Ctx>,
    expect: Expect,
    // Whether nothing but whitespace has been seen on this line -- what
    // makes a `[` a table header rather than an array.
    line_start: bool,
}

impl Lexer<'_> {
    fn run(mut self) -> Vec<Token> {
        loop {
            while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
                self.pos += 1;
            }
            let Some(c) = self.peek() else { break };
            if c == '\n' {
                self.pos += 1;
                // A newline ends a key/value pair, but only at the top
                // level: inside an array or an inline table it is just
                // whitespace, which is exactly why this can't be done
                // one line at a time.
                if self.stack.is_empty() {
                    self.expect = Expect::Key;
                    self.line_start = true;
                }
                continue;
            }
            if c == '#' {
                self.comment();
                continue;
            }
            if c == '[' && self.line_start && self.stack.is_empty() && self.expect == Expect::Key {
                self.table_header();
                continue;
            }
            self.line_start = false;
            match self.expect {
                Expect::Key => self.key_token(),
                Expect::Value => self.value_token(),
            }
        }
        self.out
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.out.push(Token { kind, start, end: self.pos });
    }

    fn comment(&mut self) {
        let start = self.pos;
        while !matches!(self.peek(), None | Some('\n')) {
            self.pos += 1;
        }
        self.push(TokenKind::Comment, start);
    }

    // `[name]` or `[[name]]`, to the closing bracket or, failing that,
    // the end of the line -- a header being typed is still a header, and
    // must not swallow the rest of the file looking for its `]`.
    fn table_header(&mut self) {
        let start = self.pos;
        let mut quoted: Vec<(usize, usize, Vec<Range<usize>>)> = Vec::new();
        self.pos += 1;
        if self.peek() == Some('[') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            match c {
                '\n' => break,
                ']' => {
                    self.pos += 1;
                    if self.peek() == Some(']') {
                        self.pos += 1;
                    }
                    break;
                }
                // A quoted segment is a string sitting inside the
                // header, and has to be consumed as one or a `]` inside
                // it would end the header early.
                '"' | '\'' => {
                    let at = self.pos;
                    let escapes = self.string();
                    quoted.push((at, self.pos, escapes));
                }
                _ => self.pos += 1,
            }
        }
        self.push(TokenKind::TableHeader, start);
        for (start, end, escapes) in quoted {
            self.out.push(Token { kind: TokenKind::Str { escapes }, start, end });
        }
    }

    fn key_token(&mut self) {
        let start = self.pos;
        match self.peek() {
            Some('=') => {
                self.pos += 1;
                self.push(TokenKind::Punctuation, start);
                self.expect = Expect::Value;
            }
            Some('.') => {
                self.pos += 1;
                self.push(TokenKind::Punctuation, start);
            }
            // An inline table's `}` with no key after the last comma --
            // `{ a = 1, }` is not legal TOML, but it is what one looks
            // like halfway through being written.
            Some('}') | Some(',') | Some(']') => self.structure(),
            // A quoted key is still a key, not a string: what it names
            // is a field, and colouring it as a value would say
            // otherwise.
            Some('"') | Some('\'') => {
                self.string();
                self.push(TokenKind::Key, start);
            }
            Some(c) if is_bare_key(c) => {
                while matches!(self.peek(), Some(c) if is_bare_key(c)) {
                    self.pos += 1;
                }
                self.push(TokenKind::Key, start);
            }
            _ => {
                self.pos += 1;
                self.push(TokenKind::Invalid, start);
            }
        }
    }

    fn value_token(&mut self) {
        let start = self.pos;
        match self.peek() {
            Some('"') | Some('\'') => {
                let escapes = self.string();
                self.out.push(Token { kind: TokenKind::Str { escapes }, start, end: self.pos });
                self.after_value();
            }
            Some('[') | Some(']') | Some('{') | Some('}') | Some(',') => self.structure(),
            // `=` in value position is a second one on the same pair --
            // not TOML, but not worth mistaking for anything either.
            Some('=') => {
                self.pos += 1;
                self.push(TokenKind::Punctuation, start);
            }
            Some(c) if c.is_ascii_alphanumeric() || c == '+' || c == '-' => {
                while matches!(self.peek(), Some(c) if is_scalar(c)) {
                    self.pos += 1;
                }
                let text: String = self.chars[start..self.pos].iter().collect();
                let kind = if text == "true" || text == "false" {
                    TokenKind::Bool
                } else if is_date_or_time(&text) {
                    // `1979-05-27 07:32:00` -- a space between the date
                    // and the time is legal, and is the one place in
                    // TOML where whitespace sits inside a single value.
                    self.maybe_join_time(&text);
                    TokenKind::DateTime
                } else if is_number(&text) {
                    TokenKind::Number
                } else {
                    TokenKind::Invalid
                };
                self.push(kind, start);
                self.after_value();
            }
            _ => {
                self.pos += 1;
                self.push(TokenKind::Invalid, start);
            }
        }
    }

    // The brackets and braces that open and close arrays and inline
    // tables, and the commas between their elements. Each one moves the
    // context, which is what decides whether the next word is a key.
    fn structure(&mut self) {
        let start = self.pos;
        let c = self.peek().unwrap_or(' ');
        self.pos += 1;
        self.push(TokenKind::Punctuation, start);
        match c {
            '[' => {
                self.stack.push(Ctx::Array);
                self.expect = Expect::Value;
            }
            '{' => {
                self.stack.push(Ctx::InlineTable);
                self.expect = Expect::Key;
            }
            ']' | '}' => {
                self.stack.pop();
                self.after_value();
            }
            ',' => {
                // A comma separates array elements (values) but inline
                // table entries (which start with a key).
                self.expect = match self.stack.last() {
                    Some(Ctx::InlineTable) => Expect::Key,
                    _ => Expect::Value,
                };
            }
            _ => {}
        }
    }

    // What follows a completed value: another element in an array, a
    // comma in an inline table, or -- at the top level -- the end of the
    // pair, which the newline handler above takes care of.
    fn after_value(&mut self) {
        self.expect = Expect::Value;
    }

    // A date with no time attached may still gain one after a single
    // space. Only then: `key = 1979-05-27` followed by a comment or
    // another pair must not absorb what comes next.
    fn maybe_join_time(&mut self, text: &str) {
        if text.contains(':') || self.peek() != Some(' ') {
            return;
        }
        let looks_like_time = self.at(1).is_some_and(|c| c.is_ascii_digit())
            && self.at(2).is_some_and(|c| c.is_ascii_digit())
            && self.at(3) == Some(':');
        if !looks_like_time {
            return;
        }
        self.pos += 1;
        while matches!(self.peek(), Some(c) if is_scalar(c)) {
            self.pos += 1;
        }
    }

    // Consumes a string of whichever of TOML's four kinds starts here,
    // returning its escape spans (always empty for a literal one).
    // Never runs past the end of the input, and a single-quoted or
    // single-double-quoted string never runs past its own line -- so
    // typing an opening quote doesn't restyle the rest of the file.
    fn string(&mut self) -> Vec<Range<usize>> {
        let quote = self.peek().unwrap_or('"');
        let multiline = self.at(1) == Some(quote) && self.at(2) == Some(quote);
        let escaped = quote == '"';
        self.pos += if multiline { 3 } else { 1 };
        let mut escapes = Vec::new();
        while let Some(c) = self.peek() {
            if c == '\n' && !multiline {
                break;
            }
            if c == '\\' && escaped {
                let at = self.pos;
                self.pos += 1;
                // `\uXXXX` and `\UXXXXXXXX` are the long ones; every
                // other escape is the backslash and one character.
                let width = match self.peek() {
                    Some('u') => 5,
                    Some('U') => 9,
                    Some(_) => 1,
                    None => 0,
                };
                self.pos = (self.pos + width).min(self.chars.len());
                escapes.push(at..self.pos);
                continue;
            }
            if c == quote {
                if !multiline {
                    self.pos += 1;
                    return escapes;
                }
                if self.at(1) == Some(quote) && self.at(2) == Some(quote) {
                    self.pos += 3;
                    return escapes;
                }
            }
            self.pos += 1;
        }
        escapes
    }
}

fn is_bare_key(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

// The characters a scalar (number, boolean or date-time) can be spelled
// with, so one can be taken in a single run and identified afterwards.
fn is_scalar(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.' | ':')
}

// `1979-05-27...` or `07:32:00...`. Shape only -- whether the fields are
// in range is a validator's question, and colouring `1979-13-45` as a
// date says what it was written as, which is the honest answer.
fn is_date_or_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |n: usize| bytes.len() > n && bytes[..n].iter().all(|b| b.is_ascii_digit());
    (digits(4) && bytes[4] == b'-') || (digits(2) && bytes[2] == b':')
}

fn is_number(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if matches!(body, "inf" | "nan") {
        return true;
    }
    // Prefixed integers, which have their own alphabets and no sign.
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let rest = rest.replace('_', "");
            return !rest.is_empty() && rest.chars().all(|c| c.is_digit(radix));
        }
    }
    // Underscores are digit separators anywhere between digits; being
    // strict about *where* is a validator's job, not a colour's.
    let plain = body.replace('_', "");
    !plain.is_empty() && plain.chars().next().is_some_and(|c| c.is_ascii_digit()) && plain.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each token as (its text, its kind), which reads like what you'd
    // see rather than like a list of offsets.
    fn lex(text: &str) -> Vec<(String, TokenKind)> {
        let chars: Vec<char> = text.chars().collect();
        tokens(text).into_iter().map(|t| (chars[t.start..t.end].iter().collect(), t.kind)).collect()
    }

    fn kinds(text: &str) -> Vec<TokenKind> {
        lex(text).into_iter().map(|(_, k)| k).collect()
    }

    fn str_kind() -> TokenKind {
        TokenKind::Str { escapes: Vec::new() }
    }

    #[test]
    fn a_pair_is_a_key_an_equals_and_a_value() {
        assert_eq!(
            lex("title = \"bish\""),
            vec![
                ("title".to_string(), TokenKind::Key),
                ("=".to_string(), TokenKind::Punctuation),
                ("\"bish\"".to_string(), str_kind()),
            ]
        );
    }

    // The reason this carries a context at all: the same word is a key
    // on the left of `=` and a value on the right.
    #[test]
    fn the_same_word_is_a_key_or_a_value_depending_on_the_side() {
        assert_eq!(kinds("true = false"), vec![TokenKind::Key, TokenKind::Punctuation, TokenKind::Bool]);
        assert_eq!(kinds("inf = 1"), vec![TokenKind::Key, TokenKind::Punctuation, TokenKind::Number]);
    }

    #[test]
    fn dotted_keys_are_segments_and_dots() {
        assert_eq!(
            kinds("a.b.c = 1"),
            vec![
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Number,
            ]
        );
    }

    #[test]
    fn a_table_header_is_one_token() {
        assert_eq!(lex("[servers.alpha]"), vec![("[servers.alpha]".to_string(), TokenKind::TableHeader)]);
        assert_eq!(lex("[[bin]]"), vec![("[[bin]]".to_string(), TokenKind::TableHeader)]);
    }

    // ...with a quoted segment layered inside it, so a header holding a
    // string still shows one.
    #[test]
    fn a_quoted_segment_of_a_header_is_also_a_string() {
        assert_eq!(
            lex("[a.\"b.c\"]"),
            vec![("[a.\"b.c\"]".to_string(), TokenKind::TableHeader), ("\"b.c\"".to_string(), str_kind())]
        );
    }

    // A `[` after a value opens an array; only one at the start of a
    // line opens a table.
    #[test]
    fn a_bracket_is_a_table_only_at_the_start_of_a_line() {
        assert_eq!(kinds("ports = [1, 2]").last(), Some(&TokenKind::Punctuation));
        assert!(!kinds("ports = [1, 2]").contains(&TokenKind::TableHeader));
        assert_eq!(kinds("[table]"), vec![TokenKind::TableHeader]);
    }

    // The case a per-line scan gets wrong: inside an array a newline is
    // whitespace, so what follows is still a value, not a new key.
    #[test]
    fn an_array_spanning_lines_stays_in_value_position() {
        assert_eq!(
            kinds("x = [\n  1,\n  2,\n]"),
            vec![
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Punctuation,
                TokenKind::Punctuation,
            ]
        );
    }

    // An inline table's own entries start with keys, which is what makes
    // a comma mean something different inside `{}` than inside `[]`.
    #[test]
    fn an_inline_table_has_keys_inside_it() {
        assert_eq!(
            kinds("p = { x = 1, y = 2 }"),
            vec![
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Punctuation,
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Punctuation,
                TokenKind::Key,
                TokenKind::Punctuation,
                TokenKind::Number,
                TokenKind::Punctuation,
            ]
        );
    }

    #[test]
    fn an_array_of_inline_tables_nests_correctly() {
        let k = kinds("a = [{ b = 1 }, { c = 2 }]");
        assert_eq!(k.iter().filter(|k| **k == TokenKind::Key).count(), 3, "a, b and c");
        assert_eq!(k.iter().filter(|k| **k == TokenKind::Number).count(), 2);
    }

    #[test]
    fn every_number_spelling_is_a_number() {
        for text in ["1", "+99", "-17", "1_000_000", "0xDEADBEEF", "0o755", "0b1101", "3.14", "5e+22", "-inf", "nan", "6.626e-34"] {
            assert_eq!(kinds(&format!("k = {text}")).last(), Some(&TokenKind::Number), "{text}");
        }
    }

    #[test]
    fn every_datetime_spelling_is_a_datetime() {
        for text in ["1979-05-27", "07:32:00", "00:32:00.999999", "1979-05-27T07:32:00Z", "1979-05-27T00:32:00-07:00"] {
            assert_eq!(kinds(&format!("k = {text}")).last(), Some(&TokenKind::DateTime), "{text}");
        }
    }

    // The one place whitespace sits inside a single TOML value.
    #[test]
    fn a_date_and_a_time_separated_by_a_space_are_one_value() {
        assert_eq!(lex("k = 1979-05-27 07:32:00").last().unwrap().0, "1979-05-27 07:32:00");
    }

    // ...but only when a time really follows.
    #[test]
    fn a_date_does_not_swallow_what_merely_comes_after_it() {
        assert_eq!(lex("k = 1979-05-27 # when").last().unwrap().0, "# when");
        assert_eq!(lex("k = 1979-05-27\nj = 1")[2].0, "1979-05-27");
    }

    #[test]
    fn all_four_string_kinds_are_strings() {
        for text in ["\"basic\"", "'literal'", "\"\"\"multi\nline\"\"\"", "'''multi\nline'''"] {
            assert_eq!(kinds(&format!("k = {text}")).last(), Some(&str_kind()), "{text}");
        }
    }

    #[test]
    fn escapes_are_marked_in_a_basic_string_and_not_in_a_literal_one() {
        let escapes = |text: &str| match tokens(text).pop().unwrap().kind {
            TokenKind::Str { escapes } => escapes.len(),
            other => panic!("expected a string, got {other:?}"),
        };
        assert_eq!(escapes("k = \"a\\nb\\u00e4\""), 2);
        assert_eq!(escapes("k = 'a\\nb'"), 0, "a literal string has no escapes");
    }

    // A single-quoted string ends at its own line, so an unterminated
    // one doesn't restyle everything below it.
    #[test]
    fn an_unterminated_single_line_string_ends_at_its_line() {
        let toks = lex("k = \"open\nj = 1");
        assert_eq!(toks[2].0, "\"open");
        assert_eq!(toks[3].0, "j");
        assert_eq!(toks[3].1, TokenKind::Key);
    }

    // ...whereas a `"""` string is meant to span lines and does.
    #[test]
    fn a_multiline_string_really_spans_lines() {
        let toks = lex("k = \"\"\"a\nb\"\"\"\nj = 1");
        assert_eq!(toks[2].0, "\"\"\"a\nb\"\"\"");
        assert_eq!(toks[3].1, TokenKind::Key);
    }

    #[test]
    fn comments_run_to_the_end_of_their_line() {
        assert_eq!(
            lex("# a note\nk = 1 # another"),
            vec![
                ("# a note".to_string(), TokenKind::Comment),
                ("k".to_string(), TokenKind::Key),
                ("=".to_string(), TokenKind::Punctuation),
                ("1".to_string(), TokenKind::Number),
                ("# another".to_string(), TokenKind::Comment),
            ]
        );
    }

    // A `#` inside a string is text, which is the whole reason strings
    // are consumed before comments are looked for.
    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        assert_eq!(lex("k = \"a # b\"").last().unwrap().0, "\"a # b\"");
    }

    #[test]
    fn spans_are_char_offsets() {
        let text = "k = \"\u{e4}\u{e4}\"\nj = 1";
        let j = tokens(text).into_iter().find(|t| t.kind == TokenKind::Key && t.start > 0).unwrap();
        assert_eq!(j.start, 9, "char offsets, not the 11 bytes this lands at");
    }

    #[test]
    fn nothing_typeable_panics() {
        for text in ["", "[", "[[", "]", "=", ".", "\"", "'", "'''", "\"\"\"", "{", "}", ",", "k =", "k = [", "\\", "k = 0x", "#"] {
            tokens(text);
        }
    }

    // Every token's text, concatenated with the gaps, must be the input
    // back again: nothing overlapping, nothing out of order, nothing
    // invented. (Table headers are skipped -- they deliberately overlap
    // the quoted segments they contain.)
    #[test]
    fn tokens_cover_the_input_in_order_without_overlapping() {
        let text = "# c\n[a.b]\nx = 1\ny = [1, {z = \"s\"}]\nd = 1979-05-27T07:32:00Z\n";
        let mut last = 0;
        for tok in tokens(text) {
            if matches!(tok.kind, TokenKind::Str { .. }) && tok.start < last {
                continue; // inside a table header
            }
            assert!(tok.start >= last, "token at {} overlaps the one before it", tok.start);
            assert!(tok.end > tok.start, "empty token at {}", tok.start);
            last = tok.end;
        }
        assert!(last <= text.chars().count());
    }
}
