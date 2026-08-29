// The man(7) macro package: the ~30 macros every Linux man page is
// written in, turned into the document model. Runs on interp's event
// stream, so by the time anything here sees a `.TP`, the page's own
// `.de`/`.ie`/`.ds` scaffolding has already been executed away.
//
// The structure this recovers is the point. A `.TP` is a *tagged
// paragraph* -- the flag and the prose that documents it, explicitly
// paired by the page's own markup. Mining that out of `man`'s rendered
// output means measuring indentation and hoping; here it is simply what
// the page said.

use std::ops::Range;

use super::interp::{self, Event};
use super::lexer::escape_len;
use super::{Block, Document, Header, Inline, Style};

pub fn parse(source: &str) -> Document {
    let (events, notes) = interp::run(source);
    let mut builder = Builder::new();
    builder.notes = notes;
    for event in events {
        builder.event(event);
    }
    builder.finish()
}

// What the current output is being collected into. `.RS`/`.RE` nest
// these, and a `.TP` body is one too.
enum Container {
    Root,
    Indented { span: Range<usize> },
    Tagged { tag: Vec<Inline>, span: Range<usize> },
}

struct Builder {
    header: Option<Header>,
    stack: Vec<(Container, Vec<Block>)>,
    para: Vec<Inline>,
    para_span: Option<Range<usize>>,
    style: Style,
    // `\fP` means "the font before this one", so one step of history is
    // exactly what roff itself keeps.
    prev_style: Style,
    literal: Option<(Vec<String>, Range<usize>)>,
    // `.TP` sets this: the *next* line of content is the tag, not body.
    awaiting_tag: bool,
    // `.SH` with no argument takes its title from the following line.
    awaiting_heading: Option<u8>,
    link: Option<(String, Vec<Inline>, Range<usize>)>,
    notes: Vec<String>,
}

impl Builder {
    fn new() -> Builder {
        Builder {
            header: None,
            stack: vec![(Container::Root, Vec::new())],
            para: Vec::new(),
            para_span: None,
            style: Style::default(),
            prev_style: Style::default(),
            literal: None,
            awaiting_tag: false,
            awaiting_heading: None,
            link: None,
            notes: Vec::new(),
        }
    }

    fn finish(mut self) -> Document {
        self.flush_para();
        self.flush_literal();
        while self.stack.len() > 1 {
            self.close_container();
        }
        let (_, blocks) = self.stack.pop().expect("the root container is never closed");
        Document { header: self.header, blocks, notes: self.notes }
    }

    fn note(&mut self, what: &str) {
        if !self.notes.iter().any(|n| n == what) {
            self.notes.push(what.to_string());
        }
    }

    fn push_block(&mut self, block: Block) {
        self.stack.last_mut().expect("the root container is never closed").1.push(block);
    }

    fn open(&mut self, container: Container) {
        self.stack.push((container, Vec::new()));
    }

    fn close_container(&mut self) {
        if self.stack.len() <= 1 {
            return;
        }
        self.flush_para();
        self.flush_literal();
        let (container, blocks) = self.stack.pop().expect("checked non-empty");
        let block = match container {
            Container::Root => return,
            Container::Indented { span } => Block::Indented { blocks, span },
            Container::Tagged { tag, span } => Block::Tagged { tag, blocks, span },
        };
        // An empty container carries nothing worth a level of indent.
        if !matches!(&block, Block::Indented { blocks, .. } if blocks.is_empty()) {
            self.push_block(block);
        }
    }

    // Closes a `.TP` body, but not an `.RS` -- a new tagged paragraph
    // ends the previous one, while an explicit indent block outlives it.
    fn close_tagged(&mut self) {
        if matches!(self.stack.last(), Some((Container::Tagged { .. }, _))) {
            self.close_container();
        }
    }

    fn flush_para(&mut self) {
        if self.para.is_empty() {
            return;
        }
        let content = std::mem::take(&mut self.para);
        let span = self.para_span.take().unwrap_or(0..0);
        self.push_block(Block::Paragraph { content, span });
    }

    fn flush_literal(&mut self) {
        let Some((lines, span)) = self.literal.take() else { return };
        if lines.iter().any(|l| !l.trim().is_empty()) {
            self.push_block(Block::Literal { lines, span });
        }
    }

