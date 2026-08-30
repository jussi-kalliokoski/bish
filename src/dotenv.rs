// `.env` files, as dotenv and everything that grew out of it read them.
//
// This looks like `ini.rs` from a distance and is deliberately not built
// on it, because the two disagree about the things that matter:
//
//   - **There are no sections.** A `[` in a `.env` file is an ordinary
//     character in a value.
//   - **`#` starts a comment mid-line**, where INI's does not. INI had
//     to sit that one out because systemd and the Desktop Entry spec
//     both say a mid-line `#` is value text; dotenv has no such
//     constituency -- every implementation of it strips trailing
//     comments -- so here the useful reading is also the correct one. It
//     still needs whitespace before it, so `URL=http://x/#frag` keeps
//     its fragment.
//   - **`$VAR` and `${VAR}` are interpolation**, and marking them is
//     most of the reason to know this format apart from INI at all: it
//     is the one part of the line whose text is not what it says.
//   - **`export KEY=value` is ordinary**, because a `.env` is so often
//     also `source`d.
//
// Offsets are **char** offsets, matching `highlight::HighlightSpan`.
#![allow(dead_code)]

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Comment { span: Range<usize> },
    Blank { span: Range<usize> },
    /// `KEY=value`, with or without a leading `export`. `separator` is
    /// the `=`; a key with none is a line still being typed rather than
    /// an error worth shouting about.
    Entry {
        export: Option<Range<usize>>,
        key: Range<usize>,
        separator: Option<usize>,
        value: Option<Value>,
        comment: Option<Range<usize>>,
        span: Range<usize>,
    },
    /// A line that is no part of any of the above.
    Junk { span: Range<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub span: Range<usize>,
    pub kind: ValueKind,
    /// `\n`, `\"` and friends -- only ever inside a double-quoted value.
    pub escapes: Vec<Range<usize>>,
    /// `$VAR` / `${VAR}` / `${VAR:-default}`, wherever interpolation
    /// applies. Empty inside a single-quoted value, which is exactly
    /// what single quotes mean here.
    pub expansions: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `"..."`: escapes and interpolation both apply.
    Quoted,
    /// `'...'`: neither does.
    Literal,
    /// Unquoted. Interpolation applies; escapes do not.
    Bare,
    /// An unquoted value that is only a number.
    Number,
    /// An unquoted `true`/`false`/`yes`/`no`/`on`/`off`.
    Bool,
}

pub fn parse(text: &str) -> Document {
    let chars: Vec<char> = text.chars().collect();
    let mut parser = Parser { chars: &chars, pos: 0, items: Vec::new() };
    parser.run();
    Document { items: parser.items }
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    items: Vec<Item>,
}

