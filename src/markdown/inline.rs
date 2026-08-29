// Inline parsing, CommonMark §6 plus GFM strikethrough and autolinks.
//
// Emphasis is why this can't be a simple scan. Whether `*` opens, closes,
// both or neither depends on the characters on *both* sides of it, and a
// run of them may have to be split between an opener and a closer -- so
// the spec's algorithm collects delimiter runs first and resolves them
// afterwards, from a stack, which is what this does.
//
// Every span this produces is a char offset into the original document,
// not into the text being scanned. Those differ: a block quote's content
// has its `> ` markers stripped and its lines joined, so `Content` keeps
// a source offset per character to map back.

use std::ops::Range;

use super::{Inline, LinkRef};

// A block's text, with each character's own offset in the source. Built
// by block.rs from the lines it accumulated.
#[derive(Debug, Clone, PartialEq)]
pub struct Content {
    pub chars: Vec<char>,
    // `offsets[i]` is where `chars[i]` came from in the document.
    pub offsets: Vec<usize>,
}

impl Content {
    pub fn from_line(text: &str, start: usize) -> Content {
        let chars: Vec<char> = text.chars().collect();
        let offsets = (0..chars.len()).map(|i| start + i).collect();
        Content { chars, offsets }
    }

    // Several lines joined by newlines, each remembering where it began
    // -- the mapping that makes a span in a block quote point at the
    // right place in the file rather than at the stripped text.
    pub fn from_lines(lines: &[(String, usize)]) -> Content {
        let mut chars = Vec::new();
        let mut offsets = Vec::new();
        for (i, (text, start)) in lines.iter().enumerate() {
            if i > 0 {
                chars.push('\n');
                offsets.push(offsets.last().map(|o| o + 1).unwrap_or(*start));
            }
            for (j, c) in text.chars().enumerate() {
                chars.push(c);
                offsets.push(start + j);
            }
        }
        Content { chars, offsets }
    }

    pub fn slice_from(&self, from: usize) -> Content {
        let from = from.min(self.chars.len());
        Content { chars: self.chars[from..].to_vec(), offsets: self.offsets[from..].to_vec() }
    }

    pub fn is_blank(&self) -> bool {
        self.chars.iter().all(|c| c.is_whitespace())
    }

    pub fn source_offset(&self, i: usize) -> Option<usize> {
        self.offsets.get(i).copied()
    }

    // The source span covering `chars[from..to]`. One past the last
    // character's offset, so an empty range is empty.
    fn span(&self, from: usize, to: usize) -> Range<usize> {
        let start = self.offsets.get(from).copied().unwrap_or_else(|| self.offsets.last().map(|o| o + 1).unwrap_or(0));
        let end = match to.checked_sub(1).and_then(|i| self.offsets.get(i)) {
            Some(o) => o + 1,
            None => start,
        };
        start..end
    }

    fn text(&self, from: usize, to: usize) -> String {
        self.chars[from.min(self.chars.len())..to.min(self.chars.len())].iter().collect()
    }
}

// One run of `*`, `_` or `~`, with what the characters around it say
// about whether it may open or close.
#[derive(Debug, Clone)]
struct Delimiter {
    c: char,
    // Where in the output the run's own text node sits, so resolving can
    // shorten it in place.
    node: usize,
    count: usize,
    can_open: bool,
    can_close: bool,
    active: bool,
}

pub fn parse(content: &Content, refs: &[LinkRef]) -> Vec<Inline> {
    let mut p = InlineParser { c: content, refs, pos: 0, out: Vec::new(), delims: Vec::new(), brackets: Vec::new() };
    p.run();
    p.finish()
}

// An unresolved `[` or `![`, waiting to find out whether a link ever
// closes it.
#[derive(Debug, Clone)]
struct Bracket {
    node: usize,
    pos: usize,
    image: bool,
    active: bool,
}

struct InlineParser<'a> {
    c: &'a Content,
    refs: &'a [LinkRef],
    pos: usize,
    out: Vec<Inline>,
    delims: Vec<Delimiter>,
    brackets: Vec<Bracket>,
}

impl<'a> InlineParser<'a> {
    fn at(&self, i: usize) -> Option<char> {
        self.c.chars.get(i).copied()
    }

