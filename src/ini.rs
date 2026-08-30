// INI: `[section]` headers over `key = value` lines, the format behind
// `.gitconfig`, systemd units, `.desktop` entries, `setup.cfg`,
// `tox.ini` and most of `/etc`. Hand-rolled like every other parser
// here, no crate.
//
// There is no INI standard, only a family of dialects that disagree
// with each other -- so the interesting decision in this module is which
// disagreements to take a side on and which to sit out. The rule
// followed throughout: **where dialects conflict, prefer the reading
// that can only ever under-describe the file, never the one that
// misreads real content as syntax.** Concretely:
//
//   - **Comments are whole-line only** (`;` or `#` as the first
//     non-blank character), which is also what vim's `dosini` syntax
//     does. Git and classic Windows INI do allow a trailing `; comment`
//     after a value, and this deliberately misses those. The other
//     choice is worse: systemd and the Desktop Entry spec both say a
//     `#` mid-line is *value text*, so treating one as a comment would
//     grey out the second half of `ExecStart=/bin/sh -c 'echo #1'` --
//     wrong about the file's actual content, where missing a comment is
//     merely quiet.
//   - **`=` and `:` are both separators**, whichever comes first on the
//     line. They can't fight: a Windows `path=C:\tmp` and a systemd
//     `Environment=A:B` both hit `=` first, and Python's configparser
//     `key: value` has no `=` to lose to.
//   - **`[remote "origin"]` subsections are recognized everywhere**,
//     since no other dialect puts a quoted string in a header, so
//     understanding one costs nothing where it can't appear.
//
// Parsing is line-by-line and total: every line becomes some `Item`, and
// nothing a user can type is an error. That is what a highlighter needs
// (a buffer mid-edit is nonsense most of the time), and unlike JSON it
// costs nothing here -- INI has no nesting, so a broken line can't
// change the meaning of the lines below it, and there is no recovery to
// get wrong.
//
// Offsets throughout are **char** offsets, not byte offsets, matching
// what `highlight::HighlightSpan` is specified in.
#![allow(dead_code)]

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A whole-line `;`/`#` comment. The span covers the marker and the
    /// text, not the indentation before it.
    Comment { span: Range<usize> },
    /// `[core]`, or `[remote "origin"]` -- `name` is the bare part,
    /// `sub` the quoted one (quotes included), `span` the whole header
    /// including its brackets. A header missing its `]` still parses:
    /// it is what `[sec` is on the way to being.
    Section { name: Range<usize>, sub: Option<Range<usize>>, span: Range<usize> },
    /// `key = value`. `separator` is the offset of the `=`/`:` itself,
    /// absent for a bare `key` (git writes boolean-true flags that way).
    /// `value` is absent for `key =` with nothing after it.
    Entry { key: Range<usize>, separator: Option<usize>, value: Option<Value>, span: Range<usize> },
    /// An indented line continuing the value above -- configparser's
    /// multi-line values, as in `setup.cfg`'s `install_requires`.
    Continuation { value: Value, span: Range<usize> },
    /// A line with nothing on it. Carried rather than dropped so the
    /// items still describe the whole file in order.
    Blank { span: Range<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub span: Range<usize>,
    pub kind: ValueKind,
    /// `\n`, `\"`, `\\` inside a quoted value, as spans within `span`.
    /// Empty for every unquoted value: a backslash in bare text is a
    /// backslash (a Windows path is full of them).
    pub escapes: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// Opens with `"`. Whether it *closes* is not this type's business
    /// -- an unterminated quote ends at the end of its line, so typing
    /// one doesn't restyle the rest of the file.
    Quoted,
    Number,
    /// One of the words every dialect agrees is a boolean.
    Bool,
    /// Anything else, which is most values: a path, a command line, a
    /// URL, an arbitrary string nobody quoted.
    Plain,
}

pub fn parse(text: &str) -> Document {
    let chars: Vec<char> = text.chars().collect();
    let mut items = Vec::new();
    let mut start = 0usize;
    // Whether the last entry ended with a separator and no value, which
    // is what opens a configparser continuation block. Only then is an
    // indented line below read as a continuation rather than as a key of
    // its own -- git indents its keys too (`[core]` / tab / `bare`), and
    // this is what tells the two apart.
    let mut continuing = false;
    while start <= chars.len() {
        let end = chars[start..].iter().position(|c| *c == '\n').map(|i| start + i).unwrap_or(chars.len());
        let item = line_item(&chars, start..end, &mut continuing);
        items.push(item);
        if end == chars.len() {
            break;
        }
        start = end + 1;
    }
    Document { items }
}

