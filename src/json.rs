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

pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser { chars: input.chars().peekable(), pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.chars.peek().is_some() {
        return Err(format!("trailing data after the JSON value (position {})", p.pos));
    }
    Ok(v)
}

struct Parser<'a> {
    chars: Peekable<Chars<'a>>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.advance();
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.advance() == Some(c) {
            Ok(())
        } else {
            Err(format!("expected '{c}' (position {})", self.pos))
        }
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_ws();
        match self.chars.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => self.parse_string().map(Value::Str),
            Some('t') | Some('f') => self.parse_bool(),
            Some('n') => self.parse_null(),
            Some(c) if *c == '-' || c.is_ascii_digit() => self.parse_number(),
            other => Err(format!("unexpected {other:?} (position {})", self.pos)),
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect('{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.chars.peek() == Some(&'}') {
            self.advance();
            return Ok(Value::Object(fields));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(':')?;
            let value = self.parse_value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.advance() {
                Some(',') => continue,
                Some('}') => break,
                other => return Err(format!("expected ',' or closing brace in object (got {other:?}, position {})", self.pos)),
            }
        }
        Ok(Value::Object(fields))
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.chars.peek() == Some(&']') {
            self.advance();
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.advance() {
                Some(',') => continue,
                Some(']') => break,
                other => return Err(format!("expected ',' or ']' in array (got {other:?}, position {})", self.pos)),
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err("unterminated string".to_string()),
                Some('"') => break,
                Some('\\') => match self.advance() {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('/') => s.push('/'),
                    Some('b') => s.push('\u{8}'),
                    Some('f') => s.push('\u{c}'),
                    Some('n') => s.push('\n'),
                    Some('r') => s.push('\r'),
                    Some('t') => s.push('\t'),
                    Some('u') => {
                        let hi = self.parse_hex4()?;
                        // A surrogate pair (\uD800-\uDBFF followed by
                        // \uDC00-\uDFFF) encodes one codepoint outside
                        // the BMP (most commonly an emoji) -- combined
                        // here rather than left as two lone surrogates,
                        // which char::from_u32 would reject outright.
                        if (0xD800..=0xDBFF).contains(&hi) {
                            if self.advance() != Some('\\') || self.advance() != Some('u') {
                                return Err("expected a low surrogate after a high surrogate".to_string());
                            }
                            let lo = self.parse_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&lo) {
                                return Err("invalid low surrogate".to_string());
                            }
                            let c = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                            s.push(char::from_u32(c).ok_or_else(|| "invalid surrogate pair".to_string())?);
                        } else {
                            s.push(char::from_u32(hi).ok_or_else(|| "invalid \\u escape".to_string())?);
                        }
                    }
                    other => return Err(format!("invalid escape {other:?} (position {})", self.pos)),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.advance().ok_or_else(|| "unterminated \\u escape".to_string())?;
            let d = c.to_digit(16).ok_or_else(|| format!("invalid hex digit '{c}' in \\u escape"))?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_bool(&mut self) -> Result<Value, String> {
        if self.take_literal("true") {
            Ok(Value::Bool(true))
        } else if self.take_literal("false") {
            Ok(Value::Bool(false))
        } else {
            Err(format!("invalid literal (position {})", self.pos))
        }
    }

    fn parse_null(&mut self) -> Result<Value, String> {
        if self.take_literal("null") {
            Ok(Value::Null)
        } else {
            Err(format!("invalid literal (position {})", self.pos))
        }
    }

    fn take_literal(&mut self, lit: &str) -> bool {
        for expected in lit.chars() {
            if self.chars.peek() != Some(&expected) {
                return false;
            }
            self.advance();
        }
        true
    }

    fn parse_number(&mut self) -> Result<Value, String> {
        let mut raw = String::new();
        if self.chars.peek() == Some(&'-') {
            raw.push(self.advance().unwrap());
        }
        while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
            raw.push(self.advance().unwrap());
        }
        if self.chars.peek() == Some(&'.') {
            raw.push(self.advance().unwrap());
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                raw.push(self.advance().unwrap());
            }
        }
        if matches!(self.chars.peek(), Some('e') | Some('E')) {
            raw.push(self.advance().unwrap());
            if matches!(self.chars.peek(), Some('+') | Some('-')) {
                raw.push(self.advance().unwrap());
            }
            while matches!(self.chars.peek(), Some(c) if c.is_ascii_digit()) {
                raw.push(self.advance().unwrap());
            }
        }
        raw.parse::<f64>().map(Value::Number).map_err(|_| format!("invalid number '{raw}' (position {})", self.pos))
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