    fn push_text(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let text = self.c.text(from, to);
        let span = self.c.span(from, to);
        // Deliberately *not* merged into the previous text node here: a
        // delimiter run is also a text node, and the emphasis algorithm
        // addresses it by index. Merging would fold `*` and the word
        // after it into one node and invalidate every index the
        // delimiter stack holds. Adjacent text is coalesced once, at the
        // end, when no index still matters (see `coalesce`).
        self.out.push(Inline::Text { text, span });
    }

    fn run(&mut self) {
        let mut text_start = 0;
        while self.pos < self.c.chars.len() {
            let start = self.pos;
            let handled = match self.at(self.pos) {
                Some('\\') => self.backslash(text_start),
                Some('`') => self.code_span(text_start),
                Some('<') => self.autolink_or_html(text_start),
                Some('*') | Some('_') | Some('~') => self.delimiter_run(text_start),
                Some('[') => self.open_bracket(text_start, false),
                Some('!') if self.at(self.pos + 1) == Some('[') => self.open_bracket(text_start, true),
                Some(']') => self.close_bracket(text_start),
                Some('\n') => self.line_break(text_start),
                Some('&') => self.entity(text_start),
                _ => false,
            };
            if handled {
                text_start = self.pos;
            } else {
                self.pos = start + 1;
            }
        }
        self.push_text(text_start, self.c.chars.len());
    }

    fn finish(mut self) -> Vec<Inline> {
        self.resolve_emphasis(0);
        coalesce(self.out)
    }
}

// ASCII punctuation, per the spec's own definition -- what a backslash
// may escape, and what counts as punctuation for the flanking rules.
fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