    fn extend_span(&mut self, span: &Range<usize>) {
        match &mut self.para_span {
            Some(existing) => existing.end = existing.end.max(span.end),
            None => self.para_span = Some(span.clone()),
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Text { text, span } => self.text(&text, span),
            Event::Request { name, args, span } => self.request(&name, args, span),
        }
    }

    fn text(&mut self, text: &str, span: Range<usize>) {
        if let Some((lines, _)) = &mut self.literal {
            let mut style = self.style;
            let mut prev = self.prev_style;
            // Font escapes still resolve inside a literal block; the
            // *layout* is what is preserved, not the markup.
            let inlines = to_inlines(text, span, &mut style, &mut prev);
            self.style = style;
            self.prev_style = prev;
            lines.push(super::text_of(&inlines));
            return;
        }
        if let Some(level) = self.awaiting_heading.take() {
            let content = self.inlines(text, span.clone());
            self.push_block(Block::Heading { level, content, span });
            return;
        }
        if self.awaiting_tag {
            self.awaiting_tag = false;
            let tag = self.inlines(text, span.clone());
            self.open(Container::Tagged { tag, span });
            return;
        }
        // roff fills, so trailing whitespace on an input line is never
        // content -- and a stripped `\"` comment always leaves some.
        let inlines = self.inlines(text.trim_end(), span.clone());
        if inlines.is_empty() {
            return;
        }
        self.extend_span(&span);
        // Between `.UR` and `.UE` the text is the link's own label.
        if let Some((_, content, _)) = &mut self.link {
            if !content.is_empty() {
                content.push(Inline::Text { text: " ".to_string(), style: Style::default(), span: span.start..span.start });
            }
            content.extend(inlines);
            return;
        }
        // Filling: consecutive input lines are one paragraph, joined by
        // a space, which is roff's own default and why a man page's
        // source line breaks don't survive into its output.
        if !self.para.is_empty() && !matches!(self.para.last(), Some(Inline::Break { .. })) {
            self.para.push(Inline::Text { text: " ".to_string(), style: self.style, span: span.start..span.start });
        }
        self.para.extend(inlines);
    }

    fn inlines(&mut self, text: &str, span: Range<usize>) -> Vec<Inline> {
        let mut style = self.style;
        let mut prev = self.prev_style;
        let out = to_inlines(text, span, &mut style, &mut prev);
        self.style = style;
        self.prev_style = prev;
        out
    }

