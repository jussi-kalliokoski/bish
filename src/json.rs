// Hand-rolled JSON parser plus a small pretty-printer and `jq`-lite
// dotted-path query -- no external crate, same spirit as glob.rs/
// regex.rs/csscolor.rs. Backs the `json` builtin (exec.rs). Only ever
// needs to *parse* and *extract*, never re-serialize an arbitrary Rust
// value into JSON (nothing in this codebase produces JSON output other
// than by re-printing a `Value` this same parser already built), so
// there's no separate "serialize any type" API -- just Value's own
// pretty_print.
//
// The path query is a small, fixed grammar (`.foo.bar[2]`, `.["key
// with spaces"]`, `.`) -- not a full jq filter/expression language (no
// pipes, `map`/`select`/`length`, arithmetic, ...). Scoped to the
// overwhelmingly common "pull one value out of some JSON" a shell
// script actually needs, see bish-before-deps.md's own note on why
// this stops here rather than growing into a second parser for jq's
// own filter language.

use std::fmt::Write as _;
use std::iter::Peekable;
use std::ops::Range;
use std::str::Chars;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<Value>),
    // `Vec<(String, Value)>`, not a HashMap/BTreeMap -- preserves the
    // source's own field order (what a human expects from a pretty-
    // printer, and what real `jq` does too), at the cost of O(n) field
    // lookup instead of O(1). Never a concern at the sizes this is
    // actually used for (shell config/API-response-sized JSON, not a
    // database export) -- not worth a HashMap-with-a-side-order-Vec
    // just to shave lookup cost nothing here is sensitive to.
    Object(Vec<(String, Value)>),
}

const NULL: Value = Value::Null;

// One lexical piece of JSON, with where it sits in the input -- `start`
// and `end` are *char* offsets (not bytes), because the one consumer that
// cares about position is syntax highlighting, whose spans index a
// `&[char]` (see bishedit::highlight::compose).
//
// The tokenizer exists so there is exactly one JSON scanner in this
// repo: `parse` below is built on it, and so is bishedit::highlight's
// JsonHighlighter. A highlighter needs positions and must survive
// nonsense (a buffer mid-keystroke is nearly always invalid JSON),
// neither of which a value parser wants to carry -- so those two needs
// meet here, at the token, instead of in two scanners that would
// disagree about what a string is the first time one of them grew an
// escape rule the other didn't.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
    // A `Str` token's own escape sequences (`\n`, `\uXXXX`, ...) as char
    // spans into the whole input -- what lets a highlighter mark them
    // inside the string the way printf's `%s` is marked inside its own
    // argument. Empty for every other kind, and ignored by `parse`,
    // which only wants the decoded value.
    pub escapes: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    Comma,
    Colon,
    // Already unescaped -- the raw source text is `start..end` if a
    // caller wants it back.
    Str(String),
    Number(f64),
    Bool(bool),
    Null,
    // `//` to end of line, or `/* ... */`. Only ever emitted by
    // `tokens_with_comments` -- strict JSON has no comments, and
    // `parse`, which reads the strict stream, therefore never sees one.
    Comment,
    // Whatever couldn't be read as any of the above, carrying the reason
    // for `parse` to report. Tokenizing never fails: it emits one of
    // these and keeps going, so a half-typed buffer still tokenizes into
    // something a highlighter can colour up to (and past) the mistake.
    Invalid(String),
}

// Every token in `input`, whitespace dropped. Never fails -- see
// TokenKind::Invalid.
pub fn tokens(input: &str) -> Vec<Token> {
    lex(input, false)
}

// The same, for JSONC -- JSON as `tsconfig.json` and every `.vscode`
// file actually write it, with `//` and `/* */` comments. Comments come
// back as real `TokenKind::Comment` tokens rather than being skipped,
// since the one consumer that wants this stream is a highlighter and a
// comment it can't see is a comment it can't colour.
//
// The other thing JSONC permits, a trailing comma before `}`/`]`, needs
// nothing here: a comma is the same token wherever it sits, and whether
// one is allowed in that position is a question for a parser, which is
// not what this stream is for.
pub fn tokens_with_comments(input: &str) -> Vec<Token> {
    lex(input, true)
}

