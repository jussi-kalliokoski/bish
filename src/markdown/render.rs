// Rendering a parsed markdown document to a terminal: what `:help` shows,
// what `:preview` shows, and the only part of this feature that has an
// opinion about how things should *look*.
//
// Output is a `Vec<String>`, one styled line each, rather than one big
// string -- both callers scroll, and a line is the unit they scroll by.
// Wrapping is by display width (bishedit::unicode_width), not by
// character count, so a line of CJK or emoji stops in the right place.
//
// HTML is where this feature's whole point shows up. A markdown document
// may contain arbitrary markup; crate::html parses it properly, and this
// then renders the handful of elements that mean something in a terminal
// (emphasis, code, links, line breaks, list items) and quietly drops the
// rest. That is "most of it ignored" by choice rather than by inability:
// the tree is right there, so `<b>x</b>` can be bold instead of literal
// angle brackets, and a `<script>` can be dropped whole instead of
// having its source printed.

use crate::bishedit::unicode_width::str_width;
use crate::html::{self, NodeData};

use super::{Align, Block, Document, Inline, List, Table};

// SGR sequences, matching the palette the rest of the editor already
// uses (bishedit::highlight::default_style's indexed colours) rather
// than inventing a second one.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const HEADING: &str = "\x1b[1;33m";
const CODE: &str = "\x1b[32m";
const LINK: &str = "\x1b[4;36m";
const QUOTE_BAR: &str = "\x1b[2;34m";

pub struct Options {
    pub width: usize,
    // Whether a fenced code block's own language gets syntax
    // highlighted. Off for a plain-text render (a test, a pipe).
    pub highlight_code: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 80, highlight_code: true }
    }
}

pub fn to_lines(doc: &Document, opts: &Options) -> Vec<String> {
    let mut out = Vec::new();
    render_blocks(&doc.blocks, opts, 0, &mut out);
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

// The same thing as one string, for a caller that just wants to print it.
pub fn to_string(doc: &Document, opts: &Options) -> String {
    let mut out = to_lines(doc, opts).join("\n");
    out.push('\n');
    out
}

fn render_blocks(blocks: &[Block], opts: &Options, indent: usize, out: &mut Vec<String>) {
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        render_block(block, opts, indent, out);
    }
}