impl InlineParser<'_> {
    // `\*` is a literal asterisk; `\` before a newline is a hard break;
    // `\` before anything else is a literal backslash.
    fn backslash(&mut self, text_start: usize) -> bool {
        match self.at(self.pos + 1) {
            Some('\n') => {
                self.push_text(text_start, self.pos);
                let span = self.c.span(self.pos, self.pos + 2);
                self.out.push(Inline::HardBreak { span });
                self.pos += 2;
                true
            }
            Some(c) if is_punctuation(c) => {
                self.push_text(text_start, self.pos);
                let span = self.c.span(self.pos, self.pos + 2);
                self.out.push(Inline::Text { text: c.to_string(), span });
                self.pos += 2;
                true
            }
            _ => false,
        }
    }

    // A code span is delimited by equal-length backtick runs. Its
    // content has one leading and trailing space stripped when both are
    // present and it isn't all spaces -- the rule that lets `` ` `` hold
    // a literal backtick.
    fn code_span(&mut self, text_start: usize) -> bool {
        let open_len = self.run_len(self.pos, '`');
        let mut i = self.pos + open_len;
        while i < self.c.chars.len() {
            if self.at(i) == Some('`') {
                let len = self.run_len(i, '`');
                if len == open_len {
                    self.push_text(text_start, self.pos);
                    let inner_start = self.pos + open_len;
                    let mut text: String = self.c.text(inner_start, i);
                    // Line endings inside a code span are spaces.
                    text = text.replace('\n', " ");
                    if text.len() >= 2
                        && text.starts_with(' ')
                        && text.ends_with(' ')
                        && !text.chars().all(|c| c == ' ')
                    {
                        text = text[1..text.len() - 1].to_string();
                    }
                    let span = self.c.span(self.pos, i + len);
                    self.out.push(Inline::Code { text, span });
                    self.pos = i + len;
                    return true;
                }
                i += len;
            } else {
                i += 1;
            }
        }
        // No closer: the run is literal text.
        false
    }

    fn run_len(&self, from: usize, c: char) -> usize {
        self.c.chars[from..].iter().take_while(|&&x| x == c).count()
    }

    // A delimiter run's flanking, §6.2: whether it can open or close
    // depends on the characters on both sides, which is why this can't
    // be decided by a scan alone.
    fn delimiter_run(&mut self, text_start: usize) -> bool {
        let c = self.at(self.pos).expect("called only on a delimiter character");
        let count = self.run_len(self.pos, c);
        // GFM strikethrough is one or two tildes; a longer run is text.
        if c == '~' && count > 2 {
            return false;
        }
        let before = if self.pos == 0 { ' ' } else { self.c.chars[self.pos - 1] };
        let after = self.at(self.pos + count).unwrap_or(' ');
        let left_flanking = !after.is_whitespace()
            && (!is_punctuation(after) || before.is_whitespace() || is_punctuation(before));
        let right_flanking = !before.is_whitespace()
            && (!is_punctuation(before) || after.is_whitespace() || is_punctuation(after));
        // `_` is stricter than `*`: it can't open or close inside a
        // word, so `snake_case_names` stays one word.
        let (can_open, can_close) = match c {
            '_' => (
                left_flanking && (!right_flanking || is_punctuation(before)),
                right_flanking && (!left_flanking || is_punctuation(after)),
            ),
            _ => (left_flanking, right_flanking),
        };

        self.push_text(text_start, self.pos);
        let span = self.c.span(self.pos, self.pos + count);
        let text = self.c.text(self.pos, self.pos + count);
        self.out.push(Inline::Text { text, span });
        self.delims.push(Delimiter { c, node: self.out.len() - 1, count, can_open, can_close, active: true });
        self.pos += count;
        true
    }

    fn open_bracket(&mut self, text_start: usize, image: bool) -> bool {
        self.push_text(text_start, self.pos);
        let len = if image { 2 } else { 1 };
        let span = self.c.span(self.pos, self.pos + len);
        let text = self.c.text(self.pos, self.pos + len);
        self.out.push(Inline::Text { text, span });
        self.brackets.push(Bracket { node: self.out.len() - 1, pos: self.pos, image, active: true });
        self.pos += len;
        true
    }

    // `]` -- the point where a link may turn out to exist, which is why
    // brackets are stacked rather than matched as they're seen.
    fn close_bracket(&mut self, text_start: usize) -> bool {
        let Some(bracket) = self.brackets.pop() else { return false };
        if !bracket.active {
            return false;
        }
        self.push_text(text_start, self.pos);
        let label_end = self.pos;
        let after = self.pos + 1;

        let resolved = self
            .inline_destination(after)
            .or_else(|| self.reference_destination(bracket.pos, label_end, after));
        let Some((dest, title, end)) = resolved else {
            // Not a link after all: both brackets stay literal text.
            let span = self.c.span(self.pos, self.pos + 1);
            self.out.push(Inline::Text { text: "]".to_string(), span });
            self.pos += 1;
            return true;
        };

        // Everything after the opening bracket's own node is the link's
        // content, with emphasis inside it resolved first.
        self.resolve_emphasis(bracket.node + 1);
        let content: Vec<Inline> = self.out.drain(bracket.node + 1..).collect();
        self.out.truncate(bracket.node);
        self.delims.retain(|d| d.node < bracket.node);
        let span = self.c.span(bracket.pos, end);
        if bracket.image {
            self.out.push(Inline::Image { dest, title, alt: content, span });
        } else {
            // Links may not contain links, so any `[` still open is
            // deactivated -- the rule that stops `[a[b](c)](d)` nesting.
            for b in self.brackets.iter_mut() {
                if !b.image {
                    b.active = false;
                }
            }
            self.out.push(Inline::Link { dest, title, content, span });
        }
        self.pos = end;
        true
    }

    // `](/dest "title")`, returning where the closing paren ended.
    fn inline_destination(&self, after: usize) -> Option<(String, String, usize)> {
        if self.at(after) != Some('(') {
            return None;
        }
        let mut i = after + 1;
        i = self.skip_ws(i);
        let (dest, next) = self.link_destination(i)?;
        i = self.skip_ws(next);
        let mut title = String::new();
        if matches!(self.at(i), Some('"') | Some('\'') | Some('(')) {
            let (t, next) = self.link_title(i)?;
            title = t;
            i = self.skip_ws(next);
        }
        if self.at(i) != Some(')') {
            return None;
        }
        Some((dest, title, i + 1))
    }

    // `[text][label]`, `[text][]` and `[text]` -- all three resolve
    // against the document's link reference definitions.
    fn reference_destination(&self, open: usize, label_end: usize, after: usize) -> Option<(String, String, usize)> {
        let (label, end) = match self.at(after) {
            Some('[') => match self.matching_bracket(after) {
                Some(close) if close == after + 1 => {
                    // `[text][]` -- the text is the label.
                    (self.c.text(open + 1, label_end), close + 1)
                }
                Some(close) => (self.c.text(after + 1, close), close + 1),
                None => return None,
            },
            // A shortcut reference: `[text]` on its own.
            _ => (self.c.text(open + 1, label_end), after),
        };
        let normalized = normalize_label(&label);
        let found = self.refs.iter().find(|r| r.label == normalized)?;
        Some((found.dest.clone(), found.title.clone(), end))
    }

    fn matching_bracket(&self, open: usize) -> Option<usize> {
        let mut i = open + 1;
        while i < self.c.chars.len() {
            match self.at(i) {
                Some('\\') => i += 2,
                Some(']') => return Some(i),
                Some('[') => return None,
                _ => i += 1,
            }
        }
        None
    }

    fn skip_ws(&self, mut i: usize) -> usize {
        while self.at(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        i
    }

    // A destination is either `<...>` or a run of non-space characters
    // with balanced parentheses.
    fn link_destination(&self, start: usize) -> Option<(String, usize)> {
        if self.at(start) == Some('<') {
            let mut i = start + 1;
            let mut out = String::new();
            while let Some(c) = self.at(i) {
                match c {
                    '>' => return Some((out, i + 1)),
                    '\n' | '<' => return None,
                    '\\' if self.at(i + 1).is_some_and(is_punctuation) => {
                        out.push(self.at(i + 1)?);
                        i += 2;
                    }
                    _ => {
                        out.push(c);
                        i += 1;
                    }
                }
            }
            return None;
        }
        let mut i = start;
        let mut depth = 0i32;
        let mut out = String::new();
        while let Some(c) = self.at(i) {
            match c {
                _ if c.is_whitespace() || c.is_control() => break,
                '\\' if self.at(i + 1).is_some_and(is_punctuation) => {
                    out.push(self.at(i + 1)?);
                    i += 2;
                    continue;
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                }
                _ => {}
            }
            out.push(c);
            i += 1;
        }
        if depth != 0 {
            return None;
        }
        Some((out, i))
    }

    fn link_title(&self, start: usize) -> Option<(String, usize)> {
        let open = self.at(start)?;
        let close = match open {
            '"' => '"',
            '\'' => '\'',
            '(' => ')',
            _ => return None,
        };
        let mut i = start + 1;
        let mut out = String::new();
        while let Some(c) = self.at(i) {
            if c == close {
                return Some((out, i + 1));
            }
            if c == '\\' && self.at(i + 1).is_some_and(is_punctuation) {
                out.push(self.at(i + 1)?);
                i += 2;
                continue;
            }
            out.push(c);
            i += 1;
        }
        None
    }

    // A newline inside a paragraph: two trailing spaces (or a backslash,
    // handled above) make it a hard break, anything else a soft one.
    fn line_break(&mut self, text_start: usize) -> bool {
        let mut end = self.pos;
        let mut spaces = 0;
        while end > text_start && self.at(end - 1) == Some(' ') {
            end -= 1;
            spaces += 1;
        }
        self.push_text(text_start, end);
        let span = self.c.span(self.pos, self.pos + 1);
        if spaces >= 2 {
            self.out.push(Inline::HardBreak { span });
        } else {
            self.out.push(Inline::SoftBreak { span });
        }
        self.pos += 1;
        // Leading whitespace on the next line is not content.
        while self.at(self.pos).is_some_and(|c| c == ' ' || c == '\t') {
            self.pos += 1;
        }
        true
    }

    fn entity(&mut self, text_start: usize) -> bool {
        let rest: String = self.c.chars[self.pos..].iter().take(34).collect();
        let Some((text, len)) = decode_entity(&rest) else { return false };
        self.push_text(text_start, self.pos);
        let span = self.c.span(self.pos, self.pos + len);
        self.out.push(Inline::Text { text, span });
        self.pos += len;
        true
    }
}