fn lex(input: &str, comments: bool) -> Vec<Token> {
    let mut lexer = Lexer { chars: input.chars().peekable(), pos: 0, comments };
    let mut out = Vec::new();
    while let Some(tok) = lexer.next_token() {
        out.push(tok);
    }
    out
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    pos: usize,
    // Whether `/`-introduced text is a comment or, as in strict JSON, a
    // character that can't start a value.
    comments: bool,
}

impl Lexer<'_> {
    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn eat(&mut self, c: char) -> bool {
        if self.chars.peek() == Some(&c) {
            self.advance();
            return true;
        }
        false
    }

    // JSON itself allows only space/tab/CR/LF between tokens; this
    // accepts anything char::is_whitespace does, which is what the
    // hand-rolled parser this replaced already did -- kept deliberately,
    // since narrowing it would start rejecting input the `json` builtin
    // has always accepted.
    fn skip_ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.advance();
        }
    }

    // `// ...` to the end of the line, or `/* ... */` -- unterminated,
    // it runs to the end of the input, which is what a comment being
    // typed looks like.
    fn comment(&mut self) -> Token {
        let start = self.pos;
        self.advance();
        let line = self.eat('/');
        if !line {
            self.advance();
        }
        loop {
            match self.chars.peek() {
                None => break,
                Some('\n') if line => break,
                Some('*') if !line => {
                    self.advance();
                    if self.eat('/') {
                        break;
                    }
                }
                _ => {
                    self.advance();
                }
            }
        }
        Token { kind: TokenKind::Comment, start, end: self.pos, escapes: Vec::new() }
    }

    fn next_token(&mut self) -> Option<Token> {
        self.skip_ws();
        if self.comments && self.chars.peek() == Some(&'/') {
            return Some(self.comment());
        }
        let start = self.pos;
        let single = |kind| Some(kind);
        let kind = match *self.chars.peek()? {
            '{' => single(TokenKind::OpenBrace),
            '}' => single(TokenKind::CloseBrace),
            '[' => single(TokenKind::OpenBracket),
            ']' => single(TokenKind::CloseBracket),
            ',' => single(TokenKind::Comma),
            ':' => single(TokenKind::Colon),
            _ => None,
        };
        if let Some(kind) = kind {
            self.advance();
            return Some(Token { kind, start, end: self.pos, escapes: Vec::new() });
        }
        let mut escapes = Vec::new();
        let kind = match *self.chars.peek()? {
            '"' => self.lex_string(&mut escapes),
            c if c == '-' || c.is_ascii_digit() => self.lex_number(),
            c if c.is_ascii_alphabetic() => self.lex_word(),
            c => {
                self.advance();
                TokenKind::Invalid(format!("unexpected '{c}'"))
            }
        };
        Some(Token { kind, start, end: self.pos, escapes })
    }

    // An unterminated string stops at the end of its line rather than
    // swallowing the rest of the input: a raw newline inside a string is
    // invalid JSON anyway, so this loses nothing -- and it means typing
    // an opening quote in the editor doesn't briefly restyle every line
    // below it.
    fn lex_string(&mut self, escapes: &mut Vec<Range<usize>>) -> TokenKind {
        self.advance();
        let mut s = String::new();
        loop {
            match self.chars.peek() {
                None | Some('\n') => return TokenKind::Invalid("unterminated string".to_string()),
                Some('"') => {
                    self.advance();
                    return TokenKind::Str(s);
                }
                Some('\\') => {
                    let esc_start = self.pos;
                    self.advance();
                    match self.escape_char() {
                        Ok(c) => {
                            s.push(c);
                            escapes.push(esc_start..self.pos);
                        }
                        Err(e) => return TokenKind::Invalid(e),
                    }
                }
                Some(_) => s.push(self.advance().unwrap()),
            }
        }
    }

    // Everything after a backslash inside a string.
    fn escape_char(&mut self) -> Result<char, String> {
        match self.advance() {
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('/') => Ok('/'),
            Some('b') => Ok('\u{8}'),
            Some('f') => Ok('\u{c}'),
            Some('n') => Ok('\n'),
            Some('r') => Ok('\r'),
            Some('t') => Ok('\t'),
            Some('u') => {
                let hi = self.hex4()?;
                // A surrogate pair (\uD800-\uDBFF followed by
                // \uDC00-\uDFFF) encodes one codepoint outside the BMP
                // (most commonly an emoji) -- combined here rather than
                // left as two lone surrogates, which char::from_u32
                // would reject outright.
                if (0xD800..=0xDBFF).contains(&hi) {
                    if !self.eat('\\') || !self.eat('u') {
                        return Err("expected a low surrogate after a high surrogate".to_string());
                    }
                    let lo = self.hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err("invalid low surrogate".to_string());
                    }
                    let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    char::from_u32(c).ok_or_else(|| "invalid surrogate pair".to_string())
                } else {
                    char::from_u32(hi).ok_or_else(|| "invalid \\u escape".to_string())
                }
            }
            None => Err("unterminated string".to_string()),
            Some(c) => Err(format!("invalid escape '\\{c}'")),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.advance().ok_or_else(|| "unterminated \\u escape".to_string())?;
            let d = c.to_digit(16).ok_or_else(|| format!("invalid hex digit '{c}' in \\u escape"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    // Deliberately more permissive than JSON's own number grammar (which
    // rejects a leading zero, a bare `-`, and `.5`): this scans the
    // characters a number is made of and lets f64's own parse be the
    // judge, which is what the parser this replaced did too. Tightening
    // it would start rejecting input the `json` builtin has accepted all
    // along, for no benefit to anything that reads JSON rather than
    // certifying it.
    fn lex_number(&mut self) -> TokenKind {
        let mut raw = String::new();
        if self.chars.peek() == Some(&'-') {
            raw.push(self.advance().unwrap());
        }
        let digits = |lexer: &mut Self, raw: &mut String| {
            while matches!(lexer.chars.peek(), Some(c) if c.is_ascii_digit()) {
                raw.push(lexer.advance().unwrap());
            }
        };
        digits(self, &mut raw);
        if self.chars.peek() == Some(&'.') {
            raw.push(self.advance().unwrap());
            digits(self, &mut raw);
        }
        if matches!(self.chars.peek(), Some('e') | Some('E')) {
            raw.push(self.advance().unwrap());
            if matches!(self.chars.peek(), Some('+') | Some('-')) {
                raw.push(self.advance().unwrap());
            }
            digits(self, &mut raw);
        }
        match raw.parse::<f64>() {
            Ok(n) => TokenKind::Number(n),
            Err(_) => TokenKind::Invalid(format!("invalid number '{raw}'")),
        }
    }

    // A bare alphabetic run: one of the three JSON literals, or a word
    // that only looks like one. Taken whole (rather than character by
    // character against each literal in turn) so `tru` reports itself as
    // one wrong word instead of decaying into a pile of stray-character
    // errors -- and so a highlighter has one span to leave alone.
    fn lex_word(&mut self) -> TokenKind {
        let mut word = String::new();
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_alphabetic()) {
            word.push(self.advance().unwrap());
        }
        match word.as_str() {
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            "null" => TokenKind::Null,
            _ => TokenKind::Invalid(format!("invalid literal '{word}'")),
        }
    }
}

pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser { toks: tokens(input).into_iter().peekable() };
    let v = p.parse_value()?;
    match p.toks.next() {
        None => Ok(v),
        Some(t) => Err(format!("trailing data after the JSON value (position {})", t.start)),
    }
}

