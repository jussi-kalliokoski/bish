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

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

// The lexer above exists to *colour* TOML: it never fails, and it emits
// a table header as one span so a highlighter can paint it whole. That
// is the wrong shape for reading a document, which has to fail, and has
// to fail somewhere in particular.
//
// So this is a second, direct recursive-descent pass over the same
// characters rather than a parser layered on those tokens. Two small
// readers of one bounded grammar, each shaped for its own job, beats one
// reader contorted to serve both -- the same call `dotenv.rs` makes
// about not being built on `ini.rs`.
//
// The result is a `json::Value`, which is not a compromise: TOML's data
// model is JSON's plus dates, and reusing it means `Cargo.toml` answers
// the same `.package.name` path query `json` already implements, and
// `:format` and every other consumer of that type comes along free. A
// date-time is carried as its own literal text -- there is no richer
// type here to put it in, and every TOML-to-JSON converter does the
// same.

/// Where a document stopped making sense, and why.
///
/// `at` is a **char** offset, like every other position in this file --
/// which is what lets `:diag` underline the actual mistake rather than
/// reporting it against the whole buffer the way a message with the
/// position baked into its text forces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub at: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Reads a whole TOML document.
///
/// Accepts TOML 1.0.0's grammar: bare/quoted/dotted keys, `[table]` and
/// `[[array of tables]]`, all four string forms, integers in every base
/// with underscores, floats including `inf`/`nan`, booleans, date-times,
/// arrays and inline tables.
///
/// What it deliberately does not do is *validate* a date-time beyond its
/// shape -- `1979-13-99` parses as the string it looks like. Checking
/// calendars is a different job from reading a config file, and nothing
/// downstream of this asks.
pub fn parse(text: &str) -> Result<crate::json::Value, ParseError> {
    let chars: Vec<char> = text.chars().collect();
    Parser { chars: &chars, pos: 0, root: Vec::new(), defined: Vec::new(), tables: Vec::new(), arrays: Vec::new() }.run()
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    // The document, as `json::Value::Object`'s own field list.
    root: Vec<(String, crate::json::Value)>,
    // Every key path a value was actually written to, for catching a
    // duplicate. Paths rather than a set of names because `a.b` and
    // `c.b` are different keys.
    defined: Vec<Vec<String>>,
    // Table headers seen, for catching `[a]` twice.
    tables: Vec<Vec<String>>,
    // Paths that are arrays of tables, so `[[a]]` appends where `[a]`
    // would collide.
    arrays: Vec<Vec<String>>,
}

impl<'a> Parser<'a> {
    fn run(mut self) -> Result<crate::json::Value, ParseError> {
        let mut path: Vec<String> = Vec::new();
        loop {
            self.skip_trivia();
            if self.pos >= self.chars.len() {
                return Ok(crate::json::Value::Object(self.root));
            }
            if self.peek() == Some('[') {
                path = self.table_header()?;
                continue;
            }
            let (key, value) = self.pair()?;
            let mut full = path.clone();
            full.extend(key);
            self.define(full)?;
            let at = self.pos;
            self.set(&full_path_of(&self.defined), value).map_err(|m| ParseError { at, message: m })?;
            self.end_of_line()?;
        }
    }

