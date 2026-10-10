// Token scanners for Rust, Python, JavaScript, TypeScript, YAML, CSS, HTML
// and Go.
//
// Token-level only: comments, strings, numbers and keywords. What a name
// means needs a language server, whose semantic tokens are layered last in
// `fileeditor::buffer_spans` and paint over these.
//
// One scanner per language rather than a shared table-driven one: the parts
// that go wrong are the parts that differ (Rust lifetimes, Python string
// prefixes, JavaScript template literals), and a flag per language would be
// a scanner per language with the structure hidden.
//
// Nothing here reports an error. A buffer being typed into is invalid most
// of the time, so an unterminated string runs as far as it can, as in
// `JsonHighlighter`.

use super::highlight::HighlightKind;
use std::ops::Range;

type Spans = Vec<(Range<usize>, HighlightKind)>;

// Byte offsets, as a `HighlightSpan` holds. A byte at or above 0x80 is part
// of a UTF-8 sequence, so it counts as an identifier character.
struct Scan<'a> {
    src: &'a [u8],
    pos: usize,
    out: Spans,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Scan<'a> {
        Scan { src: text.as_bytes(), pos: 0, out: Vec::new() }
    }

    fn at(&self, i: usize) -> u8 {
        self.src.get(i).copied().unwrap_or(0)
    }

    fn here(&self) -> u8 {
        self.at(self.pos)
    }

    fn done(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn looking_at(&self, s: &str) -> bool {
        self.src[self.pos.min(self.src.len())..].starts_with(s.as_bytes())
    }

    fn push(&mut self, start: usize, kind: HighlightKind) {
        if start < self.pos {
            self.out.push((start..self.pos, kind));
        }
    }

    /// To the end of the line, excluding the newline, which would otherwise
    /// colour the empty space to its right.
    fn skip_to_end_of_line(&mut self) {
        while !self.done() && self.here() != b'\n' {
            self.pos += 1;
        }
    }

    fn line_comment(&mut self, marker: &str) -> bool {
        if !self.looking_at(marker) {
            return false;
        }
        let start = self.pos;
        self.skip_to_end_of_line();
        self.push(start, HighlightKind::Comment);
        true
    }

    /// `nested` is Rust's rule: `/* /* */ */` is one comment there and
    /// two-thirds of one in C.
    fn block_comment(&mut self, open: &str, close: &str, nested: bool) -> bool {
        if !self.looking_at(open) {
            return false;
        }
        let start = self.pos;
        self.pos += open.len();
        let mut depth = 1usize;
        while !self.done() {
            if self.looking_at(close) {
                self.pos += close.len();
                depth -= 1;
                if depth == 0 {
                    break;
                }
            } else if nested && self.looking_at(open) {
                self.pos += open.len();
                depth += 1;
            } else {
                self.pos += 1;
            }
        }
        self.push(start, HighlightKind::Comment);
        true
    }

    /// A quoted run. `multiline` says whether a newline ends it: it does in
    /// JavaScript and Python's single-quoted forms, not in Rust.
    fn quoted(&mut self, quote: u8, multiline: bool) {
        let start = self.pos;
        self.pos += 1;
        while !self.done() {
            match self.here() {
                b'\\' => self.pos += 2,
                b'\n' if !multiline => break,
                c if c == quote => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += 1,
            }
        }
        self.push(start, HighlightKind::String);
    }

    /// A run delimited by a tripled quote: Python's `"""`.
    fn triple_quoted(&mut self, quote: u8) {
        let start = self.pos;
        let fence = [quote, quote, quote];
        let fence = std::str::from_utf8(&fence).expect("a quote is ASCII").to_string();
        self.pos += 3;
        while !self.done() {
            if self.looking_at(&fence) {
                self.pos += 3;
                break;
            }
            self.pos += if self.here() == b'\\' { 2 } else { 1 };
        }
        self.push(start, HighlightKind::String);
    }

    /// Digits plus a base prefix, `_` separators, an exponent, a decimal
    /// point and a type suffix (`1u8`, `1.5f64`, `10n`), so the suffix
    /// isn't scanned as an identifier.
    fn number(&mut self) {
        let start = self.pos;
        self.pos += 1;
        while !self.done() {
            let c = self.here();
            // An exponent's sign counts only right after the `e` of a
            // number that can have an exponent. `e` is a hex digit, so
            // `0x1E-3` is a subtraction and `1.2e-3` one number; a based
            // literal starts with `0`.
            if matches!(c, b'+' | b'-') && matches!(self.at(self.pos - 1), b'e' | b'E') && self.at(start) != b'0' {
                self.pos += 1;
                continue;
            }
            // A `.` is part of the number unless it's a method call:
            // `1.max(2)`.
            if c == b'.' && !self.at(self.pos + 1).is_ascii_digit() && self.at(self.pos + 1) != 0 && ident_char(self.at(self.pos + 1)) {
                break;
            }
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                self.pos += 1;
                continue;
            }
            break;
        }
        self.push(start, HighlightKind::Number);
    }

    /// The identifier at the cursor, as a byte range. The caller decides
    /// whether it is a keyword.
    fn word(&mut self) -> Range<usize> {
        let start = self.pos;
        while !self.done() && ident_char(self.here()) {
            self.pos += 1;
        }
        start..self.pos
    }

    fn text(&self, range: &Range<usize>) -> &str {
        std::str::from_utf8(&self.src[range.clone()]).unwrap_or("")
    }

    fn keyword_or_skip(&mut self, keywords: &[&str]) {
        let range = self.word();
        if keywords.contains(&self.text(&range)) {
            self.out.push((range, HighlightKind::Keyword));
        }
    }
}

