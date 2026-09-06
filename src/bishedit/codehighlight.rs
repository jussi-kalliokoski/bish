// Token scanners for the languages the editor already recognises by
// extension but had nothing to colour them with: rust, python,
// javascript and typescript. Opening a `.rs` file here showed plain
// uncoloured text unless a language server happened to be running and
// painting semantic tokens over the top.
//
// These are deliberately *token*-level and nothing more. What a name
// means -- a type, a function, a module -- is a question that needs the
// program understood, and there is already something in this editor
// that answers it properly: a language server's semantic tokens, which
// are layered last in `fileeditor::buffer_spans` and paint over
// whatever these produce. So the job here is only to get comments,
// strings, numbers and keywords right, which is what makes a file
// readable when no server is running, and to stay out of the way when
// one is.
//
// Each language gets its own scanner rather than a shared one driven by
// a table of rules. Their comment and number syntax really is the same,
// but the parts that go wrong are exactly the parts that differ: Rust's
// lifetimes look like unterminated character literals, Python's string
// prefixes attach to the quote, JavaScript's template literals nest
// arbitrary code inside a string. A shared engine would have to grow a
// flag for each of those, and a flag per language is a scanner per
// language with the structure hidden.
//
// Nothing here reports an error. A buffer being typed into is invalid
// most of the time, and an unterminated string simply runs to the end
// of what it can reach -- text going uncoloured past a mistake is a
// quieter and more honest signal than painting an error the moment a
// quote is opened. Same call `JsonHighlighter` already makes.

use super::highlight::HighlightKind;
use std::ops::Range;

type Spans = Vec<(Range<usize>, HighlightKind)>;

// Byte offsets throughout, which is what a `HighlightSpan` holds. A
// byte at or above 0x80 is part of a UTF-8 sequence and can only be
// inside an identifier or a string as far as any of this is concerned,
// so it counts as an identifier character and never as punctuation.
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

    /// Everything to the end of the line, the newline itself excluded --
    /// a comment that swallowed its own newline would colour the blank
    /// space to the right of it on a terminal that fills to the edge.
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

    /// A quoted run. `multiline` says whether a newline ends it: it does
    /// in JavaScript and Python's single-quoted forms, and does not in
    /// Rust, where an ordinary `"` string may span lines.
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

    /// A run delimited by a repeated quote -- Python's `"""`.
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

    /// Digits and whatever a number may carry with them: a base prefix,
    /// `_` separators, an exponent, a decimal point, and a type suffix
    /// glued to the end (`1u8`, `1.5f64`, `10n`). Taking the suffix as
    /// part of the number is what stops it being scanned as an
    /// identifier immediately afterwards.
    fn number(&mut self) {
        let start = self.pos;
        self.pos += 1;
        while !self.done() {
            let c = self.here();
            // An exponent's sign counts only directly after the `e`,
            // and only in a number that has an exponent to sign: `e` is
            // a digit in hex, so `0x1E-3` is a subtraction and `1.2e-3`
            // is one number. A leading `0` is what tells them apart,
            // since every based literal starts with one.
            if matches!(c, b'+' | b'-') && matches!(self.at(self.pos - 1), b'e' | b'E') && self.at(start) != b'0' {
                self.pos += 1;
                continue;
            }
            // A `.` is part of the number unless it is a method call on
            // one: `1.max(2)` is a number, a dot and a name.
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

// --- rust --------------------------------------------------------------

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
        // A raw string carries its own fence: `r"..."`, or `r#"..."#`
        // with as many `#` as it likes, which is how a Rust string holds
        // a quote without escaping one.
        if (c == b'r' || c == b'b') && rust_raw_string(&mut s) {
            continue;
        }
        match c {
            b'"' => s.quoted(b'"', true),
            // `'` is a character literal or a lifetime, and the two look
            // alike until the closing quote does or does not arrive.
            // Scanning it as a string either way would paint the rest of
            // the file from the first `&'a str` onwards.
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

/// `r"..."`, `r#"..."#`, `br"..."`, `br#"..."#` -- true if one started
/// here and was consumed.
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
    // `'static` is the one lifetime that is also a keyword, and reads
    // as one.
    let kind = match s.text(&(start + 1..at)) {
        "static" => HighlightKind::Keyword,
        _ => HighlightKind::Variable,
    };
    s.push(start, kind);
    true
}

// --- python ------------------------------------------------------------

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// The letters that may sit in front of a quote: raw, bytes, formatted,
/// unicode, in either case and in either order.
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
            // A decorator names what is being applied, which reads as
            // part of the declaration rather than as an expression.
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
                // `f"..."` is a prefix and its quote is what follows;
                // `f` alone is a name.
                if python_string_prefix(&word) && matches!(s.here(), b'"' | b'\'') {
                    let start = range.start;
                    python_string(&mut s);
                    // Redraw the string to include its own prefix, which
                    // is part of what the string is.
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

// --- javascript and typescript -----------------------------------------

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

/// TypeScript is JavaScript plus the words that describe types, so it
/// is the same scanner with a longer keyword list rather than a second
/// one. Everything lexical -- comments, strings, templates, numbers --
/// is identical between them.
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

/// A template literal is a string with holes in it. The string is
/// painted first and each `${...}` over the top, the same layering a
/// JSON string's escapes already use -- so the quoted text reads as
/// quoted text and the expressions in it read as something else,
/// without this having to lex JavaScript recursively.
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

#[cfg(test)]
mod tests {
    use super::*;

    // What a scanner produces, as text: each span's own source with its
    // kind, which is far easier to read in a failure than a list of byte
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

    // The case that decides whether this is usable on real Rust at all:
    // a lifetime is not an unterminated character literal, and reading
    // it as one paints the rest of the file.
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

    // A buffer being typed into is invalid most of the time. An
    // unterminated string takes what it can reach and the scanner stops
    // there rather than looping or panicking.
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

    // Real source, not a snippet: this module's own, which has the
    // constructs that are hard on purpose -- nested block comments,
    // raw strings containing quotes, lifetimes, character literals.
    // The assertion is not what it paints but that every span it
    // produces is a well-formed range inside the text, since a scanner
    // that loses its place produces one that is not.
    #[test]
    fn the_rust_scanner_survives_this_projects_own_source() {
        let text = include_str!("codehighlight.rs");
        let spans = rust(text);
        assert!(spans.len() > 200, "a file this size has more tokens than that: {}", spans.len());
        for (range, _) in &spans {
            assert!(range.start < range.end && range.end <= text.len(), "malformed span {range:?}");
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end), "span {range:?} splits a character");
        }
        // The whole file is not one string, which is what a scanner
        // that walked into a raw string and never came out would say.
        let longest = spans.iter().map(|(r, _)| r.end - r.start).max().unwrap_or(0);
        assert!(longest < text.len() / 4, "one span covers {longest} of {} bytes", text.len());
    }

    // Every scanner has to make progress on every byte it sees, or a
    // buffer with the wrong character in it hangs the editor.
    #[test]
    fn every_scanner_terminates_on_arbitrary_bytes() {
        let text = "a\u{e9}'\"`#@${}/*//\\0 1.2e-3 0x_ r#\" \u{1f600}";
        for scan in [rust, python, javascript, typescript] {
            let spans = scan(text);
            assert!(spans.iter().all(|(r, _)| r.start <= r.end && r.end <= text.len()));
        }
    }
}