    // --- character helpers -------------------------------------------

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError { at: self.pos.min(self.chars.len().saturating_sub(1)), message: message.into() })
    }

    // Spaces and tabs only -- a newline is a separator here, never
    // trivia.
    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while !matches!(self.peek(), None | Some('\n')) {
                self.pos += 1;
            }
        }
    }

    // Everything between one thing and the next: blanks, comments and
    // newlines.
    fn skip_trivia(&mut self) {
        loop {
            self.skip_blanks();
            self.skip_comment();
            match self.peek() {
                Some('\n') | Some('\r') => self.pos += 1,
                _ => return,
            }
        }
    }

    // After a pair or a header, the rest of the line must be empty.
    // Catching this is most of what makes the difference between "this
    // file is broken" and pointing at where.
    fn end_of_line(&mut self) -> Result<(), ParseError> {
        self.skip_blanks();
        self.skip_comment();
        match self.peek() {
            None | Some('\n') => Ok(()),
            Some('\r') => Ok(()),
            Some(c) => self.err(format!("unexpected '{c}' after a value -- one key/value pair per line")),
        }
    }

    // --- keys ---------------------------------------------------------

    fn is_bare_key_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_' || c == '-'
    }

    // `a`, `"a b"`, `'a'`, or any of those joined by dots.
    fn dotted_key(&mut self) -> Result<Vec<String>, ParseError> {
        let mut parts = Vec::new();
        loop {
            self.skip_blanks();
            parts.push(self.key_segment()?);
            self.skip_blanks();
            if self.peek() == Some('.') {
                self.pos += 1;
                continue;
            }
            return Ok(parts);
        }
    }

    fn key_segment(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some('"') | Some('\'') => {
                let crate::json::Value::Str(s) = self.string()? else {
                    return self.err("expected a quoted key");
                };
                Ok(s)
            }
            Some(c) if Self::is_bare_key_char(c) => {
                let start = self.pos;
                while self.peek().is_some_and(Self::is_bare_key_char) {
                    self.pos += 1;
                }
                Ok(self.chars[start..self.pos].iter().collect())
            }
            Some(c) => self.err(format!("'{c}' cannot start a key")),
            None => self.err("expected a key"),
        }
    }

    // `[table]`, `[a.b]`, `[[array]]`. Returns the path the pairs that
    // follow belong to.
    fn table_header(&mut self) -> Result<Vec<String>, ParseError> {
        let opened = self.pos;
        self.pos += 1;
        let is_array = self.peek() == Some('[');
        if is_array {
            self.pos += 1;
        }
        let path = self.dotted_key()?;
        self.skip_blanks();
        if self.peek() != Some(']') {
            return Err(ParseError { at: opened, message: "unterminated table header -- expected ']'".to_string() });
        }
        self.pos += 1;
        if is_array {
            if self.peek() != Some(']') {
                return Err(ParseError { at: opened, message: "unterminated array-of-tables header -- expected ']]'".to_string() });
            }
            self.pos += 1;
        }
        if path.is_empty() {
            return Err(ParseError { at: opened, message: "empty table header".to_string() });
        }
        if is_array {
            self.push_array_table(&path).map_err(|m| ParseError { at: opened, message: m })?;
            if !self.arrays.contains(&path) {
                self.arrays.push(path.clone());
            }
            // A new element of the array starts empty, so the keys the
            // *previous* one defined are no longer taken. Without this,
            // two `[[fruit]]` blocks each with a `name` read as the same
            // key twice -- which is the whole idiom.
            self.defined.retain(|d| !d.starts_with(&path[..]));
        } else {
            if self.tables.contains(&path) || self.defined.contains(&path) {
                return Err(ParseError { at: opened, message: format!("table [{}] is defined twice", path.join(".")) });
            }
            self.tables.push(path.clone());
            self.ensure_table(&path).map_err(|m| ParseError { at: opened, message: m })?;
        }
        self.end_of_line()?;
        Ok(path)
    }

    // --- pairs and values ---------------------------------------------

    fn pair(&mut self) -> Result<(Vec<String>, crate::json::Value), ParseError> {
        let key = self.dotted_key()?;
        self.skip_blanks();
        if self.peek() != Some('=') {
            return self.err("expected '=' after a key");
        }
        self.pos += 1;
        self.skip_blanks();
        let value = self.value()?;
        Ok((key, value))
    }

    fn value(&mut self) -> Result<crate::json::Value, ParseError> {
        match self.peek() {
            Some('"') | Some('\'') => self.string(),
            Some('[') => self.array(),
            Some('{') => self.inline_table(),
            Some('t') | Some('f') => self.boolean(),
            Some(c) if c == '+' || c == '-' || c == 'i' || c == 'n' || c.is_ascii_digit() => self.number_or_datetime(),
            Some(c) => self.err(format!("'{c}' cannot start a value")),
            None => self.err("expected a value"),
        }
    }

    fn boolean(&mut self) -> Result<crate::json::Value, ParseError> {
        for (word, v) in [("true", true), ("false", false)] {
            if self.matches_word(word) {
                self.pos += word.chars().count();
                return Ok(crate::json::Value::Bool(v));
            }
        }
        self.err("expected `true` or `false`")
    }

    fn matches_word(&self, word: &str) -> bool {
        let mut i = self.pos;
        for c in word.chars() {
            if self.chars.get(i) != Some(&c) {
                return false;
            }
            i += 1;
        }
        // Not a prefix of something longer: `truely` is not `true`.
        !self.chars.get(i).copied().is_some_and(Self::is_bare_key_char)
    }

    // A date-time and a number start the same way, so this decides
    // between them by looking for the shape only a date has: four digits
    // and a `-`, or two digits and a `:`.
    fn number_or_datetime(&mut self) -> Result<crate::json::Value, ParseError> {
        if self.looks_like_datetime() {
            let start = self.pos;
            while self
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | ':' | '.' | '+'))
                || (self.peek() == Some(' ') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) && self.chars[start..self.pos].contains(&'-'))
            {
                self.pos += 1;
            }
            return Ok(crate::json::Value::Str(self.chars[start..self.pos].iter().collect()));
        }
        self.number()
    }

    fn looks_like_datetime(&self) -> bool {
        let digits = |n: usize| (0..n).all(|i| self.peek_at(i).is_some_and(|c| c.is_ascii_digit()));
        (digits(4) && self.peek_at(4) == Some('-')) || (digits(2) && self.peek_at(2) == Some(':'))
    }

    fn number(&mut self) -> Result<crate::json::Value, ParseError> {
        let start = self.pos;
        let mut sign = 1.0;
        if matches!(self.peek(), Some('+') | Some('-')) {
            if self.peek() == Some('-') {
                sign = -1.0;
            }
            self.pos += 1;
        }
        for (word, v) in [("inf", f64::INFINITY), ("nan", f64::NAN)] {
            if self.matches_word(word) {
                self.pos += 3;
                return Ok(crate::json::Value::Number(sign * v));
            }
        }
        // A base prefix is only legal without a sign, per TOML.
        if self.peek() == Some('0')
            && let Some(radix) = match self.peek_at(1) {
                Some('x') => Some(16),
                Some('o') => Some(8),
                Some('b') => Some(2),
                _ => None,
            }
        {
            {
                self.pos += 2;
                let body = self.take_number_body();
                let cleaned: String = body.chars().filter(|c| *c != '_').collect();
                return match i64::from_str_radix(&cleaned, radix) {
                    Ok(n) if !cleaned.is_empty() => Ok(crate::json::Value::Number(n as f64)),
                    _ => Err(ParseError { at: start, message: format!("'{body}' is not a base-{radix} integer") }),
                };
            }
        }
        let body = self.take_number_body();
        if body.is_empty() {
            return Err(ParseError { at: start, message: "expected a number".to_string() });
        }
        let cleaned: String = body.chars().filter(|c| *c != '_').collect();
        match cleaned.parse::<f64>() {
            Ok(n) => Ok(crate::json::Value::Number(sign * n)),
            Err(_) => Err(ParseError { at: start, message: format!("'{cleaned}' is not a number") }),
        }
    }

    fn take_number_body(&mut self) -> String {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-')) {
            // An exponent's own sign is part of the number; a `-`
            // anywhere else ends it.
            if matches!(self.peek(), Some('+') | Some('-')) && !matches!(self.chars.get(self.pos - 1), Some('e') | Some('E')) {
                break;
            }
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    // All four of TOML's strings. Returns the *value*, unquoted and
    // unescaped.
    fn string(&mut self) -> Result<crate::json::Value, ParseError> {
        let opened = self.pos;
        let quote = self.bump().expect("caller checked");
        let literal = quote == '\'';
        let multiline = self.peek() == Some(quote) && self.peek_at(1) == Some(quote);
        if multiline {
            self.pos += 2;
            // A newline immediately after the opening delimiter is not
            // part of the value.
            if self.peek() == Some('\r') {
                self.pos += 1;
            }
            if self.peek() == Some('\n') {
                self.pos += 1;
            }
        }
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(ParseError { at: opened, message: "unterminated string".to_string() }),
                Some(c) if c == quote => {
                    if !multiline {
                        self.pos += 1;
                        return Ok(crate::json::Value::Str(out));
                    }
                    if self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote) {
                        self.pos += 3;
                        return Ok(crate::json::Value::Str(out));
                    }
                    out.push(c);
                    self.pos += 1;
                }
                Some('\n') if !multiline => {
                    return Err(ParseError { at: opened, message: "unterminated string -- a single-quoted string cannot span lines".to_string() });
                }
                Some('\\') if !literal => {
                    self.pos += 1;
                    self.escape(&mut out, multiline)?;
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn escape(&mut self, out: &mut String, multiline: bool) -> Result<(), ParseError> {
        let at = self.pos.saturating_sub(1);
        let c = match self.bump() {
            Some(c) => c,
            None => return Err(ParseError { at, message: "unterminated escape".to_string() }),
        };
        let simple = match c {
            'b' => Some('\u{8}'),
            't' => Some('\t'),
            'n' => Some('\n'),
            'f' => Some('\u{c}'),
            'r' => Some('\r'),
            '"' => Some('"'),
            '\\' => Some('\\'),
            _ => None,
        };
        if let Some(c) = simple {
            out.push(c);
            return Ok(());
        }
        if c == 'u' || c == 'U' {
            let width = if c == 'u' { 4 } else { 8 };
            let mut n: u32 = 0;
            for _ in 0..width {
                let Some(d) = self.peek().and_then(|d| d.to_digit(16)) else {
                    return Err(ParseError { at, message: format!("\\{c} needs {width} hex digits") });
                };
                n = n * 16 + d;
                self.pos += 1;
            }
            return match char::from_u32(n) {
                Some(c) => {
                    out.push(c);
                    Ok(())
                }
                None => Err(ParseError { at, message: format!("\\{c}{n:04X} is not a character") }),
            };
        }
        // A backslash at the end of a line in a multi-line string eats
        // the newline and the whitespace after it.
        if multiline && (c == '\n' || c == '\r' || c == ' ' || c == '\t') {
            while matches!(self.peek(), Some(' ') | Some('\t') | Some('\n') | Some('\r')) {
                self.pos += 1;
            }
            return Ok(());
        }
        Err(ParseError { at, message: format!("'\\{c}' is not an escape") })
    }

    fn array(&mut self) -> Result<crate::json::Value, ParseError> {
        let opened = self.pos;
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                None => return Err(ParseError { at: opened, message: "unterminated array -- expected ']'".to_string() }),
                Some(']') => {
                    self.pos += 1;
                    return Ok(crate::json::Value::Array(items));
                }
                _ => {}
            }
            items.push(self.value()?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some(']') => {}
                None => return Err(ParseError { at: opened, message: "unterminated array -- expected ']'".to_string() }),
                Some(c) => return self.err(format!("expected ',' or ']' in an array, found '{c}'")),
            }
        }
    }

    fn inline_table(&mut self) -> Result<crate::json::Value, ParseError> {
        let opened = self.pos;
        self.pos += 1;
        let mut fields: Vec<(String, crate::json::Value)> = Vec::new();
        self.skip_blanks();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(crate::json::Value::Object(fields));
        }
        loop {
            self.skip_blanks();
            let (key, value) = self.pair()?;
            insert_nested(&mut fields, &key, value).map_err(|m| ParseError { at: opened, message: m })?;
            self.skip_blanks();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                    continue;
                }
                Some('}') => {
                    self.pos += 1;
                    return Ok(crate::json::Value::Object(fields));
                }
                // An inline table is a single-line construct in TOML
                // 1.0, and saying so is more useful than the "expected
                // ',' or '}'" a newline would otherwise produce.
                Some('\n') | None => return Err(ParseError { at: opened, message: "unterminated inline table -- it must fit on one line".to_string() }),
                Some(c) => return self.err(format!("expected ',' or '}}' in an inline table, found '{c}'")),
            }
        }
    }

    // --- building the document ---------------------------------------

    fn define(&mut self, path: Vec<String>) -> Result<(), ParseError> {
        if self.defined.contains(&path) {
            return self.err(format!("'{}' is defined twice", path.join(".")));
        }
        self.defined.push(path);
        Ok(())
    }

    fn set(&mut self, path: &[String], value: crate::json::Value) -> Result<(), String> {
        insert_nested(&mut self.root, path, value)
    }

    fn ensure_table(&mut self, path: &[String]) -> Result<(), String> {
        table_at(&mut self.root, path).map(|_| ())
    }

    fn push_array_table(&mut self, path: &[String]) -> Result<(), String> {
        let (last, parents) = path.split_last().ok_or("empty path")?;
        let parent = table_at(&mut self.root, parents)?;
        match parent.iter_mut().find(|(k, _)| k == last) {
            Some((_, crate::json::Value::Array(items))) => {
                items.push(crate::json::Value::Object(Vec::new()));
                Ok(())
            }
            Some((k, _)) => Err(format!("'{k}' is not an array of tables")),
            None => {
                parent.push((last.clone(), crate::json::Value::Array(vec![crate::json::Value::Object(Vec::new())])));
                Ok(())
            }
        }
    }
}