impl Parser<'_> {
    fn run(&mut self) {
        while self.pos < self.chars.len() {
            self.item();
        }
        // A file ending in a newline has one more (empty) line, and
        // saying so keeps the items a description of the whole file.
        if self.chars.last() == Some(&'\n') || self.chars.is_empty() {
            self.items.push(Item::Blank { span: self.pos..self.pos });
        }
    }

    fn item(&mut self) {
        let line_end = self.line_end(self.pos);
        let start = self.skip_blanks(self.pos, line_end);
        if start == line_end {
            self.items.push(Item::Blank { span: self.pos..line_end });
            self.pos = (line_end + 1).min(self.chars.len().max(line_end));
            return;
        }
        if self.chars[start] == '#' {
            self.items.push(Item::Comment { span: start..line_end });
            self.pos = line_end + 1;
            return;
        }
        self.entry(start, line_end);
    }

    fn entry(&mut self, start: usize, line_end: usize) {
        // `export KEY=...`, the form that makes a `.env` also sourceable.
        let mut at = start;
        let mut export = None;
        if self.word_at(at, "export") {
            let after = self.skip_blanks(at + 6, line_end);
            // Only when a key really follows: a key literally named
            // `export` is legal, and `export=1` is that.
            if after > at + 6 && after < line_end && is_key_char(self.chars[after]) {
                export = Some(at..at + 6);
                at = after;
            }
        }
        let key_start = at;
        while at < line_end && is_key_char(self.chars[at]) {
            at += 1;
        }
        if at == key_start {
            self.items.push(Item::Junk { span: start..line_end });
            self.pos = line_end + 1;
            return;
        }
        let key = key_start..at;
        let at = self.skip_blanks(at, line_end);
        if at >= line_end || self.chars[at] != '=' {
            // A key with nothing after it: a line halfway through being
            // typed, not something to complain about.
            self.items.push(Item::Entry { export, key, separator: None, value: None, comment: None, span: start..line_end });
            self.pos = line_end + 1;
            return;
        }
        let separator = at;
        let (value, comment, end) = self.value(at + 1, line_end);
        self.items.push(Item::Entry { export, key, separator: Some(separator), value, comment, span: start..end });
        self.pos = end + 1;
    }

    // The value after `=`, plus any trailing comment, plus where the
    // whole entry ends -- which is normally this line's end, but can be
    // further down when a quoted value spans lines.
    fn value(&mut self, from: usize, line_end: usize) -> (Option<Value>, Option<Range<usize>>, usize) {
        let start = self.skip_blanks(from, line_end);
        if start >= line_end {
            return (None, None, line_end);
        }
        match self.chars[start] {
            '#' => (None, Some(start..line_end), line_end),
            '"' | '\'' => {
                let quote = self.chars[start];
                let end = self.quoted_end(start, quote);
                let escapes = if quote == '"' { self.escapes(start + 1, end) } else { Vec::new() };
                let expansions = if quote == '"' { self.expansions(start + 1, end) } else { Vec::new() };
                let kind = if quote == '"' { ValueKind::Quoted } else { ValueKind::Literal };
                let value = Value { span: start..end, kind, escapes, expansions };
                // Whatever follows a closing quote on that same line.
                let tail_end = self.line_end(end);
                let after = self.skip_blanks(end, tail_end);
                let comment = (after < tail_end && self.chars[after] == '#').then_some(after..tail_end);
                (Some(value), comment, tail_end)
            }
            _ => {
                let (end, comment) = self.bare_end(start, line_end);
                let text: String = self.chars[start..end].iter().collect();
                let kind = if is_bool(&text) {
                    ValueKind::Bool
                } else if is_number(&text) {
                    ValueKind::Number
                } else {
                    ValueKind::Bare
                };
                let expansions = self.expansions(start, end);
                let value = (end > start).then(|| Value { span: start..end, kind, escapes: Vec::new(), expansions });
                (value, comment, line_end)
            }
        }
    }

    // Where a quoted value ends, one past its closing quote.
    //
    // A double- or single-quoted value may span lines -- a PEM key in a
    // `.env` is the usual reason -- but only until a line that looks
    // like a new assignment. That bound is the whole design decision
    // here, and it exists because neither unbounded answer is good: let
    // a quote span freely and one unterminated quote restyles the rest
    // of the file; stop it at its own line and a real multi-line value's
    // continuation lines get read as *keys*, which is misreading content
    // as syntax. Recovering at the next `KEY=` keeps a genuine
    // multi-line value whole and keeps a typo's blast radius to the
    // entry it was made in.
    fn quoted_end(&self, start: usize, quote: char) -> usize {
        let mut at = start + 1;
        while at < self.chars.len() {
            match self.chars[at] {
                '\\' if quote == '"' => at += 2,
                c if c == quote => return at + 1,
                '\n' => {
                    let next = at + 1;
                    if self.looks_like_assignment(next) {
                        return at;
                    }
                    at = next;
                }
                _ => at += 1,
            }
        }
        self.chars.len()
    }

    // Where an unquoted value ends, and the comment (if any) after it. A
    // `#` needs whitespace before it, so `URL=http://x/#frag` keeps its
    // fragment and `PORT=3000 # the port` loses its comment.
    fn bare_end(&self, start: usize, line_end: usize) -> (usize, Option<Range<usize>>) {
        let mut at = start;
        while at < line_end {
            if self.chars[at] == '#' && at > start && is_blank(self.chars[at - 1]) {
                let mut end = at;
                while end > start && is_blank(self.chars[end - 1]) {
                    end -= 1;
                }
                return (end, Some(at..line_end));
            }
            at += 1;
        }
        let mut end = line_end;
        while end > start && is_blank(self.chars[end - 1]) {
            end -= 1;
        }
        (end, None)
    }

    // Whether the line beginning at `at` is a fresh `KEY=` -- what ends
    // a runaway quoted value. Deliberately strict: an indented line, or
    // one whose key holds anything unusual, is far more likely to be
    // part of a value than a new entry.
    fn looks_like_assignment(&self, at: usize) -> bool {
        let mut i = at;
        if self.word_at(i, "export") && self.chars.get(i + 6).is_some_and(|c| *c == ' ') {
            i += 7;
        }
        let key_start = i;
        while self.chars.get(i).is_some_and(|c| is_key_char(*c)) {
            i += 1;
        }
        i > key_start && self.chars.get(i) == Some(&'=')
    }

    fn escapes(&self, from: usize, to: usize) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        let mut at = from;
        while at + 1 < to.min(self.chars.len()) {
            if self.chars[at] == '\\' {
                out.push(at..at + 2);
                at += 2;
            } else {
                at += 1;
            }
        }
        out
    }

    // `$VAR`, `${VAR}` and `${VAR:-default}`. A `$` that opens neither
    // is just a dollar sign.
    fn expansions(&self, from: usize, to: usize) -> Vec<Range<usize>> {
        let end = to.min(self.chars.len());
        let mut out = Vec::new();
        let mut at = from;
        while at < end {
            if self.chars[at] != '$' {
                at += 1;
                continue;
            }
            // `\$` is an escaped dollar, not an expansion.
            if at > from && self.chars[at - 1] == '\\' {
                at += 1;
                continue;
            }
            let start = at;
            at += 1;
            if self.chars.get(at) == Some(&'{') {
                while at < end && self.chars[at] != '}' {
                    at += 1;
                }
                at = (at + 1).min(end);
                out.push(start..at);
                continue;
            }
            let name_start = at;
            while at < end && is_key_char(self.chars[at]) {
                at += 1;
            }
            if at > name_start {
                out.push(start..at);
            }
        }
        out
    }

    fn line_end(&self, from: usize) -> usize {
        self.chars[from.min(self.chars.len())..].iter().position(|c| *c == '\n').map_or(self.chars.len(), |i| from + i)
    }

    fn skip_blanks(&self, mut at: usize, limit: usize) -> usize {
        while at < limit && is_blank(self.chars[at]) {
            at += 1;
        }
        at
    }

    fn word_at(&self, at: usize, word: &str) -> bool {
        self.chars[at.min(self.chars.len())..].iter().take(word.chars().count()).copied().eq(word.chars())
    }
}

