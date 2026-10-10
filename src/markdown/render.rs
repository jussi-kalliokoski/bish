// Renders a parsed markdown document to a terminal, for `:help` and
// `:preview`. Output is one styled line per `String`, the unit both
// callers scroll by. Wrapping goes by display width
// (bishedit::unicode_width), so CJK and emoji wrap correctly.
//
// HTML in a document is parsed by crate::html, and the elements that mean
// something in a terminal (emphasis, code, links, line breaks, list
// items) are rendered: `<b>x</b>` is bold, a `<script>` vanishes; the
// rest contributes its text.

use crate::bishedit::unicode_width::{char_width, str_width};
use crate::html::{self, NodeData};

use super::{Align, Block, Document, Inline, List, Table};

// SGR sequences from the editor's palette
// (`bishedit::highlight::default_style`'s indexed colours).
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
// The four chosen by meaning rather than weight, which a theme can set
// (theme.rs); the constants above are plain attributes.
fn heading(opts: &Options) -> String {
    crate::theme::sgr(crate::theme::Ui::Heading, opts.colors.as_ref())
}

fn code(opts: &Options) -> String {
    crate::theme::sgr(crate::theme::Ui::Code, opts.colors.as_ref())
}

fn link(opts: &Options) -> String {
    crate::theme::sgr(crate::theme::Ui::Link, opts.colors.as_ref())
}

fn quote_bar(opts: &Options) -> String {
    crate::theme::sgr(crate::theme::Ui::Quote, opts.colors.as_ref())
}

#[derive(Debug, Clone)]
pub struct Options {
    pub width: usize,
    // Whether a fenced code block is highlighted in its language; off for
    // plain text (tests, pipes).
    pub highlight_code: bool,
    // Whether link text is an OSC 8 hyperlink; off for plain text and with
    // the `hyperlinks` bishopt off.
    pub hyperlinks: bool,
    // What a relative link is relative to: the document's directory. `None`
    // without a file (`:help`), when relative links get no hyperlink.
    pub base_dir: Option<std::path::PathBuf>,
    // The session's `ui_col_*` colours; `None` renders the defaults.
    pub colors: Option<crate::theme::UiColors>,
    // The `table_wrap` bishopt: whether a table too wide for `width` is
    // fitted by narrowing columns and wrapping cells, or keeps its width
    // and runs off the side for the view to scroll. Prose wraps either way.
    pub wrap_tables: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 80, highlight_code: true, hyperlinks: false, base_dir: None, colors: None, wrap_tables: true }
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