// The last path the parser recorded -- `run` writes each pair's value at
// the path it just defined, and this is that path.
fn full_path_of(defined: &[Vec<String>]) -> Vec<String> {
    defined.last().cloned().unwrap_or_default()
}

// Walks (creating as it goes) to the table `path` names, and hands back
// its field list. An array of tables resolves to its *last* element,
// which is what makes pairs after a `[[x]]` land in the one it opened.
fn table_at<'t>(root: &'t mut Vec<(String, crate::json::Value)>, path: &[String]) -> Result<&'t mut Vec<(String, crate::json::Value)>, String> {
    let mut cur = root;
    for segment in path {
        let idx = match cur.iter().position(|(k, _)| k == segment) {
            Some(i) => i,
            None => {
                cur.push((segment.clone(), crate::json::Value::Object(Vec::new())));
                cur.len() - 1
            }
        };
        cur = match &mut cur[idx].1 {
            crate::json::Value::Object(fields) => fields,
            crate::json::Value::Array(items) => match items.last_mut() {
                Some(crate::json::Value::Object(fields)) => fields,
                _ => return Err(format!("'{segment}' is not a table")),
            },
            _ => return Err(format!("'{segment}' is not a table")),
        };
    }
    Ok(cur)
}

fn insert_nested(root: &mut Vec<(String, crate::json::Value)>, path: &[String], value: crate::json::Value) -> Result<(), String> {
    let (last, parents) = path.split_last().ok_or("empty key")?;
    let table = table_at(root, parents)?;
    if table.iter().any(|(k, _)| k == last) {
        return Err(format!("'{last}' is defined twice"));
    }
    table.push((last.clone(), value));
    Ok(())
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

#[cfg(test)]
mod parse_tests {
    use super::*;
    use crate::json::Value;

    fn p(text: &str) -> Value {
        parse(text).unwrap_or_else(|e| panic!("{} at {}", e.message, e.at))
    }

    fn q(text: &str, path: &str) -> String {
        crate::json::query(&p(text), path).map(crate::json::compact_print).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_real_cargo_toml_reads_the_way_the_json_builtin_would() {
        let src = r#"
[package]
name = "bish"
version = "0.1.0"
edition = "2024"

[dependencies]

[[bin]]
name = "bish"
path = "src/main.rs"
"#;
        assert_eq!(q(src, ".package.name"), "\"bish\"");
        assert_eq!(q(src, ".package.edition"), "\"2024\"");
        assert_eq!(q(src, ".bin[0].path"), "\"src/main.rs\"");
    }

    #[test]
    fn every_scalar_form() {
        let src = concat!(
            "s = \"a\\tb\"\n",
            "l = 'c\\td'\n",
            "i = 1_000\n",
            "hex = 0xff\n",
            "oct = 0o17\n",
            "bin = 0b1010\n",
            "neg = -3\n",
            "f = 6.02e23\n",
            "inf = -inf\n",
            "yes = true\n",
            "no = false\n",
            "when = 1979-05-27T07:32:00Z\n",
            "day = 1979-05-27\n",
        );
        assert_eq!(q(src, ".s"), "\"a\\tb\"", "a basic string unescapes");
        assert_eq!(q(src, ".l"), "\"c\\\\td\"", "a literal string does not");
        assert_eq!(q(src, ".i"), "1000");
        assert_eq!(q(src, ".hex"), "255");
        assert_eq!(q(src, ".oct"), "15");
        assert_eq!(q(src, ".bin"), "10");
        assert_eq!(q(src, ".neg"), "-3");
        assert_eq!(q(src, ".yes"), "true");
        assert_eq!(q(src, ".no"), "false");
        // A date-time keeps its own text: there is no richer type here
        // to put it in.
        assert_eq!(q(src, ".when"), "\"1979-05-27T07:32:00Z\"");
        assert_eq!(q(src, ".day"), "\"1979-05-27\"");
    }

    #[test]
    fn multi_line_strings_and_the_line_ending_backslash() {
        let src = "a = \"\"\"\nline one\nline two\"\"\"\nb = '''\nraw \\n stays\n'''\nc = \"\"\"one \\\n   two\"\"\"\n";
        assert_eq!(q(src, ".a"), "\"line one\\nline two\"", "the newline after the opener is not part of it");
        assert_eq!(q(src, ".b"), "\"raw \\\\n stays\\n\"");
        assert_eq!(q(src, ".c"), "\"one two\"", "a trailing backslash eats the newline and the indent");
    }

    #[test]
    fn dotted_keys_arrays_and_inline_tables() {
        let src = concat!(
            "a.b.c = 1\n",
            "list = [1, 2, 3]\n",
            "nested = [[1, 2], [\"x\"]]\n",
            "spread = [\n  1,\n  2,\n]\n",
            "point = { x = 1, y = 2 }\n",
            "\"quoted key\" = 9\n",
        );
        assert_eq!(q(src, ".a.b.c"), "1");
        assert_eq!(q(src, ".list[2]"), "3");
        assert_eq!(q(src, ".nested[1][0]"), "\"x\"");
        assert_eq!(q(src, ".spread[1]"), "2", "an array may span lines");
        assert_eq!(q(src, ".point.y"), "2");
        assert_eq!(q(src, "[\"quoted key\"]"), "9");
    }

    #[test]
    fn array_of_tables_appends() {
        let src = "[[fruit]]\nname = \"apple\"\n\n[[fruit]]\nname = \"banana\"\n";
        assert_eq!(q(src, ".fruit[0].name"), "\"apple\"");
        assert_eq!(q(src, ".fruit[1].name"), "\"banana\"");
    }

    #[test]
    fn comments_and_blank_lines_are_not_content() {
        let src = "# leading\n\n  a = 1  # trailing\n\n# trailing comment\n";
        assert_eq!(q(src, ".a"), "1");
        assert_eq!(parse("").unwrap(), Value::Object(Vec::new()));
        assert_eq!(parse("# only a comment\n").unwrap(), Value::Object(Vec::new()));
    }

    // The half `:diag` exists for: every failure has to point somewhere
    // in particular, not just say that the file is broken.
    // Reading real files, not invented ones: a parser for a config
    // format is only worth having if it reads the configs that are
    // actually on the machine.
    #[test]
    fn every_toml_file_on_this_machine_parses() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
            if depth > 6 || out.len() > 400 {
                return;
            }
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out, depth + 1);
                } else if path.extension().is_some_and(|e| e == "toml") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        for root in ["/home/jussi/.cargo/registry/src", concat!(env!("CARGO_MANIFEST_DIR"))] {
            walk(std::path::Path::new(root), &mut files, 0);
        }
        // Nothing to prove on a machine with no TOML on it; the invented
        // cases above still cover the grammar.
        if files.len() < 5 {
            return;
        }
        let mut failures = Vec::new();
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else { continue };
            if let Err(e) = parse(&text) {
                let line = text.chars().take(e.at).filter(|c| *c == '\n').count() + 1;
                failures.push(format!("{}:{}: {}", path.display(), line, e.message));
            }
        }
        assert!(failures.is_empty(), "{} of {} real files failed:\n{}", failures.len(), files.len(), failures.join("\n"));
        assert!(files.len() >= 50, "only found {} files -- this test proves nothing at that size", files.len());
    }

    #[test]
    fn a_failure_names_a_position() {
        let at = |src: &str| parse(src).unwrap_err().at;
        let msg = |src: &str| parse(src).unwrap_err().message;

        assert_eq!(at("a = 1\nb = \n"), 10, "points past the `=` with nothing after it");
        assert!(msg("a = 1\nb = \n").contains("value"));

        // The opening quote, not wherever the scan gave up.
        assert_eq!(at("a = \"unterminated\n"), 4);
        assert!(msg("a = \"unterminated\n").contains("unterminated"));

        assert!(msg("a = 1 2\n").contains("one key/value pair per line"));
        assert!(msg("[a\n").contains("unterminated table header"));
        assert!(msg("a = [1, 2\n").contains("unterminated array"));
        assert!(msg("a = { x = 1\n").contains("unterminated inline table"));
        assert!(msg("a = tru\n").contains("true"));
        assert!(msg("a = 0xZZ\n").contains("base-16"));
        assert!(msg("a = \"\\q\"\n").contains("not an escape"));
        assert!(msg("a 1\n").contains("'='"));
    }

    #[test]
    fn a_thing_defined_twice_is_an_error() {
        assert!(parse("a = 1\na = 2\n").unwrap_err().message.contains("twice"));
        assert!(parse("[t]\n[t]\n").unwrap_err().message.contains("twice"));
        assert!(parse("x = { a = 1, a = 2 }\n").unwrap_err().message.contains("twice"));
        // ...but the same *name* under two different tables is fine.
        assert_eq!(q("[a]\nn = 1\n[b]\nn = 2\n", ".b.n"), "2");
    }
}
