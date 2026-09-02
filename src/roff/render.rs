// Rendering a parsed man page to a terminal: what `:preview` shows for a
// `.1` file, and the same shape markdown/render.rs already has -- one
// styled line per `Vec<String>` entry, wrapped by display width so CJK
// and emoji stop in the right place.
//
// Deliberately close to what `man` itself puts on screen, because that
// is the layout every reader of these pages already knows: headings
// flush left and bold, body indented, a tagged paragraph's tag on its
// own line with the body indented under it.

use crate::bishedit::unicode_width::str_width;

use super::{Block, Document, Inline, Style};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const UNDERLINE: &str = "\x1b[4m";
const HEADING: &str = "\x1b[1;33m";
const MONO: &str = "\x1b[32m";
const LINK: &str = "\x1b[4;36m";

pub struct Options {
    pub width: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 80 }
    }
}

// man's own body indent. Headings sit outside it, which is what makes a
// page scannable by section.
const BODY_INDENT: usize = 3;
const TAG_INDENT: usize = 7;

pub fn to_lines(doc: &Document, opts: &Options) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(header) = &doc.header {
        // The running header man puts at the top: title(section) on the
        // left, the manual name centred, the same on the right.
        let left = format!("{}({})", header.title, header.section);
        let centre = if header.manual.is_empty() { header.source.clone() } else { header.manual.clone() };
        out.push(format!("{BOLD}{}{RESET}", header_line(&left, &centre, opts.width)));
        out.push(String::new());
    }
    for block in &doc.blocks {
        render_block(block, opts, BODY_INDENT, &mut out);
    }
    if !doc.notes.is_empty() {
        out.push(String::new());
        for note in &doc.notes {
            out.push(format!("{DIM}\u{2014} {note}{RESET}"));
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    // Blocks each end with their own separator and headings open with
    // one, so runs of blanks are collapsed here rather than by making
    // every block track what came before it.
    let mut collapsed: Vec<String> = Vec::with_capacity(out.len());
    for line in out {
        if line.trim().is_empty() && collapsed.last().is_some_and(|l| l.trim().is_empty()) {
            continue;
        }
        collapsed.push(line);
    }
    collapsed
}

fn header_line(left: &str, centre: &str, width: usize) -> String {
    let right = left;
    let used = str_width(left) + str_width(centre) + str_width(right);
    if used + 2 > width {
        return left.to_string();
    }
    let slack = width - used;
    let before = slack / 2;
    format!("{left}{}{centre}{}{right}", " ".repeat(before), " ".repeat(slack - before))
}

fn render_block(block: &Block, opts: &Options, indent: usize, out: &mut Vec<String>) {
    match block {
        Block::Heading { level, content, .. } => {
            if !out.is_empty() {
                out.push(String::new());
            }
            // A section heading is flush left; a subsection is indented
            // once -- man's own convention.
            let pad = if *level == 1 { 0 } else { BODY_INDENT };
            let text: String = content.iter().map(styled_of).collect();
            out.push(format!("{}{HEADING}{text}{RESET}", " ".repeat(pad)));
        }
        Block::Paragraph { content, .. } => {
            for line in wrap(content, opts.width.saturating_sub(indent).max(20)) {
                out.push(format!("{}{line}", " ".repeat(indent)));
            }
            out.push(String::new());
        }
        Block::Tagged { tag, blocks, .. } => {
            let tag_text: String = tag.iter().map(styled_of).collect();
            out.push(format!("{}{tag_text}", " ".repeat(indent)));
            for inner in blocks {
                render_block(inner, opts, indent + (TAG_INDENT - BODY_INDENT), out);
            }
            if blocks.is_empty() {
                out.push(String::new());
            }
        }
        Block::Indented { blocks, .. } => {
            for inner in blocks {
                render_block(inner, opts, indent + BODY_INDENT, out);
            }
        }
        Block::Literal { lines, .. } => {
            for line in lines {
                out.push(format!("{}{MONO}{line}{RESET}", " ".repeat(indent)));
            }
            out.push(String::new());
        }
    }
}

fn styled_of(inline: &Inline) -> String {
    match inline {
        Inline::Text { text, style, .. } => paint(text, *style),
        Inline::Break { .. } => String::new(),
        Inline::Link { url, content, .. } => {
            let text: String = content.iter().map(styled_of).collect();
            let plain: String = content.iter().map(|i| i.text_content()).collect();
            if plain.trim().is_empty() { format!("{LINK}{url}{RESET}") } else { format!("{LINK}{text}{RESET}{DIM} ({url}){RESET}") }
        }
    }
}

fn paint(text: &str, style: Style) -> String {
    if style == Style::default() {
        return text.to_string();
    }
    let mut out = String::new();
    if style.mono {
        out.push_str(MONO);
    }
    if style.bold {
        out.push_str(BOLD);
    }
    if style.italic {
        // Underline, not italic: `man` has rendered italic as underline
        // on a terminal for forty years, and a reader of these pages
        // reads underline as "this is a placeholder you substitute".
        out.push_str(UNDERLINE);
    }
    out.push_str(text);
    out.push_str(RESET);
    out
}

// Greedy wrapping on whitespace, measuring display width. A `Break`
// inline ends the line where the page said to.
fn wrap(inlines: &[Inline], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    let mut pending_space = false;
    for inline in inlines {
        if let Inline::Break { .. } = inline {
            lines.push(std::mem::take(&mut line));
            used = 0;
            pending_space = false;
            continue;
        }
        let (text, style) = match inline {
            Inline::Text { text, style, .. } => (text.clone(), *style),
            Inline::Link { .. } => (styled_of(inline), Style::default()),
            Inline::Break { .. } => unreachable!("handled above"),
        };
        let already_styled = matches!(inline, Inline::Link { .. });
        for (i, word) in text.split(' ').enumerate() {
            if i > 0 {
                pending_space = true;
            }
            if word.is_empty() {
                continue;
            }
            let w = if already_styled { visible_width(word) } else { str_width(word) };
            let space = usize::from(pending_space && used > 0);
            if used > 0 && used + space + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            } else if space > 0 {
                line.push(' ');
                used += 1;
            }
            pending_space = false;
            line.push_str(&if already_styled { word.to_string() } else { paint(word, style) });
            used += w;
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn visible_width(s: &str) -> usize {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    str_width(&out)
}