fn render_block(block: &Block, opts: &Options, indent: usize, out: &mut Vec<String>) {
    let pad = " ".repeat(indent);
    // A floor, so a deeply indented block in a narrow pane still has
    // somewhere to put its text rather than wrapping every word.
    let width = opts.width.saturating_sub(indent).max(8);
    match block {
        Block::Paragraph { content, .. } => {
            for line in wrap(&inline_runs(content), width) {
                out.push(format!("{pad}{line}"));
            }
        }
        Block::Heading { level, content, .. } => {
            // A heading's own level is shown by its prefix rather than
            // by size, since a terminal has only one of those.
            let prefix = match level {
                1 => String::new(),
                _ => "  ".repeat((*level as usize).saturating_sub(2)),
            };
            let text: String = inline_runs(content).iter().map(|r| r.styled()).collect();
            out.push(format!("{pad}{HEADING}{prefix}{text}{RESET}"));
            // A top-level heading is underlined, which is what makes a
            // long help page scannable.
            if *level == 1 {
                let plain: String = content.iter().map(|i| i.text_content()).collect();
                let rule = "\u{2500}".repeat(str_width(&plain).min(width));
                out.push(format!("{pad}{DIM}{rule}{RESET}"));
            }
        }
        Block::CodeBlock { info, literal, .. } => {
            let lang = info.split_whitespace().next().unwrap_or("");
            for line in literal.lines() {
                let body = if opts.highlight_code { highlight_line(line, lang) } else { line.to_string() };
                out.push(format!("{pad}  {body}"));
            }
        }
        Block::HtmlBlock { raw, .. } => {
            let (doc, roots) = html::parse_fragment(raw, "div");
            let mut runs = Vec::new();
            for &root in &roots {
                html_runs(&doc, root, Style::default(), &mut runs);
            }
            for line in wrap(&runs, width) {
                if !line.trim().is_empty() {
                    out.push(format!("{pad}{line}"));
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            let mut inner = Vec::new();
            render_blocks(blocks, &Options { width: width.saturating_sub(2), ..*opts }, 0, &mut inner);
            for line in inner {
                out.push(format!("{pad}{QUOTE_BAR}\u{2502}{RESET} {line}"));
            }
        }
        Block::List(list) => render_list(list, opts, indent, out),
        Block::ThematicBreak { .. } => out.push(format!("{pad}{DIM}{}{RESET}", "\u{2500}".repeat(width))),
        Block::Table(table) => render_table(table, opts, indent, out),
    }
}

fn render_list(list: &List, opts: &Options, indent: usize, out: &mut Vec<String>) {
    for (i, item) in list.items.iter().enumerate() {
        if i > 0 && !list.tight {
            out.push(String::new());
        }
        // The marker's own width is what the item's content indents by,
        // so wrapped lines line up under the first one.
        let marker = match (list.ordered, item.task) {
            (_, Some(done)) => format!("{} ", if done { "[\u{2713}]" } else { "[ ]" }),
            (true, None) => format!("{}. ", list.start + i as u64),
            (false, None) => "\u{2022} ".to_string(),
        };
        let mut inner = Vec::new();
        let inner_opts = Options { width: opts.width.saturating_sub(indent + str_width(&marker)), ..*opts };
        render_blocks(&item.blocks, &inner_opts, 0, &mut inner);
        let pad = " ".repeat(indent);
        let hang = " ".repeat(indent + str_width(&marker));
        for (j, line) in inner.into_iter().enumerate() {
            if j == 0 {
                out.push(format!("{pad}{BOLD}{marker}{RESET}{line}"));
            } else {
                out.push(format!("{hang}{line}"));
            }
        }
    }
}

fn render_table(table: &Table, opts: &Options, indent: usize, out: &mut Vec<String>) {
    let columns = table.align.len();
    let cell = |cells: &[Vec<Inline>], i: usize| -> (String, usize) {
        match cells.get(i) {
            Some(inlines) => {
                let runs = inline_runs(inlines);
                let styled: String = runs.iter().map(|r| r.styled()).collect();
                let width: usize = runs.iter().map(|r| str_width(&r.text)).sum();
                (styled, width)
            }
            None => (String::new(), 0),
        }
    };
    // Every column is as wide as its widest cell, capped so a wide table
    // still fits the pane.
    let mut widths = vec![0usize; columns];
    for (i, width) in widths.iter_mut().enumerate() {
        *width = (*width).max(cell(&table.head, i).1);
        for row in &table.rows {
            *width = (*width).max(cell(row, i).1);
        }
    }
    let available = opts.width.saturating_sub(indent + 3 * columns + 1);
    let total: usize = widths.iter().sum();
    if total > available && total > 0 {
        for w in widths.iter_mut() {
            *w = (*w * available / total).max(3);
        }
    }
    let pad = " ".repeat(indent);
    let rule = |left: &str, mid: &str, right: &str| {
        let mut s = format!("{pad}{DIM}{left}");
        for (i, w) in widths.iter().enumerate() {
            s.push_str(&"\u{2500}".repeat(w + 2));
            s.push_str(if i + 1 == widths.len() { right } else { mid });
        }
        s.push_str(RESET);
        s
    };
    let row_line = |cells: &[Vec<Inline>]| {
        let mut s = format!("{pad}{DIM}\u{2502}{RESET}");
        for (i, column) in widths.iter().enumerate() {
            let (styled, w) = cell(cells, i);
            let space = column.saturating_sub(w);
            let (before, after) = match table.align[i] {
                Align::Right => (space, 0),
                Align::Center => (space / 2, space - space / 2),
                _ => (0, space),
            };
            s.push_str(&format!(" {}{}{} {DIM}\u{2502}{RESET}", " ".repeat(before), styled, " ".repeat(after)));
        }
        s
    };
    out.push(rule("\u{250c}", "\u{252c}", "\u{2510}"));
    out.push(row_line(&table.head));
    out.push(rule("\u{251c}", "\u{253c}", "\u{2524}"));
    for row in &table.rows {
        out.push(row_line(row));
    }
    out.push(rule("\u{2514}", "\u{2534}", "\u{2518}"));
}

// One piece of text plus how to draw it. Wrapping works on these so a
// line break never lands inside a styled run's escape sequence.
#[derive(Debug, Clone, Default, PartialEq)]
struct Style {
    bold: bool,
    italic: bool,
    dim: bool,
    underline: bool,
    strike: bool,
    color: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct Run {
    text: String,
    style: Style,
    // A hard break: the line ends here regardless of width.
    break_after: bool,
}

impl Run {
    fn styled(&self) -> String {
        let s = &self.style;
        if *s == Style::default() {
            return self.text.clone();
        }
        let mut out = String::new();
        if let Some(color) = s.color {
            out.push_str(color);
        }
        if s.bold {
            out.push_str(BOLD);
        }
        if s.italic {
            out.push_str(ITALIC);
        }
        if s.dim {
            out.push_str(DIM);
        }
        if s.underline {
            out.push_str(UNDERLINE);
        }
        if s.strike {
            out.push_str(STRIKE);
        }
        out.push_str(&self.text);
        out.push_str(RESET);
        out
    }
}

fn inline_runs(inlines: &[Inline]) -> Vec<Run> {
    let mut out = Vec::new();
    push_inlines(inlines, Style::default(), &mut out);
    out
}

fn push_inlines(inlines: &[Inline], base: Style, out: &mut Vec<Run>) {
    // Inline HTML arrives as *separate* nodes -- `<b>`, the text, then
    // `</b>` -- because in markdown the text between two tags is
    // markdown, not HTML. So making `<b>bold</b>` actually bold means
    // tracking which tags are open across the sequence, rather than
    // parsing each tag alone (which would find an empty element and
    // nothing to style). This is the stack of what's open, innermost
    // last, each entry the style that applies inside it.
    let mut open: Vec<(String, Style)> = Vec::new();
    // How many `<script>`/`<style>` elements are open: their content is
    // not text and contributes nothing.
    let mut suppress = 0usize;
    for inline in inlines {
        let style = open.last().map(|(_, s)| s.clone()).unwrap_or_else(|| base.clone());
        if let Inline::Html { raw, .. } = inline {
            apply_inline_tag(raw, &style, &mut open, &mut suppress, out);
            continue;
        }
        if suppress > 0 {
            continue;
        }
        match inline {
            Inline::Text { text, .. } => out.push(Run { text: text.clone(), style: style.clone(), break_after: false }),
            Inline::Code { text, .. } => {
                let mut s = style.clone();
                s.color = Some(CODE);
                out.push(Run { text: text.clone(), style: s, break_after: false });
            }
            Inline::Emph { content, .. } => push_inlines(content, Style { italic: true, ..style.clone() }, out),
            Inline::Strong { content, .. } => push_inlines(content, Style { bold: true, ..style.clone() }, out),
            Inline::Strikethrough { content, .. } => {
                push_inlines(content, Style { strike: true, ..style.clone() }, out)
            }
            Inline::Link { dest, content, .. } => {
                let mut s = style.clone();
                s.color = Some(LINK);
                push_inlines(content, s, out);
                // The destination is shown after the text, dimmed --
                // a terminal has no way to click, so hiding it would
                // lose the only useful half.
                let text: String = content.iter().map(|i| i.text_content()).collect();
                if !dest.is_empty() && *dest != text && format!("mailto:{text}") != *dest {
                    out.push(Run {
                        text: format!(" ({dest})"),
                        style: Style { dim: true, ..Style::default() },
                        break_after: false,
                    });
                }
            }
            Inline::Image { dest, alt, .. } => {
                let text: String = alt.iter().map(|i| i.text_content()).collect();
                out.push(Run {
                    text: format!("[image: {}]", if text.is_empty() { dest.clone() } else { text }),
                    style: Style { dim: true, ..style.clone() },
                    break_after: false,
                });
            }
            // Handled above, before the suppression check.
            Inline::Html { .. } => {}
            Inline::SoftBreak { .. } => {
                out.push(Run { text: " ".to_string(), style: Style::default(), break_after: false })
            }
            Inline::HardBreak { .. } => {
                out.push(Run { text: String::new(), style: Style::default(), break_after: true })
            }
        }
    }
}

// A parsed HTML subtree, rendered as far as a terminal can render it.
// The elements below are the ones that mean something here; everything
// else contributes only its text, and the three that mean "not text at
// all" contribute nothing.
fn html_runs(doc: &html::Document, node: html::NodeId, style: Style, out: &mut Vec<Run>) {
    match &doc.node(node).data {
        NodeData::Text(text) => out.push(Run { text: text.clone(), style, break_after: false }),
        NodeData::Comment(_) | NodeData::Doctype { .. } => {}
        NodeData::Document => {
            for &child in doc.children(node) {
                html_runs(doc, child, style.clone(), out);
            }
        }
        NodeData::Element { name, attrs, .. } => {
            let child_style = match name.as_str() {
                // Whatever the document says these mean, they are not
                // text -- printing their source is the one thing a
                // reader definitely doesn't want.
                "script" | "style" | "head" | "template" => return,
                "b" | "strong" => Style { bold: true, ..style },
                "i" | "em" | "cite" | "var" => Style { italic: true, ..style },
                "code" | "kbd" | "samp" | "tt" => Style { color: Some(CODE), ..style },
                "u" | "ins" => Style { underline: true, ..style },
                "s" | "del" | "strike" => Style { strike: true, ..style },
                "small" => Style { dim: true, ..style },
                "a" => Style { underline: true, color: Some(LINK), ..style },
                "br" => {
                    out.push(Run { text: String::new(), style, break_after: true });
                    return;
                }
                "hr" => {
                    out.push(Run { text: "\u{2500}\u{2500}\u{2500}".to_string(), style, break_after: true });
                    return;
                }
                "li" => {
                    out.push(Run { text: "\u{2022} ".to_string(), style: style.clone(), break_after: false });
                    style
                }
                // A block element inside inline content still ends the
                // line it was on, which is the one structural thing a
                // terminal can honour.
                "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if !out.is_empty() {
                        out.push(Run { text: String::new(), style: style.clone(), break_after: true });
                    }
                    style
                }
                _ => style,
            };
            for &child in doc.children(node) {
                html_runs(doc, child, child_style.clone(), out);
            }
            if name == "a"
                && let Some(href) = attrs.iter().find(|a| a.name == "href")
                && !href.value.is_empty()
            {
                out.push(Run {
                    text: format!(" ({})", href.value),
                    style: Style { dim: true, ..Style::default() },
                    break_after: false,
                });
            }
            if matches!(name.as_str(), "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li") {
                out.push(Run { text: String::new(), style: Style::default(), break_after: true });
            }
        }
    }
}

// Greedy wrapping on whitespace, measuring display width rather than
// characters so a wide glyph counts for what it draws. A word longer
// than the whole width is left to overflow rather than broken -- a URL
// or an identifier is more useful whole.
fn wrap(runs: &[Run], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    let mut pending_space = false;
    for run in runs {
        if run.break_after && run.text.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
            pending_space = false;
            continue;
        }
        for (i, word) in run.text.split(' ').enumerate() {
            if i > 0 {
                pending_space = true;
            }
            if word.is_empty() {
                continue;
            }
            let w = str_width(word);
            let space = usize::from(pending_space && used > 0);
            if used > 0 && used + space + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            } else if space > 0 {
                line.push(' ');
                used += 1;
            }
            pending_space = false;
            line.push_str(&Run { text: word.to_string(), style: run.style.clone(), break_after: false }.styled());
            used += w;
        }
        if run.break_after {
            lines.push(std::mem::take(&mut line));
            used = 0;
            pending_space = false;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

// A code block's own line, run through whichever highlighter its info
// string names -- the same highlighters the editor uses, so a bash
// example in `:help` is coloured exactly as it would be in a buffer.
fn highlight_line(line: &str, lang: &str) -> String {
    use crate::bishedit::highlight::{self, BashHighlighter, HighlightContext, Highlighter, JsonHighlighter};
    let spans = match lang {
        "bash" | "sh" | "shell" => BashHighlighter.highlight(line, HighlightContext::default()),
        "json" => JsonHighlighter.highlight(line, HighlightContext::default()),
        _ => return format!("{DIM}{line}{RESET}"),
    };
    let chars: Vec<char> = line.chars().collect();
    let styled: Vec<highlight::StyledSpan> = spans
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, None);
            highlight::StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();
    highlight::render_styled(&highlight::compose(&chars, &[&styled]))
}

// One inline HTML tag's effect on the style stack. Read with the real
// tokenizer rather than by looking for `<` and `>`, so an attribute
// containing either (`<a title="a > b">`) can't confuse it.
fn apply_inline_tag(raw: &str, style: &Style, open: &mut Vec<(String, Style)>, suppress: &mut usize, out: &mut Vec<Run>) {
    use crate::html::tokenizer::{Token, Tokenizer};
    let token = Tokenizer::new(raw).next();
    match token {
        Token::StartTag(tag) => {
            match tag.name.as_str() {
                "br" => {
                    out.push(Run { text: String::new(), style: style.clone(), break_after: true });
                    return;
                }
                "script" | "style" => {
                    *suppress += 1;
                    open.push((tag.name, style.clone()));
                    return;
                }
                _ => {}
            }
            let inner = inline_tag_style(&tag.name, style);
            // A void element styles nothing, and a self-closing one has
            // already closed.
            if !tag.self_closing && !is_void(&tag.name) {
                open.push((tag.name, inner));
            }
        }
        Token::EndTag(tag) => {
            if let Some(pos) = open.iter().rposition(|(n, _)| *n == tag.name) {
                if matches!(tag.name.as_str(), "script" | "style") {
                    *suppress = suppress.saturating_sub(1);
                }
                open.truncate(pos);
            }
        }
        _ => {}
    }
}

fn inline_tag_style(name: &str, style: &Style) -> Style {
    match name {
        "b" | "strong" => Style { bold: true, ..style.clone() },
        "i" | "em" | "cite" | "var" => Style { italic: true, ..style.clone() },
        "code" | "kbd" | "samp" | "tt" => Style { color: Some(CODE), ..style.clone() },
        "u" | "ins" => Style { underline: true, ..style.clone() },
        "s" | "del" | "strike" => Style { strike: true, ..style.clone() },
        "small" => Style { dim: true, ..style.clone() },
        "a" => Style { underline: true, color: Some(LINK), ..style.clone() },
        _ => style.clone(),
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source"
            | "track" | "wbr"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse;

    // Rendered without styling, so a test reads as the text a reader
    // sees rather than as escape sequences. The escapes themselves are
    // checked separately, below.
    fn plain(input: &str, width: usize) -> String {
        let doc = parse(input);
        let lines = to_lines(&doc, &Options { width, highlight_code: false });
        lines.iter().map(|l| format!("{}\n", strip_sgr(l))).collect()
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn assert_render(input: &str, width: usize, expected: &str) {
        let got = plain(input, width);
        let want: String = expected.trim_matches('\n').lines().map(|l| format!("{l}\n")).collect();
        assert_eq!(got, want, "\n--- got ---\n{got}--- want ---\n{want}");
    }

    #[test]
    fn a_heading_is_underlined_and_paragraphs_are_wrapped() {
        assert_render(
            "# Title\n\nsome text that is long enough to need wrapping at this width\n",
            30,
            "\
Title
─────

some text that is long enough
to need wrapping at this width",
        );
    }

    #[test]
    fn lists_hang_their_wrapped_lines_under_the_first() {
        assert_render(
            "- a list item long enough to wrap around\n- short\n",
            24,
            "\
• a list item long
  enough to wrap around
• short",
        );
    }

    #[test]
    fn ordered_lists_and_task_items_get_their_own_markers() {
        assert_render(
            "2. two\n3. three\n",
            40,
            "\
2. two
3. three",
        );
        assert_render(
            "- [x] done\n- [ ] todo\n",
            40,
            "\
[✓] done
[ ] todo",
        );
    }

    #[test]
    fn a_block_quote_gets_a_bar() {
        assert_render(
            "> quoted text\n",
            40,
            "\
│ quoted text",
        );
    }

    #[test]
    fn a_table_is_drawn_with_its_alignment_honoured() {
        assert_render(
            "| a | b |\n|:--|--:|\n| 1 | 2 |\n",
            40,
            "\
┌───┬───┐
│ a │ b │
├───┼───┤
│ 1 │ 2 │
└───┴───┘",
        );
    }

    #[test]
    fn a_link_shows_its_text_and_its_destination() {
        assert_render("[docs](https://example.com)\n", 60, "docs (https://example.com)");
        // ...except an autolink, where the two are the same thing.
        assert_render("<https://example.com>\n", 60, "https://example.com");
    }

    #[test]
    fn a_hard_break_ends_the_line_where_it_was_written() {
        assert_render("one  \ntwo\n", 60, "one\ntwo");
    }

    // The point of parsing the HTML rather than printing it: `<b>` is
    // bold text, not four literal characters.
    #[test]
    fn inline_html_is_rendered_as_what_it_means() {
        assert_render("a <b>bold</b> and <code>code</code> word\n", 60, "a bold and code word");
        let styled = to_lines(&parse("a <b>bold</b> word\n"), &Options { width: 60, highlight_code: false });
        assert!(styled[0].contains("\x1b[1mbold"), "the bold really is bold: {:?}", styled[0]);
    }

    #[test]
    fn a_script_element_contributes_nothing_at_all() {
        // The script contributes nothing -- and the text on either
        // side of it is still one line, because it was one `<div>`.
        assert_render("<div>keep<script>var x = 1;</script>this</div>\n", 60, "keepthis");
    }

    #[test]
    fn an_html_block_renders_its_text_without_the_tags() {
        assert_render("<div class=\"x\">\n  <p>first</p>\n  <p>second</p>\n</div>\n", 40, "first\nsecond");
    }

    #[test]
    fn emphasis_and_code_get_real_escape_sequences() {
        let lines = to_lines(&parse("*em* **strong** `code` ~~gone~~\n"), &Options { width: 60, highlight_code: false });
        let line = &lines[0];
        assert!(line.contains("\x1b[3mem"), "italic: {line:?}");
        assert!(line.contains("\x1b[1mstrong"), "bold: {line:?}");
        assert!(line.contains("code"), "code: {line:?}");
        assert!(line.contains("\x1b[9mgone"), "strikethrough: {line:?}");
    }

    // Wrapping measures what a glyph draws, not how many chars it is.
    #[test]
    fn wrapping_measures_display_width() {
        // 3 + a space + 6 is 10, which fits in 12; adding " bbb" makes
        // 14, which does not. Counting *characters* instead would make
        // the whole line 11 and fit it, so this really does pin the
        // measurement rather than the wrapping.
        assert_render("aaa 日本語 bbb\n", 12, "aaa 日本語\nbbb");
    }

    #[test]
    fn a_code_block_is_indented_and_kept_verbatim() {
        assert_render(
            "```bash\nif true; then\n  echo hi\nfi\n```\n",
            40,
            &["  if true; then", "    echo hi", "  fi"].join("\n"),
        );
    }

    #[test]
    fn an_empty_document_renders_to_nothing() {
        assert_eq!(to_lines(&parse(""), &Options::default()), Vec::<String>::new());
    }
}