fn line_item(chars: &[char], line: Range<usize>, continuing: &mut bool) -> Item {
    let indented = chars[line.clone()].first().is_some_and(|c| *c == ' ' || *c == '\t');
    let body = trim(chars, line.clone());
    if body.is_empty() {
        // A blank line ends a continuation block, the same way it ends a
        // paragraph: configparser treats an empty line as the end of a
        // multi-line value unless it is itself indented.
        if !indented {
            *continuing = false;
        }
        return Item::Blank { span: line };
    }
    match chars[body.start] {
        ';' | '#' => {
            *continuing = false;
            Item::Comment { span: body }
        }
        // Checked before the separator scan on purpose: a continuation
        // line's text is value, not syntax, and `foo >= 1.0` under an
        // `install_requires =` would otherwise split at its own `=`.
        _ if *continuing && indented => Item::Continuation { value: value_at(chars, body.clone()), span: body },
        '[' => {
            *continuing = false;
            section(chars, body)
        }
        _ => entry(chars, body, continuing),
    }
}

fn section(chars: &[char], body: Range<usize>) -> Item {
    // The *last* `]`, so a name containing one (`[a]b]`) keeps it rather
    // than the header ending early. Missing entirely, the header runs to
    // the end of its line -- a half-typed `[sec` is still a section.
    let close = chars[body.clone()].iter().rposition(|c| *c == ']').map(|i| body.start + i);
    let inner = body.start + 1..close.unwrap_or(body.end);
    let inner = trim(chars, inner);
    // `[remote "origin"]`: the quoted part is the subsection, the rest
    // the name.
    let quote = chars[inner.clone()].iter().position(|c| *c == '"').map(|i| inner.start + i);
    let (name, sub) = match quote {
        Some(q) => (trim(chars, inner.start..q), Some(trim(chars, q..inner.end))),
        None => (inner, None),
    };
    Item::Section { name, sub, span: body }
}

fn entry(chars: &[char], body: Range<usize>, continuing: &mut bool) -> Item {
    let separator = chars[body.clone()].iter().position(|c| *c == '=' || *c == ':').map(|i| body.start + i);
    let Some(separator) = separator else {
        // A bare word: git's own way of writing a true flag. Not a
        // continuation -- that case was decided before we got here.
        *continuing = false;
        return Item::Entry { key: body.clone(), separator: None, value: None, span: body };
    };
    let key = trim(chars, body.start..separator);
    let rest = trim(chars, separator + 1..body.end);
    // An empty value opens a continuation block; anything else closes
    // one.
    *continuing = rest.is_empty();
    let value = (!rest.is_empty()).then(|| value_at(chars, rest));
    Item::Entry { key, separator: Some(separator), value, span: body }
}

fn value_at(chars: &[char], span: Range<usize>) -> Value {
    if chars[span.start] == '"' {
        let mut escapes = Vec::new();
        let mut i = span.start + 1;
        while i < span.end {
            match chars[i] {
                '\\' if i + 1 < span.end => {
                    escapes.push(i..i + 2);
                    i += 2;
                }
                '"' => {
                    // Everything after the closing quote is left inside
                    // the value's own span rather than split off: with
                    // no inline comments there is nothing else it could
                    // be, and a trailing `\` continuation belongs to the
                    // value anyway.
                    break;
                }
                _ => i += 1,
            }
        }
        return Value { span, kind: ValueKind::Quoted, escapes };
    }
    let text: String = chars[span.clone()].iter().collect();
    let kind = if is_bool(&text) {
        ValueKind::Bool
    } else if is_number(&text) {
        ValueKind::Number
    } else {
        ValueKind::Plain
    };
    Value { span, kind, escapes: Vec::new() }
}

// The words the dialects agree on. `1`/`0` are booleans too in most of
// them, but they are also numbers, and saying "number" about a digit is
// never wrong.
fn is_bool(text: &str) -> bool {
    matches!(text.to_ascii_lowercase().as_str(), "true" | "false" | "yes" | "no" | "on" | "off")
}