struct Parser {
    toks: Peekable<std::vec::IntoIter<Token>>,
}

impl Parser {
    fn next_token(&mut self) -> Result<Token, String> {
        self.toks.next().ok_or_else(|| "unexpected end of input".to_string())
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        let tok = self.next_token()?;
        let at = tok.start;
        match tok.kind {
            TokenKind::OpenBrace => self.parse_object(),
            TokenKind::OpenBracket => self.parse_array(),
            TokenKind::Str(s) => Ok(Value::Str(s)),
            TokenKind::Number(n) => Ok(Value::Number(n)),
            TokenKind::Bool(b) => Ok(Value::Bool(b)),
            TokenKind::Null => Ok(Value::Null),
            TokenKind::Invalid(reason) => Err(format!("{reason} (position {at})")),
            other => Err(format!("unexpected {} (position {at})", describe(&other))),
        }
    }

    // The open brace is already consumed.
    fn parse_object(&mut self) -> Result<Value, String> {
        let mut fields = Vec::new();
        if matches!(self.toks.peek(), Some(t) if t.kind == TokenKind::CloseBrace) {
            self.toks.next();
            return Ok(Value::Object(fields));
        }
        loop {
            let tok = self.next_token()?;
            let at = tok.start;
            let TokenKind::Str(key) = tok.kind else {
                return Err(format!("expected a string key in object (got {}, position {at})", describe(&tok.kind)));
            };
            let colon = self.next_token()?;
            if colon.kind != TokenKind::Colon {
                return Err(format!(
                    "expected ':' after an object key (got {}, position {})",
                    describe(&colon.kind),
                    colon.start
                ));
            }
            fields.push((key, self.parse_value()?));
            let tok = self.next_token()?;
            match tok.kind {
                TokenKind::Comma => continue,
                TokenKind::CloseBrace => break,
                other => {
                    return Err(format!(
                        "expected ',' or closing brace in object (got {}, position {})",
                        describe(&other),
                        tok.start
                    ))
                }
            }
        }
        Ok(Value::Object(fields))
    }