// The same as one string, for printing.
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
    // A floor, so a deeply indented block in a narrow pane has room for
    // more than a word per line.
    let width = opts.width.saturating_sub(indent).max(8);
    match block {
        Block::Paragraph { content, .. } => {
            for line in wrap(&inline_runs(content, opts), width) {
                out.push(format!("{pad}{line}"));
            }
        }
        Block::Heading { level, content, .. } => {
            // A heading's level shows by its prefix, since a terminal has
            // one size.
            let prefix = match level {
                1 => String::new(),
                _ => "  ".repeat((*level as usize).saturating_sub(2)),
            };
            let text: String = inline_runs(content, opts).iter().map(|r| r.styled()).collect();
            out.push(format!("{pad}{}{prefix}{text}{RESET}", heading(opts)));
            // A top-level heading is underlined, which makes long help pages
            // scannable.
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
                html_runs(&doc, root, Style::default(), &mut runs, opts, 0);
            }
            for line in wrap(&runs, width) {
                if !line.trim().is_empty() {
                    out.push(format!("{pad}{line}"));
                }
            }
        }
        Block::BlockQuote { blocks, .. } => {
            let mut inner = Vec::new();
            render_blocks(blocks, &Options { width: width.saturating_sub(2), ..opts.clone() }, 0, &mut inner);
            for line in inner {
                out.push(format!("{pad}{}\u{2502}{RESET} {line}", quote_bar(opts)));
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
        // The marker's width is the item's content indent, so wrapped lines
        // line up.
        let marker = match (list.ordered, item.task) {
            (_, Some(done)) => format!("{} ", if done { "[\u{2713}]" } else { "[ ]" }),
            (true, None) => format!("{}. ", list.start + i as u64),
            (false, None) => "\u{2022} ".to_string(),
        };
        let mut inner = Vec::new();
        let inner_opts = Options { width: opts.width.saturating_sub(indent + str_width(&marker)), ..opts.clone() };
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
    let cell = |cells: &[Vec<Inline>], i: usize| -> Vec<Run> { cells.get(i).map(|inlines| inline_runs(inlines, opts)).unwrap_or_default() };
    let natural = |runs: &[Run]| -> usize { runs.iter().map(|r| str_width(&r.text)).sum() };
    let mut widths = vec![0usize; columns];
    for (i, width) in widths.iter_mut().enumerate() {
        *width = (*width).max(natural(&cell(&table.head, i)));
        for row in &table.rows {
            *width = (*width).max(natural(&cell(row, i)));
        }
    }
    // Each column is as wide as its widest cell, until the table doesn't
    // fit: then each keeps a floor and yields a share of the rest in
    // proportion to what it wanted, so prose yields and numbers aren't
    // squeezed. The floors and shares add up to exactly what's available.
    // With `table_wrap` off, columns keep their width and the table runs
    // off the side. Either way rows and rules agree on a width.
    let available = opts.width.saturating_sub(indent + 3 * columns + 1);
    let total: usize = widths.iter().sum();
    if opts.wrap_tables && total > available {
        const FLOOR: usize = 3;
        let slack = available.saturating_sub(FLOOR * columns);
        let over: usize = widths.iter().map(|w| w.saturating_sub(FLOOR)).sum();
        for w in widths.iter_mut() {
            *w = FLOOR + w.saturating_sub(FLOOR) * slack / over.max(1);
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
    // A row is as tall as its tallest cell; what doesn't fit wraps within
    // its column. With natural widths nothing wraps, one line per row.
    let row_lines = |cells: &[Vec<Inline>]| -> Vec<String> {
        let wrapped: Vec<Vec<(String, usize)>> = widths.iter().enumerate().map(|(i, w)| wrap_runs(&cell(cells, i), *w, true)).collect();
        let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1).max(1);
        (0..height)
            .map(|line| {
                let mut s = format!("{pad}{DIM}\u{2502}{RESET}");
                for (i, column) in widths.iter().enumerate() {
                    let (styled, w) = wrapped[i].get(line).cloned().unwrap_or_default();
                    let space = column.saturating_sub(w);
                    let (before, after) = match table.align[i] {
                        Align::Right => (space, 0),
                        Align::Center => (space / 2, space - space / 2),
                        _ => (0, space),
                    };
                    s.push_str(&format!(" {}{}{} {DIM}\u{2502}{RESET}", " ".repeat(before), styled, " ".repeat(after)));
                }
                s
            })
            .collect()
    };
    out.push(rule("\u{250c}", "\u{252c}", "\u{2510}"));
    out.extend(row_lines(&table.head));
    out.push(rule("\u{251c}", "\u{253c}", "\u{2524}"));
    for row in &table.rows {
        out.extend(row_lines(row));
    }
    out.push(rule("\u{2514}", "\u{2534}", "\u{2518}"));
}

// One piece of text and how to draw it. Wrapping works on these so a
// break never lands inside an escape sequence.
#[derive(Debug, Clone, Default, PartialEq)]
struct Style {
    bold: bool,
    italic: bool,
    dim: bool,
    underline: bool,
    strike: bool,
    // A whole SGR sequence: the four meaningful colours are themeable
    // (theme.rs), so resolved per render.
    color: Option<String>,
    // The run's OSC 8 target, if any. On the style because wrapping splits a
    // run into words by cloning it, so a wrapped link stays a link on both
    // rows.
    link: Option<Link>,
}

// A hyperlink target and the id tying its pieces together: wrapping
// splits `[the site](...)` into runs with separate OSC 8 sequences, and
// a terminal treats adjacent links without an id as separate. The
// link's source offset is the id, unique within a document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    id: usize,
    url: String,
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
        // Outside the SGR codes, so their reset doesn't land between the
        // hyperlink's open and its text.
        let link = s.link.as_ref().filter(|l| crate::url::is_safe(&l.url));
        if let Some(link) = link {
            out.push_str(&format!("\x1b]8;id={};{}\x1b\\", link.id, link.url));
        }
        if let Some(color) = &s.color {
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
        if link.is_some() {
            out.push_str("\x1b]8;;\x1b\\");
        }
        out
    }
}

fn inline_runs(inlines: &[Inline], opts: &Options) -> Vec<Run> {
    let mut out = Vec::new();
    push_inlines(inlines, Style::default(), &mut out, opts);
    out
}

fn push_inlines(inlines: &[Inline], base: Style, out: &mut Vec<Run>, opts: &Options) {
    // Inline HTML arrives as separate nodes (`<b>`, the text, `</b>`), since
    // text between tags is markdown. So which tags are open is tracked
    // across them: this stack, innermost last, each with its style.
    let mut open: Vec<(String, Style)> = Vec::new();
    // How many `<script>`/`<style>` are open: their content isn't text.
    let mut suppress = 0usize;
    for inline in inlines {
        let style = open.last().map(|(_, s)| s.clone()).unwrap_or_else(|| base.clone());
        if let Inline::Html { raw, .. } = inline {
            apply_inline_tag(raw, &style, &mut open, &mut suppress, out, opts);
            continue;
        }
        if suppress > 0 {
            continue;
        }
        match inline {
            Inline::Text { text, .. } => out.push(Run { text: text.clone(), style: style.clone(), break_after: false }),
            Inline::Code { text, .. } => {
                let mut s = style.clone();
                s.color = Some(code(opts));
                out.push(Run { text: text.clone(), style: s, break_after: false });
            }
            Inline::Emph { content, .. } => push_inlines(content, Style { italic: true, ..style.clone() }, out, opts),
            Inline::Strong { content, .. } => push_inlines(content, Style { bold: true, ..style.clone() }, out, opts),
            Inline::Strikethrough { content, .. } => push_inlines(content, Style { strike: true, ..style.clone() }, out, opts),
            Inline::Link { dest, content, span, .. } => {
                let mut s = style.clone();
                s.color = Some(link(opts));
                s.link =
                    opts.hyperlinks.then(|| crate::url::absolute(dest, opts.base_dir.as_deref())).flatten().map(|url| Link { id: span.start, url });
                push_inlines(content, s, out, opts);
                // The destination follows the text, dimmed: not every
                // terminal can click a link, and seeing where it goes is
                // worth a few columns.
                let text: String = content.iter().map(|i| i.text_content()).collect();
                if !dest.is_empty() && *dest != text && format!("mailto:{text}") != *dest {
                    out.push(Run { text: format!(" ({dest})"), style: Style { dim: true, ..Style::default() }, break_after: false });
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
            Inline::SoftBreak { .. } => out.push(Run { text: " ".to_string(), style: Style::default(), break_after: false }),
            Inline::HardBreak { .. } => out.push(Run { text: String::new(), style: Style::default(), break_after: true }),
        }
    }
}

// A parsed HTML subtree, rendered as far as a terminal can: the
// elements below mean something here, the rest give their text, and
// three mean "not text at all".
fn html_runs(doc: &html::Document, node: html::NodeId, style: Style, out: &mut Vec<Run>, opts: &Options, depth: usize) {
    // The HTML tree is an arena built iteratively, so its depth is the
    // document's (fifty thousand `<div>`s is legal). This walk recurses, so
    // the floor is here: past it, a subtree contributes nothing.
    const MAX_DEPTH: usize = 128;
    if depth == MAX_DEPTH {
        return;
    }
    match &doc.node(node).data {
        NodeData::Text(text) => out.push(Run { text: text.clone(), style, break_after: false }),
        NodeData::Comment(_) | NodeData::Doctype { .. } => {}
        NodeData::Document => {
            for &child in doc.children(node) {
                html_runs(doc, child, style.clone(), out, opts, depth + 1);
            }
        }
        NodeData::Element { name, attrs, .. } => {
            let child_style = match name.as_str() {
                // Whatever the document says, these aren't text, and
                // printing their source is what a reader doesn't want.
                "script" | "style" | "head" | "template" => return,
                "b" | "strong" => Style { bold: true, ..style },
                "i" | "em" | "cite" | "var" => Style { italic: true, ..style },
                "code" | "kbd" | "samp" | "tt" => Style { color: Some(code(opts)), ..style },
                "u" | "ins" => Style { underline: true, ..style },
                "s" | "del" | "strike" => Style { strike: true, ..style },
                "small" => Style { dim: true, ..style },
                "a" => Style { underline: true, color: Some(link(opts)), ..style },
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
                // A block element in inline content still ends its line,
                // the one structure a terminal can honour.
                "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    if !out.is_empty() {
                        out.push(Run { text: String::new(), style: style.clone(), break_after: true });
                    }
                    style
                }
                _ => style,
            };
            for &child in doc.children(node) {
                html_runs(doc, child, child_style.clone(), out, opts, depth + 1);
            }
            if name == "a"
                && let Some(href) = attrs.iter().find(|a| a.name == "href")
                && !href.value.is_empty()
            {
                out.push(Run { text: format!(" ({})", href.value), style: Style { dim: true, ..Style::default() }, break_after: false });
            }
            if matches!(name.as_str(), "p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li") {
                out.push(Run { text: String::new(), style: Style::default(), break_after: true });
            }
        }
    }
}

// Greedy wrapping on whitespace by display width, so a wide glyph
// counts for what it draws.
fn wrap(runs: &[Run], width: usize) -> Vec<String> {
    wrap_runs(runs, width, false).into_iter().map(|(line, _)| line).collect()
}

// The same, with each line's width: a table cell is padded to its
// column, and its styled string can't be measured afterwards.
// `break_words` breaks a word wider than the column; in prose such a
// word (a URL) overflows whole.
fn wrap_runs(runs: &[Run], width: usize, break_words: bool) -> Vec<(String, usize)> {
    let mut lines: Vec<(String, usize)> = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    let mut pending_space = false;
    for run in runs {
        if run.break_after && run.text.is_empty() {
            lines.push((std::mem::take(&mut line), used));
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
            for piece in split_to_width(word, width, break_words) {
                let w = str_width(&piece);
                let space = usize::from(pending_space && used > 0);
                if used > 0 && used + space + w > width {
                    lines.push((std::mem::take(&mut line), used));
                    used = 0;
                } else if space > 0 {
                    line.push(' ');
                    used += 1;
                }
                pending_space = false;
                line.push_str(&Run { text: piece, style: run.style.clone(), break_after: false }.styled());
                used += w;
            }
        }
        if run.break_after {
            lines.push((std::mem::take(&mut line), used));
            used = 0;
            pending_space = false;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push((line, used));
    }
    lines
}

// One word in pieces no wider than `width`: the word itself unless
// breaking was asked for and it doesn't fit. By display width.
fn split_to_width(word: &str, width: usize, break_words: bool) -> Vec<String> {
    if !break_words || width == 0 || str_width(word) <= width {
        return vec![word.to_string()];
    }
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut used = 0;
    for c in word.chars() {
        let w = char_width(c);
        if used > 0 && used + w > width {
            out.push(std::mem::take(&mut piece));
            used = 0;
        }
        piece.push(c);
        used += w;
    }
    if !piece.is_empty() {
        out.push(piece);
    }
    out
}

// A code block line through the highlighter its info string names, the
// editor's, so a bash example in `:help` looks as in a buffer.
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

// One inline tag's effect on the style stack, read with the real
// tokenizer, so an attribute containing `>` (`<a title="a > b">`)
// can't confuse it.
fn apply_inline_tag(raw: &str, style: &Style, open: &mut Vec<(String, Style)>, suppress: &mut usize, out: &mut Vec<Run>, opts: &Options) {
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
            let inner = inline_tag_style(&tag.name, style, opts);
            // A void element styles nothing, and a self-closing one already
            // closed.
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

fn inline_tag_style(name: &str, style: &Style, opts: &Options) -> Style {
    match name {
        "b" | "strong" => Style { bold: true, ..style.clone() },
        "i" | "em" | "cite" | "var" => Style { italic: true, ..style.clone() },
        "code" | "kbd" | "samp" | "tt" => Style { color: Some(code(opts)), ..style.clone() },
        "u" | "ins" => Style { underline: true, ..style.clone() },
        "s" | "del" | "strike" => Style { strike: true, ..style.clone() },
        "small" => Style { dim: true, ..style.clone() },
        "a" => Style { underline: true, color: Some(link(opts)), ..style.clone() },
        _ => style.clone(),
    }
}

fn is_void(name: &str) -> bool {
    matches!(name, "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" | "link" | "meta" | "param" | "source" | "track" | "wbr")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::parse;

    // Without styling, so a test reads as the text; escapes are checked
    // separately.
    fn plain(input: &str, width: usize) -> String {
        let doc = parse(input);
        let lines = to_lines(&doc, &Options { width, highlight_code: false, ..Options::default() });
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

    // Every line of a table is its frame's width: the narrowed columns'
    // cells wrap rather than overflow.
    #[test]
    fn a_table_too_wide_for_the_pane_wraps_inside_its_columns() {
        let src =
            "| operation | measured | verdict |\n|---|---:|---|\n| penalty for a bracket test | 2.30 \u{b5}s | slower than bash by a good margin |\n";
        let lines = plain(src, 40);
        let widths: Vec<usize> = lines.lines().map(str_width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "every line squares up with the frame: {widths:?}");
        assert!(widths[0] <= 40, "and fits the pane: {}", widths[0]);
        // Nothing is dropped: the cell wraps, both halves on the page.
        let text: String = lines.lines().collect::<Vec<_>>().join(" ");
        for word in ["penalty", "bracket", "test", "slower", "margin"] {
            assert!(text.contains(word), "{word} survived the wrap:\n{lines}");
        }
    }

    // `table_wrap` off: natural column widths, running off the side, every
    // line still the same width.
    #[test]
    fn a_wide_table_keeps_its_columns_when_table_wrap_is_off() {
        let src =
            "| operation | measured | verdict |\n|---|---:|---|\n| penalty for a bracket test | 2.30 \u{b5}s | slower than bash by a good margin |\n";
        let doc = parse(src);
        let opts = Options { width: 40, highlight_code: false, wrap_tables: false, ..Options::default() };
        let lines: Vec<String> = to_lines(&doc, &opts).iter().map(|l| strip_sgr(l)).collect();
        let widths: Vec<usize> = lines.iter().map(|l| str_width(l)).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "the rows and the rules still agree: {widths:?}");
        assert!(widths[0] > 40, "and the table is wider than the pane, to be scrolled to: {}", widths[0]);
        // Each cell whole on one line.
        assert!(lines.iter().any(|l| l.contains("penalty for a bracket test")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("slower than bash by a good margin")), "{lines:?}");
    }

    // A word with nowhere to wrap stays whole in prose and breaks in a
    // column.
    #[test]
    fn a_word_wider_than_its_column_breaks_in_a_table_and_not_in_prose() {
        let table = plain("| a |\n|---|\n| supercalifragilistic |\n", 20);
        let widths: Vec<usize> = table.lines().map(str_width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        assert!(table.contains("superc"), "{table}");
        assert_eq!(plain("supercalifragilistic\n", 10), "supercalifragilistic\n");
    }

    #[test]
    fn a_link_shows_its_text_and_its_destination() {
        assert_render("[docs](https://example.com)\n", 60, "docs (https://example.com)");
        // Except an autolink, where the two are the same.
        assert_render("<https://example.com>\n", 60, "https://example.com");
    }

    #[test]
    fn a_hard_break_ends_the_line_where_it_was_written() {
        assert_render("one  \ntwo\n", 60, "one\ntwo");
    }

    // `<b>` is bold text, not four literal characters.
    #[test]
    fn inline_html_is_rendered_as_what_it_means() {
        assert_render("a <b>bold</b> and <code>code</code> word\n", 60, "a bold and code word");
        let styled = to_lines(&parse("a <b>bold</b> word\n"), &Options { width: 60, highlight_code: false, ..Options::default() });
        assert!(styled[0].contains("\x1b[1mbold"), "the bold really is bold: {:?}", styled[0]);
    }

    #[test]
    fn a_script_element_contributes_nothing_at_all() {
        // The script contributes nothing, and the text either side is one
        // line, being one `<div>`.
        assert_render("<div>keep<script>var x = 1;</script>this</div>\n", 60, "keepthis");
    }

    #[test]
    fn an_html_block_renders_its_text_without_the_tags() {
        assert_render("<div class=\"x\">\n  <p>first</p>\n  <p>second</p>\n</div>\n", 40, "first\nsecond");
    }

    #[test]
    fn emphasis_and_code_get_real_escape_sequences() {
        let lines = to_lines(&parse("*em* **strong** `code` ~~gone~~\n"), &Options { width: 60, highlight_code: false, ..Options::default() });
        let line = &lines[0];
        assert!(line.contains("\x1b[3mem"), "italic: {line:?}");
        assert!(line.contains("\x1b[1mstrong"), "bold: {line:?}");
        assert!(line.contains("code"), "code: {line:?}");
        assert!(line.contains("\x1b[9mgone"), "strikethrough: {line:?}");
    }

    // Wrapping measures what a glyph draws, not chars.
    #[test]
    fn wrapping_measures_display_width() {
        // 3, a space and 6 is 10, fitting 12; " bbb" makes 14, which doesn't.
        // Counting chars would make the line 11 and fit it, so this pins the
        // measurement.
        assert_render("aaa 日本語 bbb\n", 12, "aaa 日本語\nbbb");
    }

    #[test]
    fn a_code_block_is_indented_and_kept_verbatim() {
        assert_render("```bash\nif true; then\n  echo hi\nfi\n```\n", 40, &["  if true; then", "    echo hi", "  fi"].join("\n"));
    }

    #[test]
    fn an_empty_document_renders_to_nothing() {
        assert_eq!(to_lines(&parse(""), &Options::default()), Vec::<String>::new());
    }

    fn osc8_targets(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("\x1b]8;") {
            rest = &rest[at + 4..];
            rest = rest.split_once(';').map(|(_, url)| url).unwrap_or(rest);
            let end = rest.find("\x1b\\").unwrap_or(rest.len());
            if !rest[..end].is_empty() {
                out.push(rest[..end].to_string());
            }
            rest = &rest[end..];
        }
        out
    }

    #[test]
    fn hyperlinks_are_emitted_for_absolute_and_resolved_relative_destinations() {
        let doc = crate::markdown::parse("See [the site](https://example.com/x) and [the plan](plan.md).");
        let opts = Options {
            width: 200,
            highlight_code: false,
            hyperlinks: true,
            base_dir: Some(std::path::PathBuf::from("/tmp/somewhere")),
            ..Options::default()
        };
        let text = to_lines(&doc, &opts).join("\n");
        let mut targets = osc8_targets(&text);
        targets.dedup();
        assert_eq!(targets, vec!["https://example.com/x", "file:///tmp/somewhere/plan.md"]);
    }

    // Each word of a link becomes its own run and sequence, sharing one id,
    // so the terminal sees one hyperlink.
    #[test]
    fn the_pieces_of_one_link_share_an_id() {
        let doc = crate::markdown::parse("[the site here](https://example.com/x)");
        let opts = Options { width: 200, highlight_code: false, hyperlinks: true, ..Options::default() };
        let text = to_lines(&doc, &opts).join("\n");
        assert_eq!(osc8_targets(&text).len(), 3, "one per word");
        let ids: Vec<&str> = text.match_indices("\x1b]8;id=").map(|(at, _)| &text[at + 8..at + 9]).collect();
        assert!(ids.windows(2).all(|w| w[0] == w[1]), "all three share one id: {ids:?}");
    }

    #[test]
    fn no_hyperlinks_are_emitted_when_they_are_off() {
        let doc = crate::markdown::parse("[a](https://example.com/x)");
        let opts = Options { width: 200, highlight_code: false, hyperlinks: false, ..Options::default() };
        assert!(osc8_targets(&to_lines(&doc, &opts).join("\n")).is_empty());
    }

    // With no directory, a relative destination gets no link; the absolute
    // one beside it does.
    #[test]
    fn a_relative_destination_needs_a_base_directory() {
        let doc = crate::markdown::parse("[a](https://example.com/x) [b](plan.md)");
        let opts = Options { width: 200, highlight_code: false, hyperlinks: true, ..Options::default() };
        assert_eq!(osc8_targets(&to_lines(&doc, &opts).join("\n")), vec!["https://example.com/x"]);
    }
}