fn ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

// Rust

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let",
    "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "union",
    "unsafe", "use", "where", "while", "yield",
];

pub fn rust(text: &str) -> Spans {
    let mut s = Scan::new(text);
    while !s.done() {
        if s.line_comment("//") || s.block_comment("/*", "*/", true) {
            continue;
        }
        let c = s.here();
        // A raw string: `r"..."`, or `r#"..."#` with any number of `#`.
        if (c == b'r' || c == b'b') && rust_raw_string(&mut s) {
            continue;
        }
        match c {
            b'"' => s.quoted(b'"', true),
            // `'` starts a character literal or a lifetime, which look
            // alike until the closing quote does or doesn't arrive. Scanned
            // as a string, `&'a str` would paint the rest of the file.
            b'\'' => {
                if rust_lifetime(&mut s) {
                    continue;
                }
                s.quoted(b'\'', false);
            }
            c if c.is_ascii_digit() => s.number(),
            c if ident_start(c) => s.keyword_or_skip(RUST_KEYWORDS),
            _ => s.pos += 1,
        }
    }
    s.out
}

/// `r"..."`, `r#"..."#`, `br"..."`, `br#"..."#`; true if one was consumed.
fn rust_raw_string(s: &mut Scan) -> bool {
    let start = s.pos;
    let mut at = s.pos;
    if s.at(at) == b'b' {
        at += 1;
    }
    if s.at(at) != b'r' {
        return false;
    }
    at += 1;
    let hashes = {
        let from = at;
        while s.at(at) == b'#' {
            at += 1;
        }
        at - from
    };
    if s.at(at) != b'"' {
        return false;
    }
    s.pos = at + 1;
    let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
    while !s.done() {
        if s.looking_at(&close) {
            s.pos += close.len();
            break;
        }
        s.pos += 1;
    }
    s.push(start, HighlightKind::String);
    true
}

/// `'a` with no closing quote is a lifetime, not a string. `'a'` is a
/// character. The difference is one byte past the name.
fn rust_lifetime(s: &mut Scan) -> bool {
    if !ident_start(s.at(s.pos + 1)) {
        return false;
    }
    let mut at = s.pos + 1;
    while ident_char(s.at(at)) {
        at += 1;
    }
    if s.at(at) == b'\'' {
        return false; // a character literal after all
    }
    let start = s.pos;
    s.pos = at;
    // `'static` is a lifetime that reads as a keyword.
    let kind = match s.text(&(start + 1..at)) {
        "static" => HighlightKind::Keyword,
        _ => HighlightKind::Variable,
    };
    s.push(start, kind);
    true
}

// Python

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// The letters that may precede a quote (raw, bytes, formatted, unicode),
/// in either case and order.
fn python_string_prefix(word: &str) -> bool {
    !word.is_empty() && word.len() <= 2 && word.bytes().all(|c| matches!(c.to_ascii_lowercase(), b'r' | b'b' | b'f' | b'u'))
}

pub fn python(text: &str) -> Spans {
    let mut s = Scan::new(text);
    while !s.done() {
        if s.line_comment("#") {
            continue;
        }
        let c = s.here();
        match c {
            b'"' | b'\'' => python_string(&mut s),
            // A decorator reads as part of the declaration.
            b'@' if ident_start(s.at(s.pos + 1)) => {
                let start = s.pos;
                s.pos += 1;
                while !s.done() && (ident_char(s.here()) || s.here() == b'.') {
                    s.pos += 1;
                }
                s.push(start, HighlightKind::Keyword);
            }
            c if c.is_ascii_digit() => s.number(),
            c if ident_start(c) => {
                let range = s.word();
                let word = s.text(&range).to_string();
                // `f"..."` is a prefix and a string; `f` alone is a name.
                if python_string_prefix(&word) && matches!(s.here(), b'"' | b'\'') {
                    let start = range.start;
                    python_string(&mut s);
                    // Extend the string's span over its prefix.
                    if let Some(last) = s.out.last_mut() {
                        last.0.start = start;
                    }
                    continue;
                }
                if PYTHON_KEYWORDS.contains(&word.as_str()) {
                    s.out.push((range, HighlightKind::Keyword));
                }
            }
            _ => s.pos += 1,
        }
    }
    s.out
}

fn python_string(s: &mut Scan) {
    let quote = s.here();
    match s.at(s.pos + 1) == quote && s.at(s.pos + 2) == quote {
        true => s.triple_quoted(quote),
        false => s.quoted(quote, false),
    }
}

// JavaScript and TypeScript

const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// TypeScript is JavaScript's scanner with the type keywords added.
const TS_EXTRA_KEYWORDS: &[&str] = &[
    "abstract",
    "any",
    "as",
    "asserts",
    "bigint",
    "boolean",
    "declare",
    "enum",
    "implements",
    "infer",
    "interface",
    "is",
    "keyof",
    "namespace",
    "never",
    "number",
    "object",
    "override",
    "private",
    "protected",
    "public",
    "readonly",
    "satisfies",
    "string",
    "symbol",
    "type",
    "unique",
    "unknown",
];