fn is_number(text: &str) -> bool {
    let text = text.strip_prefix(['+', '-']).unwrap_or(text);
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit());
    }
    // A size suffix (`1M`, `30s`) is deliberately *not* a number: those
    // are units, and only some dialects have them.
    !text.is_empty() && text.parse::<f64>().is_ok() && text.chars().any(|c| c.is_ascii_digit())
}

// The span with surrounding whitespace (`\r` included, so a CRLF file
// behaves) removed from both ends.
fn trim(chars: &[char], span: Range<usize>) -> Range<usize> {
    let blank = |c: char| c == ' ' || c == '\t' || c == '\r';
    let mut start = span.start;
    let mut end = span.end;
    while start < end && blank(chars[start]) {
        start += 1;
    }
    while end > start && blank(chars[end - 1]) {
        end -= 1;
    }
    start..end
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

    // Every line becomes exactly one item, in order, covering the file.
    #[test]
    fn every_line_produces_one_item() {
        assert_eq!(items("a=1\n; c\n\n[s]\n").len(), 5, "the empty last line counts too");
        assert_eq!(items("").len(), 1);
    }

    #[test]
    fn a_section_header_splits_into_name_and_subsection() {
        let src = "[remote \"origin\"]";
        let Item::Section { name, sub, span } = &items(src)[0] else { panic!("expected a section") };
        assert_eq!(text_of(src, name), "remote");
        assert_eq!(text_of(src, sub.as_ref().unwrap()), "\"origin\"");
        assert_eq!(text_of(src, span), src);
    }

    #[test]
    fn a_plain_section_header_has_no_subsection() {
        let src = "  [core]  ";
        let Item::Section { name, sub, span } = &items(src)[0] else { panic!("expected a section") };
        assert_eq!(text_of(src, name), "core");
        assert_eq!(*sub, None);
        assert_eq!(text_of(src, span), "[core]", "the indentation is not part of the header");
    }

    // A header being typed is still a header, or the whole file below it
    // would restyle on the way to the `]`.
    #[test]
    fn an_unclosed_section_header_still_parses() {
        let src = "[sec";
        let Item::Section { name, .. } = &items(src)[0] else { panic!("expected a section") };
        assert_eq!(text_of(src, name), "sec");
    }

    #[test]
    fn an_entry_splits_on_whichever_separator_comes_first() {
        for (src, key, value) in
            [("a = 1", "a", "1"), ("a: 1", "a", "1"), ("path=C:\\tmp", "path", "C:\\tmp"), ("Environment=A:B", "Environment", "A:B")]
        {
            let Item::Entry { key: k, value: v, .. } = &items(src)[0] else { panic!("expected an entry: {src}") };
            assert_eq!(text_of(src, k), key);
            assert_eq!(text_of(src, &v.as_ref().unwrap().span), value);
        }
    }

    // git writes a true flag as the bare key.
    #[test]
    fn a_bare_key_is_an_entry_with_no_separator() {
        let src = "[core]\n\tbare\n";
        let Item::Entry { key, separator, value, .. } = &items(src)[1] else { panic!("expected an entry") };
        assert_eq!(text_of(src, key), "bare");
        assert_eq!(*separator, None);
        assert_eq!(*value, None);
    }

    #[test]
    fn comments_are_recognized_at_the_start_of_a_line_only() {
        let src = "; a comment\n  # also one\nkey = value ; not one\n";
        assert!(matches!(items(src)[0], Item::Comment { .. }));
        assert!(matches!(items(src)[1], Item::Comment { .. }));
        let Item::Entry { value, .. } = &items(src)[2] else { panic!("expected an entry") };
        assert_eq!(
            text_of(src, &value.as_ref().unwrap().span),
            "value ; not one",
            "a mid-line marker is value text, since systemd and .desktop say so"
        );
    }

    // The `#` in a systemd unit's own shell command must survive.
    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        let src = "ExecStart=/bin/sh -c 'echo #1'";
        let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
        assert_eq!(text_of(src, &value.as_ref().unwrap().span), "/bin/sh -c 'echo #1'");
    }

    #[test]
    fn an_empty_value_opens_a_continuation_block() {
        let src = "install_requires =\n    foo >= 1.0\n    bar\n";
        assert!(matches!(items(src)[0], Item::Entry { value: None, separator: Some(_), .. }));
        let Item::Continuation { value, .. } = &items(src)[1] else { panic!("expected a continuation") };
        assert_eq!(text_of(src, &value.span), "foo >= 1.0", "not split at its own `=`");
        assert!(matches!(items(src)[2], Item::Continuation { .. }));
    }

    // The case that makes the "an empty value opened it" rule necessary:
    // git indents its keys too, so indentation alone can't decide.
    #[test]
    fn an_indented_key_under_a_section_is_a_key_not_a_continuation() {
        let src = "[core]\n\tbare = false\n\tfilemode = true\n";
        assert!(matches!(items(src)[1], Item::Entry { .. }));
        assert!(matches!(items(src)[2], Item::Entry { .. }));
    }

    #[test]
    fn a_blank_line_ends_a_continuation_block() {
        let src = "a =\n    x\n\ny = 1\n";
        assert!(matches!(items(src)[1], Item::Continuation { .. }));
        assert!(matches!(items(src)[3], Item::Entry { .. }));
    }

    #[test]
    fn value_kinds_are_recognized() {
        let kind = |src: &str| {
            let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
            value.as_ref().unwrap().kind
        };
        assert_eq!(kind("a = 1"), ValueKind::Number);
        assert_eq!(kind("a = -2.5"), ValueKind::Number);
        assert_eq!(kind("a = 0xFF"), ValueKind::Number);
        assert_eq!(kind("a = true"), ValueKind::Bool);
        assert_eq!(kind("a = Off"), ValueKind::Bool);
        assert_eq!(kind("a = \"x\""), ValueKind::Quoted);
        assert_eq!(kind("a = /usr/bin/env"), ValueKind::Plain);
        assert_eq!(kind("a = 30s"), ValueKind::Plain, "a unit suffix is not a number");
        assert_eq!(kind("a = inf"), ValueKind::Plain, "`f64::parse` accepts it, INI does not mean it");
    }

    #[test]
    fn escapes_inside_a_quoted_value_are_located() {
        let src = "a = \"one\\ttwo\\\"\"";
        let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
        let value = value.as_ref().unwrap();
        let found: Vec<String> = value.escapes.iter().map(|e| text_of(src, e)).collect();
        assert_eq!(found, vec!["\\t", "\\\""]);
    }

    #[test]
    fn a_backslash_in_an_unquoted_value_is_just_a_backslash() {
        let src = "path = C:\\Users\\me";
        let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
        assert!(value.as_ref().unwrap().escapes.is_empty());
    }

    // Nothing may swallow the rest of the file.
    #[test]
    fn an_unterminated_quote_ends_at_its_own_line() {
        let src = "a = \"open\nb = 1\n";
        let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
        assert_eq!(text_of(src, &value.as_ref().unwrap().span), "\"open");
        assert!(matches!(items(src)[1], Item::Entry { .. }));
    }

    #[test]
    fn crlf_line_endings_do_not_end_up_inside_a_value() {
        let src = "a = 1\r\nb = 2\r\n";
        let Item::Entry { value, .. } = &items(src)[0] else { panic!("expected an entry") };
        assert_eq!(text_of(src, &value.as_ref().unwrap().span), "1");
    }

    // Spans are char offsets, so a non-ASCII line above doesn't shift
    // everything below by the bytes it happens to take.
    #[test]
    fn spans_are_char_offsets() {
        let src = "a = \u{e4}\u{e4}\nb = 2\n";
        let Item::Entry { key, .. } = &items(src)[1] else { panic!("expected an entry") };
        assert_eq!(text_of(src, key), "b");
        assert_eq!(key.start, 7, "char offsets, not the 9 bytes this line actually starts at");
    }

    // Anything at all can be in the buffer while it is being typed.
    #[test]
    fn nothing_typeable_panics() {
        for src in ["", "\n", "=", ":", "[", "]", "[]", "[\"\"]", "\"", "\\", "a=", "=a", "   ", "\t\t", "[a] = b", "#", ";"] {
            let doc = parse(src);
            assert_eq!(doc.items.len(), src.chars().filter(|c| *c == '\n').count() + 1, "for {src:?}");
        }
    }
}