    // The open bracket is already consumed.
    fn parse_array(&mut self) -> Result<Value, String> {
        let mut items = Vec::new();
        if matches!(self.toks.peek(), Some(t) if t.kind == TokenKind::CloseBracket) {
            self.toks.next();
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            let tok = self.next_token()?;
            match tok.kind {
                TokenKind::Comma => continue,
                TokenKind::CloseBracket => break,
                other => {
                    return Err(format!("expected ',' or ']' in array (got {}, position {})", describe(&other), tok.start))
                }
            }
        }
        Ok(Value::Array(items))
    }
}

// How a token reads inside a parse error -- the source spelling for
// punctuation, a word for anything with a value in it, since printing an
// entire nested string or number back at the reader says less about what
// went wrong than "a string" does.
fn describe(kind: &TokenKind) -> &'static str {
    match kind {
        TokenKind::OpenBrace => "'{'",
        TokenKind::CloseBrace => "'}'",
        TokenKind::OpenBracket => "'['",
        TokenKind::CloseBracket => "']'",
        TokenKind::Comma => "','",
        TokenKind::Colon => "':'",
        TokenKind::Str(_) => "a string",
        TokenKind::Number(_) => "a number",
        TokenKind::Bool(_) => "a boolean",
        TokenKind::Null => "null",
        // Unreachable through `parse`, which reads the strict stream --
        // named rather than lumped in with Invalid so that if a JSONC
        // stream is ever handed to a parser, it says what it saw.
        TokenKind::Comment => "a comment",
        TokenKind::Invalid(_) => "invalid input",
    }
}

pub fn pretty_print(v: &Value) -> String {
    let mut out = String::new();
    write_pretty(&mut out, v, 0);
    out
}

fn write_pretty(out: &mut String, v: &Value, indent: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, *n),
        Value::Str(s) => write_json_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&"  ".repeat(indent + 1));
                write_pretty(out, item, indent + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push(']');
        }
        Value::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (k, val)) in fields.iter().enumerate() {
                out.push_str(&"  ".repeat(indent + 1));
                write_json_string(out, k);
                out.push_str(": ");
                write_pretty(out, val, indent + 1);
                if i + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&"  ".repeat(indent));
            out.push('}');
        }
    }
}

// JSON numbers have no int/float distinction of their own; printed
// without a trailing ".0" when the value is a whole number that fits
// exactly (matching how every JSON value this shape actually came from
// -- an integer field in the source -- would have looked to begin
// with), the ordinary float rendering otherwise.
fn write_number(out: &mut String, n: f64) {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        let _ = write!(out, "{}", n as i64);
    } else {
        let _ = write!(out, "{n}");
    }
}

fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// `.foo.bar[2]`/`.["key with spaces"]`/`.` -- a missing field or an
// out-of-range index resolves to `Null` rather than an error (matching
// real `jq`'s own "absent means null" convention), so a script can
// write `json .maybe_missing file.json` and get a clean "null" instead
// of a parse-time-shaped failure for something that's a perfectly
// ordinary runtime outcome. A malformed *path expression* itself (bad
// syntax, not a bad lookup) is still a real Err.
pub fn query<'a>(root: &'a Value, path: &str) -> Result<&'a Value, String> {
    let path = path.trim();
    if path.is_empty() || path == "." {
        return Ok(root);
    }
    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;
    let mut cur = root;
    while i < chars.len() {
        match chars[i] {
            '.' => {
                i += 1;
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if start == i {
                    return Err(format!("expected a field name after '.' (position {i})"));
                }
                let key: String = chars[start..i].iter().collect();
                cur = field(cur, &key);
            }
            '[' => {
                i += 1;
                if chars.get(i) == Some(&'"') {
                    i += 1;
                    let start = i;
                    while i < chars.len() && chars[i] != '"' {
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err("unterminated quoted key in path".to_string());
                    }
                    let key: String = chars[start..i].iter().collect();
                    i += 1; // closing quote
                    if chars.get(i) != Some(&']') {
                        return Err(format!("expected ']' after a quoted key (position {i})"));
                    }
                    i += 1;
                    cur = field(cur, &key);
                } else {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                    if start == i || chars.get(i) != Some(&']') {
                        return Err(format!("expected a numeric index in [...] (position {i})"));
                    }
                    let idx: usize = chars[start..i].iter().collect::<String>().parse().map_err(|_| "invalid index".to_string())?;
                    i += 1;
                    cur = match cur {
                        Value::Array(items) => items.get(idx).unwrap_or(&NULL),
                        _ => &NULL,
                    };
                }
            }
            other => return Err(format!("unexpected '{other}' in path (position {i})")),
        }
    }
    Ok(cur)
}

fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    match v {
        Value::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v).unwrap_or(&NULL),
        _ => &NULL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        tokens(input).into_iter().map(|t| t.kind).collect()
    }

    // Char offsets, not bytes -- the é is two bytes and one column, and
    // a highlighter that got this wrong would paint everything after it
    // one position off.
    #[test]
    fn token_spans_are_char_offsets_covering_each_token_exactly() {
        let toks = tokens(r#"{"é": 12}"#);
        let spans: Vec<(usize, usize)> = toks.iter().map(|t| (t.start, t.end)).collect();
        assert_eq!(spans, vec![(0, 1), (1, 4), (4, 5), (6, 8), (8, 9)]);
    }

    #[test]
    fn tokenizes_every_kind_and_drops_whitespace() {
        assert_eq!(
            kinds("{ \"a\" : [ 1, true , null ] }"),
            vec![
                TokenKind::OpenBrace,
                TokenKind::Str("a".to_string()),
                TokenKind::Colon,
                TokenKind::OpenBracket,
                TokenKind::Number(1.0),
                TokenKind::Comma,
                TokenKind::Bool(true),
                TokenKind::Comma,
                TokenKind::Null,
                TokenKind::CloseBracket,
                TokenKind::CloseBrace,
            ]
        );
    }

    // The property the highlighter depends on and the parser doesn't:
    // tokenizing never gives up, so everything after a mistake is still
    // read as itself rather than lost.
    #[test]
    fn tokenizing_recovers_after_invalid_input_instead_of_stopping() {
        let toks = kinds("[tru, 1]");
        assert_eq!(toks[0], TokenKind::OpenBracket);
        assert!(matches!(toks[1], TokenKind::Invalid(_)), "{:?}", toks[1]);
        assert_eq!(toks[2..], [TokenKind::Comma, TokenKind::Number(1.0), TokenKind::CloseBracket]);
    }

    // An open quote must not swallow the rest of the buffer: a raw
    // newline can't appear in a JSON string anyway, so ending the bad
    // token at the line break costs nothing and keeps every line below
    // it highlighted as itself while it's being typed.
    #[test]
    fn an_unterminated_string_ends_at_its_own_line() {
        let toks = tokens("{\"a\n\"b\": 1}");
        assert!(matches!(toks[1].kind, TokenKind::Invalid(_)), "{:?}", toks[1].kind);
        assert_eq!(toks[1].end, 3, "stops before the newline");
        assert_eq!(toks[2].kind, TokenKind::Str("b".to_string()));
    }

    #[test]
    fn string_escapes_are_recorded_as_their_own_spans() {
        let toks = tokens(r#""a\nb\u00e9""#);
        assert_eq!(toks[0].kind, TokenKind::Str("a\nbé".to_string()));
        assert_eq!(toks[0].escapes, vec![2..4, 5..11]);
    }

    #[test]
    fn a_token_with_no_escapes_carries_none() {
        assert!(tokens(r#"{"a": 1}"#).iter().all(|t| t.escapes.is_empty()));
    }

    #[test]
    fn parses_every_scalar_kind() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("42").unwrap(), Value::Number(42.0));
        assert_eq!(parse("-3.5").unwrap(), Value::Number(-3.5));
        assert_eq!(parse("1e3").unwrap(), Value::Number(1000.0));
        assert_eq!(parse(r#""hi""#).unwrap(), Value::Str("hi".to_string()));
    }

    #[test]
    fn parses_string_escapes_including_unicode_and_surrogate_pairs() {
        assert_eq!(parse(r#""a\nb\t\"c""#).unwrap(), Value::Str("a\nb\t\"c".to_string()));
        assert_eq!(parse(r#""é""#).unwrap(), Value::Str("é".to_string()));
        // U+1F600 GRINNING FACE, encoded as a UTF-16 surrogate pair.
        assert_eq!(parse(r#""😀""#).unwrap(), Value::Str("😀".to_string()));
    }

    #[test]
    fn parses_nested_arrays_and_objects_preserving_field_order() {
        let v = parse(r#"{"b": 1, "a": [1, 2, {"c": null}]}"#).unwrap();
        match v {
            Value::Object(fields) => {
                assert_eq!(fields[0].0, "b");
                assert_eq!(fields[1].0, "a");
            }
            _ => panic!("expected an object"),
        }
    }

    #[test]
    fn rejects_trailing_data_and_malformed_input() {
        assert!(parse("42 43").is_err());
        assert!(parse("{").is_err());
        assert!(parse(r#"{"a":}"#).is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn pretty_print_round_trips_a_representative_document() {
        let src = r#"{"name":"bish","tags":["shell","editor"],"stable":false,"count":3}"#;
        let v = parse(src).unwrap();
        let out = pretty_print(&v);
        assert_eq!(
            out,
            "{\n  \"name\": \"bish\",\n  \"tags\": [\n    \"shell\",\n    \"editor\"\n  ],\n  \"stable\": false,\n  \"count\": 3\n}"
        );
    }

    #[test]
    fn query_navigates_dotted_fields_and_indices() {
        let v = parse(r#"{"a":{"b":[10,20,{"c":"deep"}]}}"#).unwrap();
        assert_eq!(query(&v, ".").unwrap(), &v);
        assert_eq!(query(&v, ".a.b[0]").unwrap(), &Value::Number(10.0));
        assert_eq!(query(&v, ".a.b[2].c").unwrap(), &Value::Str("deep".to_string()));
        assert_eq!(query(&v, r#".a["b"][1]"#).unwrap(), &Value::Number(20.0));
    }

    #[test]
    fn query_resolves_missing_fields_and_out_of_range_indices_to_null_not_an_error() {
        let v = parse(r#"{"a":1}"#).unwrap();
        assert_eq!(query(&v, ".missing").unwrap(), &Value::Null);
        assert_eq!(query(&v, ".a.also_missing").unwrap(), &Value::Null);
        let arr = parse("[1,2]").unwrap();
        assert_eq!(query(&arr, "[9]").unwrap(), &Value::Null);
    }

    #[test]
    fn query_rejects_malformed_path_syntax() {
        let v = parse("{}").unwrap();
        assert!(query(&v, ".[").is_err());
        assert!(query(&v, ".foo[abc]").is_err());
    }
}