    fn request(&mut self, name: &str, args: Vec<String>, span: Range<usize>) {
        match name {
            "TH" | "Dt" => {
                if name == "Dt" {
                    self.note("this page uses mdoc macros, which this parser renders only roughly");
                }
                let get = |i: usize| args.get(i).cloned().unwrap_or_default();
                self.header = Some(Header {
                    title: get(0),
                    section: get(1),
                    date: get(2),
                    source: get(3),
                    manual: get(4),
                });
            }
            "Dd" | "Os" => self.note("this page uses mdoc macros, which this parser renders only roughly"),
            "SH" | "SS" | "Sh" | "Ss" => {
                self.flush_para();
                self.flush_literal();
                while self.stack.len() > 1 {
                    self.close_container();
                }
                let level = if name.eq_ignore_ascii_case("sh") { 1 } else { 2 };
                if args.is_empty() {
                    self.awaiting_heading = Some(level);
                    return;
                }
                let content = self.inlines(&args.join(" "), span.clone());
                self.push_block(Block::Heading { level, content, span });
            }
            // Every paragraph break: they differ in indentation, which a
            // terminal at this width has no way to show anyway.
            "PP" | "LP" | "P" | "HP" | "Pp" | "sp" | "br" | "PD" | "ne" | "ti" => {
                if self.literal.is_some() {
                    // A break inside a no-fill block is a blank line,
                    // which is the one thing it can be there.
                    if let Some((lines, _)) = &mut self.literal
                        && (name == "br" || name == "sp")
                    {
                        lines.push(String::new());
                    }
                    return;
                }
                if name == "br" && !self.para.is_empty() {
                    // A break inside a paragraph is a real line break,
                    // not the end of the paragraph.
                    self.para.push(Inline::Break { span });
                    return;
                }
                self.flush_para();
                if matches!(name, "PP" | "LP" | "P" | "HP" | "Pp") {
                    self.close_tagged();
                }
            }
            "TP" | "TQ" => {
                self.flush_para();
                self.flush_literal();
                self.close_tagged();
                self.awaiting_tag = true;
            }
            "IP" => {
                self.flush_para();
                self.flush_literal();
                self.close_tagged();
                // `.IP` carries its own tag as an argument -- or has
                // none, in which case it is just an indented paragraph.
                match args.first() {
                    Some(tag) if !tag.is_empty() => {
                        let tag = self.inlines(tag, span.clone());
                        self.open(Container::Tagged { tag, span });
                    }
                    _ => self.open(Container::Indented { span }),
                }
            }
            "RS" => {
                self.flush_para();
                self.flush_literal();
                self.open(Container::Indented { span });
            }
            "RE" => {
                self.flush_para();
                self.flush_literal();
                // `.RE` closes the indent and any tagged paragraph
                // inside it.
                self.close_tagged();
                if matches!(self.stack.last(), Some((Container::Indented { .. }, _))) {
                    self.close_container();
                }
            }
            // The one-font macros. With no arguments they set the font
            // for the lines that follow instead.
            "B" | "I" | "SM" | "SB" | "R" | "CW" => {
                let style = match name {
                    "B" | "SB" => Style { bold: true, ..Style::default() },
                    "I" => Style { italic: true, ..Style::default() },
                    "CW" => Style { mono: true, ..Style::default() },
                    _ => Style::default(),
                };
                if args.is_empty() {
                    self.prev_style = self.style;
                    self.style = style;
                    return;
                }
                let text = args.join(" ");
                self.add_styled(&text, style, span);
            }
            // The alternating-font macros: `.BR ls (1)` is "ls" bold and
            // "(1)" roman, with no space between them -- which is the
            // whole reason they exist.
            "BI" | "IB" | "BR" | "RB" | "IR" | "RI" => {
                let (a, b) = font_pair(name);
                self.add_alternating(&args, a, b, span);
            }
            "nf" | "Bd" => {
                self.flush_para();
                if self.literal.is_none() {
                    self.literal = Some((Vec::new(), span));
                }
            }
            "fi" | "Ed" => self.flush_literal(),
            "EX" => {
                self.flush_para();
                self.prev_style = self.style;
                self.style = Style { mono: true, ..Style::default() };
                if self.literal.is_none() {
                    self.literal = Some((Vec::new(), span));
                }
            }
            "EE" => {
                self.flush_literal();
                self.style = Style::default();
            }
            "UR" | "MT" => {
                self.flush_para();
                let raw = args.first().cloned().unwrap_or_default();
                let url = if name == "MT" { format!("mailto:{raw}") } else { raw };
                self.link = Some((url, Vec::new(), span));
            }
            "UE" | "ME" => {
                if let Some((url, content, link_span)) = self.link.take() {
                    self.extend_span(&link_span);
                    self.para.push(Inline::Link { url, content, span: link_span });
                }
                // A trailer argument is the punctuation after the link.
                if let Some(trailer) = args.first().filter(|t| !t.is_empty()) {
                    let inlines = self.inlines(trailer, span.clone());
                    self.para.extend(inlines);
                }
            }
            // `.SY`/`.YS` wrap a synopsis; `.OP` is one optional argument.
            "SY" => {
                self.flush_para();
                let text = args.join(" ");
                self.add_styled(&text, Style { bold: true, ..Style::default() }, span);
            }
            "YS" => self.flush_para(),
            "OP" => {
                let bracketed = format!("[{}]", args.join(" "));
                self.add_styled(&bracketed, Style { bold: true, ..Style::default() }, span);
            }
            // Font selection outside a macro.
            "ft" => {
                self.prev_style = self.style;
                self.style = match args.first().map(String::as_str) {
                    Some("B") | Some("3") => Style { bold: true, ..Style::default() },
                    Some("I") | Some("2") => Style { italic: true, ..Style::default() },
                    Some("CW") | Some("C") => Style { mono: true, ..Style::default() },
                    Some("P") => self.prev_style,
                    _ => Style::default(),
                };
            }
            // Layout requests a terminal at one width has nothing to do
            // with.
            "ll" | "in" | "ad" | "na" | "ps" | "vs" | "nh" | "hy" | "ce" | "rj" | "fl" | "ns" | "rs" | "bp"
            | "pl" | "po" | "tl" | "pc" | "nx" | "ex" | "am" | "PU" | "UC" | "DT" | "IX" | "ci" => {}
            other => {
                // An unknown macro's arguments are still text the reader
                // should see -- dropping them would silently lose
                // content from a page using a package this doesn't know.
                if !args.is_empty() {
                    let text = args.join(" ");
                    self.text(&text, span);
                }
                if other.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    self.note(&format!("`.{other}` is not a macro this parser knows"));
                }
            }
        }
    }

    fn add_styled(&mut self, text: &str, style: Style, span: Range<usize>) {
        let mut inline_style = style;
        let mut prev = self.style;
        let inlines = to_inlines(text, span.clone(), &mut inline_style, &mut prev);
        if inlines.is_empty() {
            return;
        }
        self.extend_span(&span);
        self.space_before(&span);
        match &mut self.link {
            Some((_, content, _)) => content.extend(inlines),
            None => self.para.extend(inlines),
        }
    }

    // `.BR a b c` alternates fonts *without* spaces between the
    // arguments, which is how `ls`(1) is written as one unit.
    fn add_alternating(&mut self, args: &[String], first: Style, second: Style, span: Range<usize>) {
        if args.is_empty() {
            return;
        }
        self.extend_span(&span);
        self.space_before(&span);
        let mut out = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            let mut style = if i % 2 == 0 { first } else { second };
            let mut prev = self.style;
            out.extend(to_inlines(arg, span.clone(), &mut style, &mut prev));
        }
        match &mut self.link {
            Some((_, content, _)) => content.extend(out),
            None => self.para.extend(out),
        }
    }

    fn space_before(&mut self, span: &Range<usize>) {
        let target = match &self.link {
            Some((_, content, _)) => content,
            None => &self.para,
        };
        if target.is_empty() || matches!(target.last(), Some(Inline::Break { .. })) {
            return;
        }
        let space = Inline::Text { text: " ".to_string(), style: Style::default(), span: span.start..span.start };
        match &mut self.link {
            Some((_, content, _)) => content.push(space),
            None => self.para.push(space),
        }
    }
}