// `&amp;`, `&#65;`, `&#x41;` -- resolved through the HTML parser's own
// table, so markdown and HTML can't disagree about what an entity means.
fn decode_entity(rest: &str) -> Option<(String, usize)> {
    let chars: Vec<char> = rest.chars().collect();
    if chars.first() != Some(&'&') {
        return None;
    }
    if chars.get(1) == Some(&'#') {
        let (radix, from) = match chars.get(2) {
            Some('x') | Some('X') => (16, 3),
            _ => (10, 2),
        };
        let digits: String = chars[from..].iter().take_while(|c| c.is_digit(radix)).collect();
        if digits.is_empty() || chars.get(from + digits.len()) != Some(&';') {
            return None;
        }
        let code = u32::from_str_radix(&digits, radix).ok()?;
        let c = if code == 0 { '\u{FFFD}' } else { char::from_u32(code).unwrap_or('\u{FFFD}') };
        return Some((c.to_string(), from + digits.len() + 1));
    }
    let name: String = chars[1..].iter().take_while(|c| c.is_ascii_alphanumeric()).collect();
    let terminated = chars.get(1 + name.chars().count()) == Some(&';');
    if !terminated {
        return None;
    }
    let key = format!("{name};");
    let i = crate::html::entities::NAMED.binary_search_by(|(n, _)| n.cmp(&key.as_str())).ok()?;
    Some((crate::html::entities::NAMED[i].1.to_string(), key.chars().count() + 1))
}