// `.` and `-` are unusual but real: plenty of tools write
// `SERVICE.PORT` or `some-key` into a `.env`.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\r'
}

fn is_bool(text: &str) -> bool {
    matches!(text.to_ascii_lowercase().as_str(), "true" | "false" | "yes" | "no" | "on" | "off")
}

fn is_number(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    !body.is_empty() && body.parse::<f64>().is_ok() && body.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(text: &str) -> Vec<Item> {
        parse(text).items
    }

    fn text_of(text: &str, span: &Range<usize>) -> String {
        text.chars().skip(span.start).take(span.end - span.start).collect()
    }

    fn entry(text: &str, index: usize) -> (String, String) {
        match &items(text)[index] {
            Item::Entry { key, value, .. } => {
                (text_of(text, key), value.as_ref().map(|v| text_of(text, &v.span)).unwrap_or_default())
            }
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    #[test]
    fn a_pair_is_a_key_and_a_value() {
        assert_eq!(entry("PORT=3000", 0), ("PORT".to_string(), "3000".to_string()));
        assert_eq!(entry("PORT = 3000", 0), ("PORT".to_string(), "3000".to_string()));
        assert_eq!(entry("EMPTY=", 0), ("EMPTY".to_string(), String::new()));
    }

    #[test]
    fn export_is_recognized_and_is_not_part_of_the_key() {
        let Item::Entry { export, key, .. } = &items("export PORT=3000")[0] else { panic!("expected an entry") };
        assert_eq!(text_of("export PORT=3000", export.as_ref().unwrap()), "export");
        assert_eq!(text_of("export PORT=3000", key), "PORT");
    }

    // ...but a key really named `export` is still a key.
    #[test]
    fn a_key_named_export_is_not_the_keyword() {
        let src = "export=1";
        let Item::Entry { export, key, .. } = &items(src)[0] else { panic!("expected an entry") };
        assert_eq!(*export, None);
        assert_eq!(text_of(src, key), "export");
    }

    #[test]
    fn value_kinds_are_recognized() {
        let kind = |src: &str| match &items(src)[0] {
            Item::Entry { value: Some(v), .. } => v.kind,
            other => panic!("expected a value: {other:?}"),
        };
        assert_eq!(kind("A=3000"), ValueKind::Number);
        assert_eq!(kind("A=-1.5"), ValueKind::Number);
        assert_eq!(kind("A=true"), ValueKind::Bool);
        assert_eq!(kind("A=\"x\""), ValueKind::Quoted);
        assert_eq!(kind("A='x'"), ValueKind::Literal);
        assert_eq!(kind("A=/usr/bin"), ValueKind::Bare);
    }

    // The rule INI had to sit out, and the one dotenv actually wants.
    #[test]
    fn a_hash_after_whitespace_starts_a_comment() {
        assert_eq!(entry("PORT=3000 # the port", 0).1, "3000");
        let Item::Entry { comment, .. } = &items("PORT=3000 # the port")[0] else { panic!() };
        assert_eq!(text_of("PORT=3000 # the port", comment.as_ref().unwrap()), "# the port");
    }

    // ...and only after whitespace, so a URL keeps its fragment.
    #[test]
    fn a_hash_inside_a_word_is_part_of_the_value() {
        assert_eq!(entry("URL=http://x/#frag", 0).1, "http://x/#frag");
    }

    #[test]
    fn a_hash_inside_quotes_is_never_a_comment() {
        assert_eq!(entry("A=\"a # b\"", 0).1, "\"a # b\"");
        let Item::Entry { comment, .. } = &items("A=\"a # b\"")[0] else { panic!() };
        assert_eq!(*comment, None);
    }

    #[test]
    fn a_whole_line_comment_is_a_comment() {
        assert!(matches!(items("# a note\nA=1")[0], Item::Comment { .. }));
        assert!(matches!(items("   # indented")[0], Item::Comment { .. }));
    }

    // The distinctive part: the piece of the line whose text isn't what
    // it says.
    #[test]
    fn interpolation_is_located_in_bare_and_double_quoted_values() {
        let expansions = |src: &str| match &items(src)[0] {
            Item::Entry { value: Some(v), .. } => v.expansions.iter().map(|e| text_of(src, e)).collect::<Vec<_>>(),
            other => panic!("expected a value: {other:?}"),
        };
        assert_eq!(expansions("A=$HOME/bin"), vec!["$HOME"]);
        assert_eq!(expansions("A=\"${HOME}/bin\""), vec!["${HOME}"]);
        assert_eq!(expansions("A=\"${PORT:-3000}\""), vec!["${PORT:-3000}"]);
        assert_eq!(expansions("A=$A:$B"), vec!["$A", "$B"]);
    }

    // Single quotes mean exactly this.
    #[test]
    fn a_single_quoted_value_interpolates_nothing() {
        let Item::Entry { value: Some(v), .. } = &items("A='$HOME'")[0] else { panic!() };
        assert!(v.expansions.is_empty());
        assert!(v.escapes.is_empty());
    }

    #[test]
    fn a_lone_dollar_is_not_an_expansion() {
        let Item::Entry { value: Some(v), .. } = &items("A=cost$")[0] else { panic!() };
        assert!(v.expansions.is_empty());
    }

    #[test]
    fn escapes_are_located_in_a_double_quoted_value_only() {
        let escapes = |src: &str| match &items(src)[0] {
            Item::Entry { value: Some(v), .. } => v.escapes.len(),
            other => panic!("expected a value: {other:?}"),
        };
        assert_eq!(escapes("A=\"a\\nb\""), 1);
        assert_eq!(escapes("A='a\\nb'"), 0);
        assert_eq!(escapes("A=a\\nb"), 0, "a backslash in a bare value is a backslash");
    }

    // A real multi-line value stays one value.
    #[test]
    fn a_quoted_value_may_span_lines() {
        let src = "KEY=\"-----BEGIN-----\nabc\ndef\n-----END-----\"\nNEXT=1";
        assert_eq!(entry(src, 0).1, "\"-----BEGIN-----\nabc\ndef\n-----END-----\"");
        assert_eq!(entry(src, 1), ("NEXT".to_string(), "1".to_string()));
    }

    // ...and an unterminated one stops at the next assignment rather
    // than swallowing the rest of the file. Neither answer is free; see
    // quoted_end's own comment for why this is the one chosen.
    #[test]
    fn an_unterminated_quote_stops_at_the_next_assignment() {
        let src = "A=\"oops\nB=2\nC=3";
        assert_eq!(entry(src, 0).1, "\"oops");
        assert_eq!(entry(src, 1), ("B".to_string(), "2".to_string()));
        assert_eq!(entry(src, 2), ("C".to_string(), "3".to_string()));
    }

    // An indented continuation line is value text, not a new entry --
    // which is what keeps the recovery rule from firing on real data.
    #[test]
    fn an_indented_line_does_not_end_a_quoted_value() {
        let src = "A=\"one\n  b=2\nstill\"\nB=1";
        assert_eq!(entry(src, 0).1, "\"one\n  b=2\nstill\"");
    }

    #[test]
    fn a_key_with_no_equals_is_an_entry_still_being_typed() {
        let Item::Entry { key, separator, value, .. } = &items("PORT")[0] else { panic!("expected an entry") };
        assert_eq!(text_of("PORT", key), "PORT");
        assert_eq!(*separator, None);
        assert_eq!(*value, None);
    }

    #[test]
    fn spans_are_char_offsets() {
        let src = "A=\u{e4}\u{e4}\nB=2";
        let Item::Entry { key, .. } = &items(src)[1] else { panic!("expected an entry") };
        assert_eq!(key.start, 5, "char offsets, not the 7 bytes this lands at");
    }

    #[test]
    fn nothing_typeable_panics() {
        for src in ["", "\n", "=", "=1", "\"", "'", "$", "${", "A=", "A=\"", "A='", "export", "export ", "#", "   ", "A=$", "A=${"] {
            parse(src);
        }
    }
}