fn font_pair(name: &str) -> (Style, Style) {
    let bold = Style { bold: true, ..Style::default() };
    let italic = Style { italic: true, ..Style::default() };
    let roman = Style::default();
    match name {
        "BI" => (bold, italic),
        "IB" => (italic, bold),
        "BR" => (bold, roman),
        "RB" => (roman, bold),
        "IR" => (italic, roman),
        _ => (roman, italic),
    }
}

// Text with its escapes resolved into styled runs. `style` and `prev`
// are threaded in and out because a font change made on one line stays
// in effect on the next -- roff's font is a mode, not a span.
pub fn to_inlines(text: &str, span: Range<usize>, style: &mut Style, prev: &mut Style) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Inline> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, out: &mut Vec<Inline>, style: Style| {
        if !buf.is_empty() {
            out.push(Inline::Text { text: std::mem::take(buf), style, span: span.clone() });
        }
    };
    while i < chars.len() {
        if chars[i] != '\\' {
            buf.push(chars[i]);
            i += 1;
            continue;
        }
        let len = escape_len(&chars, i);
        let arg: String = match chars.get(i + 2) {
            Some('(') => chars.iter().skip(i + 3).take(2).collect(),
            Some('[') => chars[i + 3..].iter().take_while(|&&c| c != ']').collect(),
            Some(&c) => c.to_string(),
            None => String::new(),
        };
        match chars.get(i + 1) {
            Some('f') => {
                flush(&mut buf, &mut out, *style);
                let next = match arg.as_str() {
                    "B" | "3" => Style { bold: true, ..Style::default() },
                    "I" | "2" => Style { italic: true, ..Style::default() },
                    "BI" | "4" => Style { bold: true, italic: true, ..Style::default() },
                    "CW" | "C" | "CR" | "CB" | "CI" => Style { mono: true, ..Style::default() },
                    "P" | "" => *prev,
                    _ => Style::default(),
                };
                *prev = *style;
                *style = next;
            }
            // A literal character the page had to escape.
            Some('-') => buf.push('-'),
            Some('e') | Some('\\') => buf.push('\\'),
            Some('.') => buf.push('.'),
            Some('\'') => buf.push('\''),
            Some('`') => buf.push('`'),
            Some('~') | Some(' ') | Some('0') | Some('|') | Some('^') => buf.push(' '),
            Some('t') => buf.push('\t'),
            // Zero-width and hyphenation control: they exist to affect
            // typesetting, and contribute no characters.
            Some('&') | Some('%') | Some(':') | Some('c') | Some('!') | Some('/') | Some(',') => {}
            // A named special character.
            Some('(') | Some('[') => {
                let name: String = match chars.get(i + 1) {
                    Some('(') => chars.iter().skip(i + 2).take(2).collect(),
                    _ => chars[i + 2..].iter().take_while(|&&c| c != ']').collect(),
                };
                buf.push_str(special_char(&name));
            }
            // Size, motion and drawing: presentation with no text.
            // Size, motion, drawing -- and `\X`/`\Y`, which write a
            // control string straight to the output device. Real pages
            // use `\X'tty: link ...'` around every option name; those
            // are hyperlinks for a device that can show them, not text.
            Some('s') | Some('h') | Some('v') | Some('l') | Some('L') | Some('D') | Some('o') | Some('w')
            | Some('z') | Some('k') | Some('x') | Some('X') | Some('Y') | Some('n') | Some('*') => {}
            Some(&c) => buf.push(c),
            None => {}
        }
        i += len.max(1);
    }
    flush(&mut buf, &mut out, *style);
    out
}