// §4.7: a link label is matched case-insensitively with runs of
// whitespace collapsed, so `[FOO   bar]` and `[foo bar]` are one label.
pub fn normalize_label(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

impl InlineParser<'_> {
    // `<https://x>`, `<a@b.c>`, and raw inline HTML -- three things that
    // all start with `<`, distinguished by what follows.
    fn autolink_or_html(&mut self, text_start: usize) -> bool {
        if let Some((dest, end, is_email)) = self.autolink() {
            self.push_text(text_start, self.pos);
            let span = self.c.span(self.pos, end);
            let text = self.c.text(self.pos + 1, end - 1);
            let href = if is_email { format!("mailto:{dest}") } else { dest };
            self.out.push(Inline::Link {
                dest: href,
                title: String::new(),
                content: vec![Inline::Text { text, span: self.c.span(self.pos + 1, end - 1) }],
                span,
            });
            self.pos = end;
            return true;
        }
        if let Some(end) = self.raw_html() {
            self.push_text(text_start, self.pos);
            let raw = self.c.text(self.pos, end);
            let span = self.c.span(self.pos, end);
            self.out.push(Inline::Html { raw, span });
            self.pos = end;
            return true;
        }
        false
    }

    // An absolute URI or an email address between angle brackets --
    // §6.5's own grammar, which is deliberately narrow (no spaces, a
    // real scheme) so that `<not a link>` isn't one.
    fn autolink(&self) -> Option<(String, usize, bool)> {
        let close = self.c.chars[self.pos..].iter().position(|&c| c == '>')? + self.pos;
        let inner = self.c.text(self.pos + 1, close);
        if inner.is_empty() || inner.chars().any(|c| c.is_whitespace() || c == '<') {
            return None;
        }
        if let Some((scheme, _)) = inner.split_once(':')
            && (2..=32).contains(&scheme.len())
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        {
            return Some((inner, close + 1, false));
        }
        if is_email(&inner) {
            return Some((inner, close + 1, true));
        }
        None
    }

    // Inline raw HTML: an open tag, close tag, comment, processing
    // instruction, declaration or CDATA section. Recognized by the same
    // grammar the HTML tokenizer uses, so what markdown calls HTML and
    // what crate::html parses are the same thing.
    fn raw_html(&self) -> Option<usize> {
        let rest: String = self.c.chars[self.pos..].iter().collect();
        html_span(&rest).map(|len| self.pos + len)
    }
}

fn is_email(s: &str) -> bool {
    let Some((user, domain)) = s.split_once('@') else { return false };
    if user.is_empty() || domain.is_empty() {
        return false;
    }
    let user_ok = user.chars().all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~-.".contains(c));
    let domain_ok = domain.split('.').all(|part| {
        !part.is_empty()
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !part.starts_with('-')
            && !part.ends_with('-')
    });
    user_ok && domain_ok && domain.contains('.')
}