pub fn javascript(text: &str) -> Spans {
    js_like(text, JS_KEYWORDS)
}

pub fn typescript(text: &str) -> Spans {
    let mut keywords: Vec<&str> = JS_KEYWORDS.to_vec();
    keywords.extend_from_slice(TS_EXTRA_KEYWORDS);
    js_like(text, &keywords)
}

fn js_like(text: &str, keywords: &[&str]) -> Spans {
    let mut s = Scan::new(text);
    while !s.done() {
        if s.line_comment("//") || s.block_comment("/*", "*/", false) {
            continue;
        }
        match s.here() {
            b'"' => s.quoted(b'"', false),
            b'\'' => s.quoted(b'\'', false),
            b'`' => template(&mut s),
            c if c.is_ascii_digit() => s.number(),
            c if ident_start(c) => s.keyword_or_skip(keywords),
            _ => s.pos += 1,
        }
    }
    s.out
}

/// A template literal is a string with holes. The string is painted first
/// and each `${...}` over it, as a JSON string's escapes are, so this
/// needn't lex JavaScript recursively.
fn template(s: &mut Scan) {
    let start = s.pos;
    s.pos += 1;
    let mut holes: Vec<Range<usize>> = Vec::new();
    while !s.done() {
        match s.here() {
            b'\\' => s.pos += 2,
            b'`' => {
                s.pos += 1;
                break;
            }
            b'$' if s.at(s.pos + 1) == b'{' => {
                let hole = s.pos;
                s.pos += 2;
                let mut depth = 1usize;
                while !s.done() && depth > 0 {
                    match s.here() {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    s.pos += 1;
                }
                holes.push(hole..s.pos);
            }
            _ => s.pos += 1,
        }
    }
    s.push(start, HighlightKind::String);
    for hole in holes {
        s.out.push((hole, HighlightKind::FormatSpecifier));
    }
}

// YAML
//
// Line-oriented, because YAML is: a line's meaning comes from its
// indentation and first character, except in a block scalar, whose body is
// everything indented past the line that opened it. Flow forms (`{a: 1}`)
// are read like block forms; at token level a key is a word before a colon
// either way.

/// Case-insensitive because YAML 1.1 was, and files still use `True` and
/// `NO`.
const YAML_CONSTANTS: &[&str] = &["true", "false", "yes", "no", "on", "off", "null", "~"];

pub fn yaml(text: &str) -> Spans {
    let mut out: Spans = Vec::new();
    let mut at = 0usize;
    // Set by `|` or `>` at the end of a line: everything indented further
    // is the scalar's text, so a `#` in an embedded shell script isn't a
    // YAML comment.
    let mut block: Option<usize> = None;
    for line in text.split_inclusive('\n') {
        let end = at + line.trim_end_matches('\n').len();
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        if let Some(opened_at) = block {
            if trimmed.is_empty() || indent > opened_at {
                // One span per line, so a line-at-a-time renderer sees it.
                if !trimmed.is_empty() {
                    out.push((at + indent..end, HighlightKind::String));
                }
                at += line.len();
                continue;
            }
            block = None;
        }

        if trimmed.is_empty() {
            at += line.len();
            continue;
        }
        if trimmed == "---" || trimmed == "..." {
            out.push((at + indent..end, HighlightKind::Keyword));
            at += line.len();
            continue;
        }
        yaml_line(text, at + indent, end, &mut out, &mut block, indent);
        at += line.len();
    }
    out
}

fn yaml_line(text: &str, start: usize, end: usize, out: &mut Spans, block: &mut Option<usize>, indent: usize) {
    let bytes = text.as_bytes();
    let mut i = start;

    // A sequence dash is punctuation, and what follows starts afresh.
    while i < end && bytes[i] == b'-' && (i + 1 >= end || bytes[i + 1] == b' ') {
        out.push((i..i + 1, HighlightKind::Operator));
        i += 2;
        i = i.min(end);
        while i < end && bytes[i] == b' ' {
            i += 1;
        }
    }

    // A comment only starts a token, which after the dashes means here.
    if i < end && bytes[i] == b'#' {
        out.push((i..end, HighlightKind::Comment));
        return;
    }

    // The key: everything up to a `:` followed by a space or the line end,
    // skipping colons inside quotes.
    if let Some(colon) = yaml_key_end(bytes, i, end) {
        out.push((i..colon, HighlightKind::Key));
        out.push((colon..colon + 1, HighlightKind::Operator));
        i = (colon + 1).min(end);
        while i < end && bytes[i] == b' ' {
            i += 1;
        }
    }

    if i >= end {
        return;
    }
    match bytes[i] {
        b'#' => out.push((i..end, HighlightKind::Comment)),
        b'|' | b'>' => {
            out.push((i..end, HighlightKind::Operator));
            *block = Some(indent);
        }
        b'&' | b'*' | b'!' => {
            let mut j = i + 1;
            while j < end && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            out.push((i..j, HighlightKind::Variable));
            yaml_value(text, j, end, out);
        }
        _ => yaml_value(text, i, end, out),
    }
}

/// Where this line's key ends; `None` for a bare value.
fn yaml_key_end(bytes: &[u8], from: usize, end: usize) -> Option<usize> {
    let mut i = from;
    let mut quote = None;
    while i < end {
        match bytes[i] {
            q @ (b'"' | b'\'') if quote.is_none() => quote = Some(q),
            q if Some(q) == quote => quote = None,
            b':' if quote.is_none() && (i + 1 >= end || bytes[i + 1] == b' ') => return Some(i),
            b'#' if quote.is_none() && i > from && bytes[i - 1] == b' ' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// The rest of a line after its key: a scalar, and maybe a comment.
fn yaml_value(text: &str, from: usize, end: usize, out: &mut Spans) {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < end {
        match bytes[i] {
            b'#' if i > from && bytes[i - 1] == b' ' => {
                out.push((i..end, HighlightKind::Comment));
                return;
            }
            q @ (b'"' | b'\'') => {
                let start = i;
                i += 1;
                while i < end {
                    if bytes[i] == b'\\' && q == b'"' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == q {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push((start..i.min(end), HighlightKind::String));
            }
            c if c.is_ascii_digit() || (c == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) => {
                let start = i;
                i += 1;
                while i < end && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_' | b'-' | b'+')) {
                    i += 1;
                }
                let word = &text[start..i];
                // Dates and versions (`1.2.3`) are plain scalars, not
                // numbers.
                if word.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E' | 'x' | 'b' | 'o'))
                    && word.matches('.').count() <= 1
                    && word.matches('-').count() <= 1
                {
                    out.push((start..i, HighlightKind::Number));
                }
            }
            c if c.is_ascii_alphabetic() || c == b'~' => {
                let start = i;
                while i < end && !bytes[i].is_ascii_whitespace() && bytes[i] != b',' && bytes[i] != b'}' && bytes[i] != b']' {
                    i += 1;
                }
                if YAML_CONSTANTS.contains(&text[start..i].to_ascii_lowercase().as_str()) {
                    out.push((start..i, HighlightKind::Keyword));
                }
            }
            _ => i += 1,
        }
        if bytes.get(i).is_some_and(|c| c.is_ascii_whitespace() || *c == b',') {
            i += 1;
        } else if i <= from {
            i = from + 1;
        }
    }
}

// CSS
//
// Whether a token is a selector, property or value depends on the braces:
// outside a block it's a selector, inside a property up to the colon and a
// value after. `#fff` is an id in one place and a colour in the other.

pub fn css(text: &str) -> Spans {
    let mut s = Scan::new(text);
    let mut depth = 0usize;
    // Past the `:` in the current declaration, i.e. in the value.
    let mut in_value = false;
    let mut selector_start = None;
    while !s.done() {
        if s.block_comment("/*", "*/", false) {
            continue;
        }
        let c = s.here();
        // Outside a block everything up to the `{` is one selector,
        // punctuation and all. Started here so a selector can't start on a
        // space or carry over the previous at-rule's `;`.
        if depth == 0 && !c.is_ascii_whitespace() && !matches!(c, b'@' | b';') && selector_start.is_none() {
            selector_start = Some(s.pos);
        }
        match c {
            b'"' => s.quoted(b'"', false),
            b'\'' => s.quoted(b'\'', false),
            b'{' => {
                if let Some(start) = selector_start.take() {
                    css_selector(&mut s, start);
                }
                s.out.push((s.pos..s.pos + 1, HighlightKind::Operator));
                s.pos += 1;
                depth += 1;
                in_value = false;
            }
            b'}' => {
                s.out.push((s.pos..s.pos + 1, HighlightKind::Operator));
                s.pos += 1;
                depth = depth.saturating_sub(1);
                in_value = false;
                selector_start = None;
            }
            b':' if depth > 0 => {
                s.out.push((s.pos..s.pos + 1, HighlightKind::Operator));
                s.pos += 1;
                in_value = true;
            }
            b';' => {
                s.out.push((s.pos..s.pos + 1, HighlightKind::Operator));
                s.pos += 1;
                in_value = false;
                selector_start = None;
            }
            b'@' if ident_start(s.at(s.pos + 1)) => {
                let start = s.pos;
                s.pos += 1;
                css_ident(&mut s);
                // At top level this names a rule; inside a value it names a
                // colour, GTK's extension.
                let kind = match in_value {
                    true => HighlightKind::Variable,
                    false => HighlightKind::Keyword,
                };
                s.push(start, kind);
            }
            b'!' if depth > 0 => {
                let start = s.pos;
                s.pos += 1;
                css_ident(&mut s);
                s.push(start, HighlightKind::Keyword);
            }
            // A custom property, and the `var(--x)` that reads it.
            b'-' if s.at(s.pos + 1) == b'-' && ident_start(s.at(s.pos + 2)) => {
                let start = s.pos;
                s.pos += 2;
                css_ident(&mut s);
                s.push(start, HighlightKind::Variable);
            }
            b'#' if depth > 0 && in_value => {
                let start = s.pos;
                s.pos += 1;
                css_ident(&mut s);
                s.push(start, HighlightKind::Number);
            }
            c if c.is_ascii_digit() || (c == b'.' && s.at(s.pos + 1).is_ascii_digit()) => {
                let start = s.pos;
                s.number();
                // The unit is part of the number: `10px`.
                if !s.done() && (ident_start(s.here()) || s.here() == b'%') {
                    if s.here() == b'%' {
                        s.pos += 1;
                    } else {
                        css_ident(&mut s);
                    }
                    s.out.pop();
                    s.push(start, HighlightKind::Number);
                }
            }
            c if ident_start(c) => {
                let start = s.pos;
                css_ident(&mut s);
                if depth > 0 && !in_value {
                    s.push(start, HighlightKind::Key);
                }
            }
            _ => s.pos += 1,
        }
    }
    s.out
}

/// A CSS identifier, hyphens included: `font-family`.
fn css_ident(s: &mut Scan) {
    while !s.done() && (ident_char(s.here()) || s.here() == b'-') {
        s.pos += 1;
    }
}

/// The whole selector as one span, as `IniHighlighter` does for a section
/// header.
fn css_selector(s: &mut Scan, start: usize) {
    let mut end = s.pos;
    while end > start && s.at(end - 1).is_ascii_whitespace() {
        end -= 1;
    }
    if end > start {
        s.out.push((start..end, HighlightKind::Keyword));
    }
}

// HTML
//
// Not html/tokenizer.rs: that emits tokens without source offsets, and
// adding them would change the browser's code path to colour text.

pub fn html(text: &str) -> Spans {
    let mut s = Scan::new(text);
    while !s.done() {
        if s.looking_at("<!--") {
            let start = s.pos;
            s.pos += 4;
            while !s.done() && !s.looking_at("-->") {
                s.pos += 1;
            }
            s.pos = (s.pos + 3).min(s.src.len());
            s.push(start, HighlightKind::Comment);
            continue;
        }
        if s.looking_at("<!") {
            let start = s.pos;
            while !s.done() && s.here() != b'>' {
                s.pos += 1;
            }
            s.pos = (s.pos + 1).min(s.src.len());
            s.push(start, HighlightKind::Keyword);
            continue;
        }
        if s.here() == b'<' && (ident_start(s.at(s.pos + 1)) || (s.at(s.pos + 1) == b'/' && ident_start(s.at(s.pos + 2)))) {
            html_tag(&mut s);
            continue;
        }
        // A character reference, coloured like a string escape.
        if s.here() == b'&' {
            let start = s.pos;
            let mut at = s.pos + 1;
            if s.at(at) == b'#' {
                at += 1;
            }
            while s.at(at).is_ascii_alphanumeric() {
                at += 1;
            }
            if s.at(at) == b';' && at > s.pos + 1 {
                s.pos = at + 1;
                s.push(start, HighlightKind::FormatSpecifier);
                continue;
            }
        }
        s.pos += 1;
    }
    s.out
}

/// One tag, `<` to `>`. For `<script>` and `<style>`, also their contents,
/// with the scanner for the language inside.
fn html_tag(s: &mut Scan) {
    let open = s.pos;
    s.pos += 1;
    if s.here() == b'/' {
        s.pos += 1;
    }
    let name = s.word();
    let name_text = s.text(&name).to_ascii_lowercase();
    s.out.push((open..s.pos, HighlightKind::Keyword));
    let closing = s.at(open + 1) == b'/';

    while !s.done() && s.here() != b'>' {
        match s.here() {
            b'"' => s.quoted(b'"', true),
            b'\'' => s.quoted(b'\'', true),
            b'=' => {
                s.out.push((s.pos..s.pos + 1, HighlightKind::Operator));
                s.pos += 1;
            }
            c if ident_start(c) || c == b'-' || c == b':' || c == b'@' => {
                let start = s.pos;
                while !s.done() && (ident_char(s.here()) || matches!(s.here(), b'-' | b':' | b'@' | b'.')) {
                    s.pos += 1;
                }
                s.push(start, HighlightKind::Key);
            }
            c if !c.is_ascii_whitespace() && c != b'/' => {
                let start = s.pos;
                while !s.done() && !s.here().is_ascii_whitespace() && s.here() != b'>' {
                    s.pos += 1;
                }
                s.push(start, HighlightKind::String);
            }
            _ => s.pos += 1,
        }
    }
    let tag_end = (s.pos + 1).min(s.src.len());
    s.out.push((s.pos.min(tag_end)..tag_end, HighlightKind::Keyword));
    s.pos = tag_end;

    // The raw text runs to the matching end tag and none of it is markup,
    // so it can be handed to the other scanner.
    if closing {
        return;
    }
    let scan: fn(&str) -> Spans = match name_text.as_str() {
        "script" => javascript,
        "style" => css,
        _ => return,
    };
    let body_start = s.pos;
    let close_tag = format!("</{name_text}");
    while !s.done() {
        if s.src[s.pos..].len() >= close_tag.len() && s.src[s.pos..s.pos + close_tag.len()].eq_ignore_ascii_case(close_tag.as_bytes()) {
            break;
        }
        s.pos += 1;
    }
    let body = std::str::from_utf8(&s.src[body_start..s.pos]).unwrap_or("");
    for (range, kind) in scan(body) {
        s.out.push((body_start + range.start..body_start + range.end, kind));
    }
}

// Go
//
// The one thing it has that the others don't is the backtick raw string,
// which spans lines and has no escapes. Read as an ordinary string, the
// first regexp literal paints the rest of the file.

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

/// Predeclared identifiers, not keywords (any can be shadowed), but they
/// read as part of the language.
const GO_PREDECLARED: &[&str] = &[
    "any",
    "append",
    "bool",
    "byte",
    "cap",
    "clear",
    "close",
    "comparable",
    "complex",
    "complex64",
    "complex128",
    "copy",
    "delete",
    "error",
    "false",
    "float32",
    "float64",
    "imag",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "iota",
    "len",
    "make",
    "max",
    "min",
    "new",
    "nil",
    "panic",
    "print",
    "println",
    "real",
    "recover",
    "rune",
    "string",
    "true",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
];

pub fn go(text: &str) -> Spans {
    let mut s = Scan::new(text);
    while !s.done() {
        if s.line_comment("//") || s.block_comment("/*", "*/", false) {
            continue;
        }
        match s.here() {
            b'"' => s.quoted(b'"', false),
            // A rune literal; Go has no lifetimes.
            b'\'' => s.quoted(b'\'', false),
            b'`' => go_raw_string(&mut s),
            c if c.is_ascii_digit() || (c == b'.' && s.at(s.pos + 1).is_ascii_digit()) => s.number(),
            c if ident_start(c) => {
                let range = s.word();
                let word = s.text(&range);
                if GO_KEYWORDS.contains(&word) || GO_PREDECLARED.contains(&word) {
                    s.out.push((range, HighlightKind::Keyword));
                }
            }
            _ => s.pos += 1,
        }
    }
    s.out
}

/// No escapes, so a backslash can't hide the terminator.
fn go_raw_string(s: &mut Scan) {
    let start = s.pos;
    s.pos += 1;
    while !s.done() && s.here() != b'`' {
        s.pos += 1;
    }
    s.pos = (s.pos + 1).min(s.src.len());
    s.push(start, HighlightKind::String);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each span's text with its kind, easier to read in a failure than byte
    // offsets.
    fn painted(text: &str, spans: Spans) -> Vec<(String, HighlightKind)> {
        spans.into_iter().map(|(r, k)| (text[r].to_string(), k)).collect()
    }

    #[test]
    fn rust_comments_strings_numbers_and_keywords() {
        let text = "// note\nlet x = \"hi\"; /* a /* b */ */ 0x1F_u8;";
        assert_eq!(
            painted(text, rust(text)),
            vec![
                ("// note".into(), HighlightKind::Comment),
                ("let".into(), HighlightKind::Keyword),
                ("\"hi\"".into(), HighlightKind::String),
                ("/* a /* b */ */".into(), HighlightKind::Comment),
                ("0x1F_u8".into(), HighlightKind::Number),
            ]
        );
    }

    // A lifetime isn't an unterminated character literal.
    #[test]
    fn a_rust_lifetime_is_not_a_string() {
        let text = "fn f<'a>(s: &'a str) -> char { 'x' }";
        let spans = painted(text, rust(text));
        assert!(spans.contains(&("'a".to_string(), HighlightKind::Variable)), "{spans:?}");
        assert!(spans.contains(&("'x'".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(!spans.iter().any(|(t, k)| *k == HighlightKind::String && t.contains("str")), "{spans:?}");
    }

    #[test]
    fn a_rust_raw_string_holds_its_own_quotes() {
        let text = r####"let re = r#"a "quoted" thing"#; let n = 1;"####;
        let spans = painted(text, rust(text));
        assert!(spans.contains(&(r##"r#"a "quoted" thing"#"##.to_string(), HighlightKind::String)), "{spans:?}");
        assert!(spans.contains(&("1".to_string(), HighlightKind::Number)), "the scanner came back out of the string: {spans:?}");
    }

    #[test]
    fn static_is_the_lifetime_that_is_also_a_keyword() {
        let text = "&'static str";
        assert_eq!(painted(text, rust(text)), vec![("'static".into(), HighlightKind::Keyword)]);
    }

    #[test]
    fn python_prefixes_belong_to_the_string_they_are_stuck_to() {
        let text = "x = f\"a {b}\"\ny = rb'\\d'\nz = f\n";
        let spans = painted(text, python(text));
        assert!(spans.contains(&("f\"a {b}\"".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(spans.contains(&("rb'\\d'".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(!spans.iter().any(|(t, _)| t == "f"), "a lone `f` is a name: {spans:?}");
    }

    #[test]
    fn a_python_docstring_runs_to_its_closing_fence() {
        let text = "def f():\n    \"\"\"One.\n\n    And two.\n    \"\"\"\n    return 1\n";
        let spans = painted(text, python(text));
        assert!(spans.iter().any(|(t, k)| *k == HighlightKind::String && t.contains("And two")), "{spans:?}");
        assert!(spans.contains(&("return".to_string(), HighlightKind::Keyword)), "the fence closed: {spans:?}");
    }

    #[test]
    fn a_python_decorator_reads_as_part_of_the_declaration() {
        let text = "@functools.cache\ndef f(): pass";
        let spans = painted(text, python(text));
        assert!(spans.contains(&("@functools.cache".to_string(), HighlightKind::Keyword)), "{spans:?}");
    }

    #[test]
    fn a_template_literal_keeps_its_holes_separate() {
        let text = "const a = `x ${y + 1} z`;";
        let spans = painted(text, javascript(text));
        assert!(spans.contains(&("`x ${y + 1} z`".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(spans.contains(&("${y + 1}".to_string(), HighlightKind::FormatSpecifier)), "{spans:?}");
    }

    #[test]
    fn typescript_knows_the_words_that_describe_types() {
        let text = "interface A { b: string }";
        let js = painted(text, javascript(text));
        let ts = painted(text, typescript(text));
        assert!(!js.iter().any(|(t, _)| t == "interface"), "javascript has no such word: {js:?}");
        assert!(ts.contains(&("interface".to_string(), HighlightKind::Keyword)), "{ts:?}");
        assert!(ts.contains(&("string".to_string(), HighlightKind::Keyword)), "{ts:?}");
    }

    // An unterminated string takes what it can reach; no loop, no panic.
    #[test]
    fn an_unterminated_construct_ends_with_the_text() {
        for (text, spans) in [
            ("let s = \"open", rust("let s = \"open")),
            ("/* open", rust("/* open")),
            ("x = '''open", python("x = '''open")),
            ("const a = `open ${b", javascript("const a = `open ${b")),
        ] {
            assert!(spans.iter().all(|(r, _)| r.end <= text.len()), "a span ran past the text: {text:?}");
        }
    }

    // This module's own source, which has nested block comments, raw
    // strings containing quotes, lifetimes and character literals. Every
    // span has to be a well-formed range; a scanner that loses its place
    // makes one that isn't.
    #[test]
    fn the_rust_scanner_survives_this_projects_own_source() {
        let text = include_str!("codehighlight.rs");
        let spans = rust(text);
        assert!(spans.len() > 200, "a file this size has more tokens than that: {}", spans.len());
        for (range, _) in &spans {
            assert!(range.start < range.end && range.end <= text.len(), "malformed span {range:?}");
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end), "span {range:?} splits a character");
        }
        // Not one string for the whole file, as a scanner stuck in a raw
        // string would say.
        let longest = spans.iter().map(|(r, _)| r.end - r.start).max().unwrap_or(0);
        assert!(longest < text.len() / 4, "one span covers {longest} of {} bytes", text.len());
    }

    #[test]
    fn yaml_keys_values_and_comments() {
        let text = "# top\nname: bish   # trailing\ncount: 3\nok: true\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("# top".to_string(), HighlightKind::Comment)), "{spans:?}");
        assert!(spans.contains(&("name".to_string(), HighlightKind::Key)), "{spans:?}");
        assert!(spans.contains(&("# trailing".to_string(), HighlightKind::Comment)), "{spans:?}");
        assert!(spans.contains(&("3".to_string(), HighlightKind::Number)), "{spans:?}");
        assert!(spans.contains(&("true".to_string(), HighlightKind::Keyword)), "{spans:?}");
    }

    // A block scalar's body is text: a `#` in it is the script's comment.
    #[test]
    fn a_block_scalar_is_text_and_not_yaml() {
        let text = "run: |\n  # not a yaml comment\n  echo hi: there\nnext: 1\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("# not a yaml comment".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(!spans.iter().any(|(t, k)| *k == HighlightKind::Key && t == "echo hi"), "{spans:?}");
        assert!(spans.contains(&("next".to_string(), HighlightKind::Key)), "the block ended with the indentation: {spans:?}");
    }

    #[test]
    fn a_yaml_sequence_dash_is_not_part_of_the_key() {
        let text = "steps:\n  - uses: actions/checkout\n  - run: make\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("uses".to_string(), HighlightKind::Key)), "{spans:?}");
        assert!(spans.contains(&("run".to_string(), HighlightKind::Key)), "{spans:?}");
    }

    #[test]
    fn a_colon_inside_a_quoted_yaml_string_is_not_a_key() {
        let text = "a: \"x: y\"\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("a".to_string(), HighlightKind::Key)), "{spans:?}");
        assert!(spans.contains(&("\"x: y\"".to_string(), HighlightKind::String)), "{spans:?}");
    }

    #[test]
    fn yaml_anchors_and_document_markers() {
        let text = "---\nbase: &defaults\n  a: 1\nuse: *defaults\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("---".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("&defaults".to_string(), HighlightKind::Variable)), "{spans:?}");
        assert!(spans.contains(&("*defaults".to_string(), HighlightKind::Variable)), "{spans:?}");
    }

    #[test]
    fn a_version_is_not_a_number() {
        let text = "version: 1.2.3\nwhen: 2026-09-07\nport: 8080\n";
        let spans = painted(text, yaml(text));
        assert!(spans.contains(&("8080".to_string(), HighlightKind::Number)), "{spans:?}");
        assert!(!spans.iter().any(|(t, k)| *k == HighlightKind::Number && (t == "1.2.3" || t.contains('-'))), "{spans:?}");
    }

    #[test]
    fn css_tells_a_selector_from_a_property_from_a_value() {
        let text = ".card > p { color: #fff; margin: 10px 0; }";
        let spans = painted(text, css(text));
        assert!(spans.contains(&(".card > p".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("color".to_string(), HighlightKind::Key)), "{spans:?}");
        assert!(spans.contains(&("#fff".to_string(), HighlightKind::Number)), "{spans:?}");
        assert!(spans.contains(&("10px".to_string(), HighlightKind::Number)), "a unit belongs to its measurement: {spans:?}");
    }

    #[test]
    fn a_hash_means_different_things_on_either_side_of_a_brace() {
        let text = "#main { color: #abc; }";
        let spans = painted(text, css(text));
        assert!(spans.contains(&("#main".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("#abc".to_string(), HighlightKind::Number)), "{spans:?}");
    }

    #[test]
    fn css_custom_properties_and_at_rules() {
        let text = "@media print { :root { --gap: 4px; padding: var(--gap) !important; } }";
        let spans = painted(text, css(text));
        assert!(spans.contains(&("@media".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("--gap".to_string(), HighlightKind::Variable)), "{spans:?}");
        assert!(spans.contains(&("!important".to_string(), HighlightKind::Keyword)), "{spans:?}");
    }

    #[test]
    fn html_tags_attributes_and_entities() {
        let text = "<!-- hi -->\n<a href=\"/x\" data-n=3>&amp;</a>";
        let spans = painted(text, html(text));
        assert!(spans.contains(&("<!-- hi -->".to_string(), HighlightKind::Comment)), "{spans:?}");
        assert!(spans.contains(&("<a".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("href".to_string(), HighlightKind::Key)), "{spans:?}");
        assert!(spans.contains(&("\"/x\"".to_string(), HighlightKind::String)), "{spans:?}");
        assert!(spans.contains(&("3".to_string(), HighlightKind::String)), "an unquoted value is still a value: {spans:?}");
        assert!(spans.contains(&("&amp;".to_string(), HighlightKind::FormatSpecifier)), "{spans:?}");
        assert!(spans.contains(&("</a".to_string(), HighlightKind::Keyword)), "{spans:?}");
    }

    #[test]
    fn script_and_style_are_scanned_as_what_they_hold() {
        let text = "<style>p { color: red }</style><script>let x = 1; // n</script>";
        let spans = painted(text, html(text));
        assert!(spans.contains(&("color".to_string(), HighlightKind::Key)), "the css scanner ran: {spans:?}");
        assert!(spans.contains(&("let".to_string(), HighlightKind::Keyword)), "the js scanner ran: {spans:?}");
        assert!(spans.contains(&("// n".to_string(), HighlightKind::Comment)), "{spans:?}");
        assert!(spans.contains(&("</script".to_string(), HighlightKind::Keyword)), "and it stopped at the end tag: {spans:?}");
    }

    #[test]
    fn a_doctype_is_not_a_tag() {
        let text = "<!DOCTYPE html>\n<html>";
        let spans = painted(text, html(text));
        assert!(spans.contains(&("<!DOCTYPE html>".to_string(), HighlightKind::Keyword)), "{spans:?}");
    }

    #[test]
    fn go_comments_strings_numbers_and_keywords() {
        let text = "// note\npackage main\n\nfunc f() int { return 0x1F_u }";
        let spans = painted(text, go(text));
        assert!(spans.contains(&("// note".to_string(), HighlightKind::Comment)), "{spans:?}");
        assert!(spans.contains(&("package".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("func".to_string(), HighlightKind::Keyword)), "{spans:?}");
        assert!(spans.contains(&("int".to_string(), HighlightKind::Keyword)), "a predeclared type reads as part of the language: {spans:?}");
    }

    #[test]
    fn a_go_raw_string_has_no_escapes_and_spans_lines() {
        let text = "re := `a\\d+\n\"quoted\"`\nn := 1";
        let spans = painted(text, go(text));
        assert!(
            spans.iter().any(|(t, k)| *k == HighlightKind::String && t.contains("quoted") && t.contains('\n')),
            "the whole raw string is one span: {spans:?}"
        );
        assert!(spans.contains(&("1".to_string(), HighlightKind::Number)), "the scanner came back out of it: {spans:?}");
    }

    #[test]
    fn a_backslash_does_not_end_a_go_raw_string_early() {
        let text = "s := `ends with a backslash \\` + x";
        let spans = painted(text, go(text));
        let strings: Vec<&String> = spans.iter().filter(|(_, k)| *k == HighlightKind::String).map(|(t, _)| t).collect();
        assert_eq!(strings.len(), 1, "{spans:?}");
        assert!(strings[0].ends_with('`'), "it ended at the backtick, not before it: {strings:?}");
    }

    #[test]
    fn a_go_rune_is_just_a_character() {
        let text = "c := 'x'; d := '\\n'";
        let spans = painted(text, go(text));
        assert!(spans.contains(&("'x'".to_string(), HighlightKind::String)), "{spans:?}");
    }

    #[test]
    fn go_predeclared_names_are_coloured_even_though_they_are_not_keywords() {
        let text = "var s string = \"x\"\nb := make([]byte, len(s))\nif b == nil { panic(true) }";
        let spans = painted(text, go(text));
        for word in ["string", "make", "byte", "len", "nil", "panic", "true"] {
            assert!(spans.contains(&(word.to_string(), HighlightKind::Keyword)), "{word} is missing: {spans:?}");
        }
        assert!(!spans.iter().any(|(t, _)| t == "s" || t == "b"), "an ordinary name gets nothing: {spans:?}");
    }

    // Every scanner makes progress on every byte, or a stray character
    // hangs the editor.
    #[test]
    fn every_scanner_terminates_on_arbitrary_bytes() {
        let text = "a\u{e9}'\"`#@${}/*//\\0 1.2e-3 0x_ r#\" \u{1f600}";
        for scan in [rust, python, javascript, typescript, yaml, css, html, go] {
            let spans = scan(text);
            assert!(spans.iter().all(|(r, _)| r.start <= r.end && r.end <= text.len()));
        }
    }
}