// groff's special-character names, limited to the ones man pages
// actually use. An unknown name renders as itself rather than
// disappearing, so a page using something exotic still reads.
fn special_char(name: &str) -> &str {
    match name {
        "bu" => "\u{2022}",
        "em" => "\u{2014}",
        "en" => "\u{2013}",
        "hy" => "-",
        "aq" => "'",
        "dq" => "\"",
        "oq" => "\u{2018}",
        "cq" => "\u{2019}",
        "lq" => "\u{201C}",
        "rq" => "\u{201D}",
        "ga" => "`",
        "ti" => "~",
        "ha" => "^",
        "ci" => "\u{25CB}",
        "co" => "\u{A9}",
        "rg" => "\u{AE}",
        "tm" => "\u{2122}",
        "de" => "\u{B0}",
        "mu" => "\u{D7}",
        "di" => "\u{F7}",
        "+-" => "\u{B1}",
        "<=" => "\u{2264}",
        ">=" => "\u{2265}",
        "!=" => "\u{2260}",
        "==" => "\u{2261}",
        "->" => "\u{2192}",
        "<-" => "\u{2190}",
        "ua" => "\u{2191}",
        "da" => "\u{2193}",
        "lA" => "\u{21D0}",
        "rA" => "\u{21D2}",
        "sc" => "\u{A7}",
        "dg" => "\u{2020}",
        "dd" => "\u{2021}",
        "ps" => "\u{B6}",
        "or" => "|",
        "ba" => "|",
        "br" => "\u{2502}",
        "ru" => "_",
        "ul" => "_",
        "bv" => "|",
        "rn" => "\u{203E}",
        "es" => "\u{2205}",
        "if" => "\u{221E}",
        "pl" => "+",
        "mi" => "-",
        "eq" => "=",
        "ap" => "~",
        "~~" => "\u{2248}",
        "na" => "",
        "12" => "\u{BD}",
        "14" => "\u{BC}",
        "34" => "\u{BE}",
        "lB" => "[",
        "rB" => "]",
        "lC" => "{",
        "rC" => "}",
        "la" => "<",
        "ra" => ">",
        "Fo" => "\u{AB}",
        "Fc" => "\u{BB}",
        "S" => "",
        "nb" => "",
        "ss" => "",
        other => {
            // `\[uXXXX]` names a code point directly.
            if let Some(hex) = other.strip_prefix('u')
                && hex.len() >= 4
                && hex.chars().all(|c| c.is_ascii_hexdigit())
            {
                return UNICODE_NAMES.iter().find(|(n, _)| *n == other).map(|(_, s)| *s).unwrap_or(other);
            }
            other
        }
    }
}

// `\[uXXXX]` for the handful of code points man pages name that way.
// Resolving the general case would need a `char::from_u32` and a
// `&'static str`, which this signature can't return -- so the common
// ones are a table and the rest fall through as their own name.
const UNICODE_NAMES: &[(&str, &str)] = &[
    ("u2018", "\u{2018}"),
    ("u2019", "\u{2019}"),
    ("u201C", "\u{201C}"),
    ("u201D", "\u{201D}"),
    ("u2010", "-"),
    ("u2014", "\u{2014}"),
    ("u2026", "\u{2026}"),
    ("u00A0", " "),
];