// How many characters of `s` are one HTML tag/comment/declaration, or
// `None` if it doesn't start with one. Shared by inline parsing and
// block.rs's HTML-block condition 7.
pub fn html_span(s: &str) -> Option<usize> {
    let chars: Vec<char> = s.chars().collect();
    if chars.first() != Some(&'<') {
        return None;
    }
    let starts = |p: &str| s.starts_with(p);
    if starts("<!--") {
        // The spec's own restrictions: the text may not start with `>`
        // or `->`, and may not contain `--`.
        let body_start = 4;
        let mut i = body_start;
        while i + 2 < chars.len() {
            if chars[i] == '-' && chars[i + 1] == '-' && chars[i + 2] == '>' {
                let body: String = chars[body_start..i].iter().collect();
                if body.starts_with('>') || body.starts_with("->") || body.contains("--") || body.ends_with('-') {
                    return None;
                }
                return Some(i + 3);
            }
            i += 1;
        }
        return None;
    }
    if starts("<?") {
        return chars.windows(2).position(|w| w == ['?', '>']).map(|i| i + 2);
    }
    if starts("<![CDATA[") {
        return chars.windows(3).position(|w| w == [']', ']', '>']).map(|i| i + 3);
    }
    if starts("<!") && chars.get(2).is_some_and(|c| c.is_ascii_alphabetic()) {
        return chars.iter().position(|&c| c == '>').map(|i| i + 1);
    }
    let mut i = 1;
    let closing = chars.get(1) == Some(&'/');
    if closing {
        i = 2;
    }
    if !chars.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    while chars.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-') {
        i += 1;
    }
    if closing {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        return (chars.get(i) == Some(&'>')).then_some(i + 1);
    }
    loop {
        let ws_start = i;
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        match chars.get(i) {
            Some('>') => return Some(i + 1),
            Some('/') if chars.get(i + 1) == Some(&'>') => return Some(i + 2),
            Some(c) if c.is_ascii_alphabetic() || matches!(c, '_' | ':') => {
                // An attribute has to be preceded by whitespace.
                if i == ws_start {
                    return None;
                }
            }
            _ => return None,
        }
        while chars.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '.' | '-')) {
            i += 1;
        }
        let before_eq = i;
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        if chars.get(i) != Some(&'=') {
            i = before_eq;
            continue;
        }
        i += 1;
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        match chars.get(i) {
            Some('"') | Some('\'') => {
                let quote = chars[i];
                i += 1;
                let close = chars[i..].iter().position(|&c| c == quote)? + i;
                i = close + 1;
            }
            Some(c) if !c.is_whitespace() && !matches!(c, '"' | '\'' | '=' | '<' | '>' | '`') => {
                while chars
                    .get(i)
                    .is_some_and(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | '=' | '<' | '>' | '`'))
                {
                    i += 1;
                }
            }
            _ => return None,
        }
    }
}

// HTML block condition 7: a complete open or close tag, with nothing but
// whitespace after it on the line.
pub fn is_complete_tag_line(line: &str) -> bool {
    match html_span(line) {
        Some(len) => line.chars().skip(len).all(|c| c.is_whitespace()),
        None => false,
    }
}

impl InlineParser<'_> {
    // §6.2's "process emphasis": walk the delimiter stack forward
    // looking for a closer, then back from it for the nearest opener
    // that may pair with it. The "rule of three" (a run that can both
    // open and close only pairs when the lengths don't sum to a multiple
    // of three) is what makes `*a**b*` come out the way it does.
    fn resolve_emphasis(&mut self, floor: usize) {
        let mut closer_idx = 0;
        while closer_idx < self.delims.len() {
            let closer = self.delims[closer_idx].clone();
            if !closer.can_close || !closer.active || closer.node < floor {
                closer_idx += 1;
                continue;
            }
            let mut opener_idx = None;
            for i in (0..closer_idx).rev() {
                let d = &self.delims[i];
                if !d.active || d.node < floor || !d.can_open || d.c != closer.c {
                    continue;
                }
                let odd = (d.can_close || closer.can_open)
                    && (d.count + closer.count).is_multiple_of(3)
                    && !(d.count.is_multiple_of(3) && closer.count.is_multiple_of(3));
                if odd {
                    continue;
                }
                opener_idx = Some(i);
                break;
            }
            let Some(opener_idx) = opener_idx else {
                // No opener: this run can never close anything, so stop
                // reconsidering it.
                self.delims[closer_idx].can_close = false;
                closer_idx += 1;
                continue;
            };
            let taken = if closer.c == '~' {
                // GFM strikethrough is all-or-nothing: `~~` pairs with
                // `~~`, `~` with `~`.
                if self.delims[opener_idx].count != closer.count {
                    self.delims[closer_idx].can_close = false;
                    closer_idx += 1;
                    continue;
                }
                closer.count
            } else if self.delims[opener_idx].count >= 2 && closer.count >= 2 {
                2
            } else {
                1
            };
            self.wrap_emphasis(opener_idx, closer_idx, taken);
            // Everything between the two is now inside the new node.
            self.delims.retain(|d| d.active);
            // Both ends may have shrunk or vanished, so the search
            // starts over rather than trying to keep an index valid
            // across a splice.
            closer_idx = 0;
        }
        // Whatever is left never paired: its text stays as it is.
        self.delims.retain(|d| d.node < floor);
    }

    fn wrap_emphasis(&mut self, opener_idx: usize, closer_idx: usize, taken: usize) {
        let opener = self.delims[opener_idx].clone();
        let closer = self.delims[closer_idx].clone();

        // Shorten both runs' own text nodes by what this pair consumed.
        // The content sits between the *consumed* delimiters, which are
        // the innermost ones -- so it starts where the opener run ended
        // and ends where the closer run began, both read before either
        // is shortened.
        let (open_span_end, close_span_start) = {
            let open_span_end = match &mut self.out[opener.node] {
                Inline::Text { text, span } => {
                    let end = span.end;
                    text.truncate(text.len() - taken);
                    span.end -= taken;
                    end
                }
                _ => 0,
            };
            let close_span_start = match &mut self.out[closer.node] {
                Inline::Text { text, span } => {
                    let start = span.start;
                    *text = text[taken..].to_string();
                    span.start += taken;
                    start
                }
                _ => 0,
            };
            (open_span_end, close_span_start)
        };

        let content: Vec<Inline> = self.out[opener.node + 1..closer.node].to_vec();
        let span = open_span_end..close_span_start;
        let node = match (closer.c, taken) {
            ('~', _) => Inline::Strikethrough { content, span },
            (_, 2) => Inline::Strong { content, span },
            _ => Inline::Emph { content, span },
        };
        // Replace the range between the delimiters with the new node.
        self.out.splice(opener.node + 1..closer.node, [node]);

        // Every delimiter between the two is gone, and every node after
        // the splice moves by however much the splice changed the
        // length -- which is *negative* when the two runs were adjacent
        // and the new node made the vector longer.
        let replaced = closer.node - opener.node - 1;
        let shift = replaced as isize - 1;
        for d in self.delims.iter_mut() {
            if d.node > opener.node && d.node < closer.node {
                d.active = false;
            } else if d.node >= closer.node {
                d.node = (d.node as isize - shift) as usize;
            }
        }
        self.delims[opener_idx].count -= taken;
        self.delims[closer_idx].count -= taken;
        if self.delims[opener_idx].count == 0 {
            self.delims[opener_idx].active = false;
            let node = self.delims[opener_idx].node;
            self.remove_empty_text(node);
        }
        let closer_node = self.delims[closer_idx].node;
        if self.delims[closer_idx].count == 0 {
            self.delims[closer_idx].active = false;
            self.remove_empty_text(closer_node);
        }
    }

    fn remove_empty_text(&mut self, node: usize) {
        if matches!(self.out.get(node), Some(Inline::Text { text, .. }) if text.is_empty()) {
            self.out.remove(node);
            for d in self.delims.iter_mut() {
                if d.node > node {
                    d.node -= 1;
                }
            }
        }
    }
}

// Adjacent text nodes become one, recursively -- run after the emphasis
// algorithm, which needs them separate (see `push_text`).
fn coalesce(inlines: Vec<Inline>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::with_capacity(inlines.len());
    for inline in inlines {
        let inline = match inline {
            Inline::Emph { content, span } => Inline::Emph { content: coalesce(content), span },
            Inline::Strong { content, span } => Inline::Strong { content: coalesce(content), span },
            Inline::Strikethrough { content, span } => Inline::Strikethrough { content: coalesce(content), span },
            Inline::Link { dest, title, content, span } => {
                Inline::Link { dest, title, content: coalesce(content), span }
            }
            Inline::Image { dest, title, alt, span } => Inline::Image { dest, title, alt: coalesce(alt), span },
            other => other,
        };
        match (out.last_mut(), &inline) {
            (Some(Inline::Text { text, span }), Inline::Text { text: add, span: add_span }) => {
                text.push_str(add);
                span.end = add_span.end;
            }
            _ => out.push(inline),
        }
    }
    // An empty text node left behind by a consumed delimiter is not
    // content.
    out.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
    out
}
