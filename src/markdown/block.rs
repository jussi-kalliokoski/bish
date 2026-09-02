// Block structure, CommonMark §4 plus GFM tables and task list items.
//
// The shape is the spec's own, and the reference implementation's: for
// each line, first walk the open containers checking whether the line
// continues each of them, then look for new block starts, then hand
// whatever is left to the deepest open leaf. Blocks are finalized (and
// their inline content parsed) when they close.
//
// The one thing worth knowing before reading: this cannot be done in a
// single pass over characters, because block structure is decided by
// lines and by lines that come *later* -- `foo\n===` is a heading, and
// nothing about `foo` says so. That is why inline parsing (inline.rs) is
// a separate pass over the text these blocks accumulate.

use std::ops::Range;

use super::inline::{self, Content};
use super::{Align, Block, Document, LinkRef, List, ListItem, Table};

// A source line, remembering where it started so every span this
// produces is an offset into the original document.
struct Line {
    chars: Vec<char>,
    start: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Document,
    BlockQuote,
    List { ordered: bool, start: u64, marker: char, delim: char, tight: bool, saw_blank: bool },
    Item { indent: usize, marker: Range<usize>, task: Option<bool>, saw_blank_child: bool },
    Paragraph,
    // `fenced` false is an indented code block.
    Code { fenced: bool, fence_char: char, fence_len: usize, indent: usize, info: String, info_span: Range<usize> },
    // The seven HTML block conditions (§4.6). The kind decides what ends
    // the block, which is the only reason it has to be remembered.
    Html { condition: u8 },
    Table { align: Vec<Align>, header: Vec<(String, usize)> },
}

// A block whose structure is settled but whose inline content has not
// been parsed yet. That has to wait for the end of the document: a link
// reference definition is visible to the whole document, including to
// text *above* it, so no paragraph can be resolved until every
// definition has been seen.
enum Raw {
    Paragraph { content: Content, span: Range<usize> },
    Heading { level: u8, content: Content, marker: Range<usize>, span: Range<usize> },
    Code { info: String, literal: String, fenced: bool, info_span: Range<usize>, literal_span: Range<usize>, span: Range<usize> },
    Html { raw: String, span: Range<usize> },
    Quote { blocks: Vec<Raw>, span: Range<usize> },
    List { ordered: bool, start: u64, tight: bool, items: Vec<RawItem>, span: Range<usize> },
    ThematicBreak { span: Range<usize> },
    Table { align: Vec<Align>, header: Content, rows: Vec<Content>, span: Range<usize> },
}

struct RawItem {
    blocks: Vec<Raw>,
    task: Option<bool>,
    marker: Range<usize>,
    span: Range<usize>,
}

fn resolve(raw: Raw, refs: &[LinkRef]) -> Block {
    match raw {
        Raw::Paragraph { content, span } => Block::Paragraph { content: inline::parse(&content, refs), span },
        Raw::Heading { level, content, marker, span } => Block::Heading { level, content: inline::parse(&content, refs), marker, span },
        Raw::Code { info, literal, fenced, info_span, literal_span, span } => {
            Block::CodeBlock { info, literal, fenced, info_span, literal_span, span }
        }
        Raw::Html { raw, span } => Block::HtmlBlock { raw, span },
        Raw::Quote { blocks, span } => Block::BlockQuote { blocks: blocks.into_iter().map(|b| resolve(b, refs)).collect(), span },
        Raw::List { ordered, start, tight, items, span } => Block::List(List {
            ordered,
            start,
            tight,
            items: items
                .into_iter()
                .map(|i| ListItem { blocks: i.blocks.into_iter().map(|b| resolve(b, refs)).collect(), task: i.task, marker: i.marker, span: i.span })
                .collect(),
            span,
        }),
        Raw::ThematicBreak { span } => Block::ThematicBreak { span },
        Raw::Table { align, header, rows, span } => {
            let columns = align.len();
            let head = split_row(&header, Some(columns)).iter().map(|c| inline::parse(c, refs)).collect();
            let rows = rows.iter().map(|r| split_row(r, Some(columns)).iter().map(|c| inline::parse(c, refs)).collect()).collect();
            Block::Table(Table { align, head, rows, span })
        }
    }
}

struct Open {
    kind: Kind,
    children: Vec<Raw>,
    // A list's own items, filled in as each Item block closes -- an
    // item isn't a block, so it can't live in `children`.
    items: Vec<RawItem>,
    // Raw lines for a leaf block, each with its own source offset.
    lines: Vec<(String, usize)>,
    start: usize,
    end: usize,
    // Whether this block can still take lines. A closed block is
    // finalized into its parent's children.
    open: bool,
}

impl Open {
    fn new(kind: Kind, start: usize) -> Open {
        Open { kind, children: Vec::new(), items: Vec::new(), lines: Vec::new(), start, end: start, open: true }
    }

    fn accepts_lines(&self) -> bool {
        matches!(self.kind, Kind::Paragraph | Kind::Code { .. } | Kind::Html { .. } | Kind::Table { .. })
    }

    fn is_container(&self) -> bool {
        matches!(self.kind, Kind::Document | Kind::BlockQuote | Kind::List { .. } | Kind::Item { .. })
    }
}

pub fn parse(input: &str) -> Document {
    let mut parser = Parser {
        stack: vec![Open::new(Kind::Document, 0)],
        link_refs: Vec::new(),
        line: Line { chars: Vec::new(), start: 0 },
        offset: 0,
        column: 0,
        next_nonspace: 0,
        next_nonspace_column: 0,
        indent: 0,
        blank: false,
        partial_tab: 0,
        all_matched: true,
        consumed: false,
    };
    for line in split_lines(input) {
        parser.process(line);
    }
    parser.finish()
}

// Splits on '\n', keeping each line's own start offset, with '\r\n'
// normalized away -- the only newline handling the rest of this needs.
fn split_lines(input: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut chars = Vec::new();
    let mut start = 0;
    for (i, c) in input.chars().enumerate() {
        if c == '\n' {
            if chars.last() == Some(&'\r') {
                chars.pop();
            }
            out.push(Line { chars: std::mem::take(&mut chars), start });
            start = i + 1;
        } else {
            chars.push(c);
        }
    }
    if !chars.is_empty() {
        if chars.last() == Some(&'\r') {
            chars.pop();
        }
        out.push(Line { chars, start });
    }
    out
}

struct Parser {
    stack: Vec<Open>,
    link_refs: Vec<LinkRef>,
    line: Line,
    // Where in the line the parser has consumed to, in characters...
    offset: usize,
    // ...and in columns, which differ once a tab is involved.
    column: usize,
    next_nonspace: usize,
    next_nonspace_column: usize,
    indent: usize,
    blank: bool,
    // Columns still owed by a tab that was consumed part-way -- a tab
    // advances to the next multiple of four, so a container that eats
    // two columns of it leaves two behind.
    partial_tab: usize,
    all_matched: bool,
    // Set when a line was entirely a marker -- an opening or closing
    // code fence, a heading, a thematic break -- so phase 3 neither adds
    // it as content nor counts it as the blank line it now looks like.
    consumed: bool,
}

const TAB_STOP: usize = 4;
const CODE_INDENT: usize = 4;

// How deeply containers (block quotes, lists, list items) may nest. A
// list item is its own container inside its list, so a real nested list
// costs two levels per visible level -- 128 is still far past anything
// written on purpose.
const MAX_NESTING: usize = 128;

impl Parser {
    fn process(&mut self, line: Line) {
        self.line = line;
        self.offset = 0;
        self.column = 0;
        self.partial_tab = 0;
        self.all_matched = true;
        self.consumed = false;

        // Phase 1: how far down the open blocks does this line still
        // belong? Two answers matter and they differ: the deepest block
        // that matched, and the deepest *container* that did. A new
        // block always closes the leaf it lands in, even a leaf the line
        // would otherwise have continued.
        let mut last_index = 0;
        let mut container = 0;
        for i in 1..self.stack.len() {
            self.find_next_nonspace();
            if !self.continues(i) {
                self.all_matched = false;
                break;
            }
            last_index = i;
            if self.stack[i].is_container() {
                container = i;
            }
        }

        // Phase 2: new block starts, from the deepest matched container
        // down.
        let mut started_any = false;
        loop {
            self.find_next_nonspace();
            let tip = self.stack.len() - 1;
            // Only a container may hold a new block -- and a paragraph,
            // which some blocks are allowed to interrupt.
            if !self.stack[tip].is_container() && !matches!(self.stack[tip].kind, Kind::Paragraph) {
                break;
            }
            let Some(started) = self.try_start(container, &mut started_any) else { break };
            if !started {
                break;
            }
            if self.consumed {
                break;
            }
        }

        // Phase 3: whatever is left of the line goes to the tip.
        self.find_next_nonspace();
        let line_end = self.line.start + self.line.chars.len();
        let tip = self.stack.len() - 1;
        // Lazy continuation: a paragraph keeps going even on a line that
        // left the block quote or list item it was in, which is what
        // makes `> one\ntwo` one paragraph.
        let lazy = !self.all_matched && !self.blank && !started_any && matches!(self.stack[tip].kind, Kind::Paragraph);
        if !lazy && !started_any && !self.all_matched {
            self.close_to(last_index);
        }
        // A line that was entirely a marker (a fence either way, a
        // heading, a break) is not also content, and is not the blank
        // line it now looks like.
        if self.consumed {
            for open in self.stack.iter_mut() {
                open.end = line_end;
            }
            return;
        }
        if !self.blank {
            for open in self.stack.iter_mut() {
                open.end = line_end;
            }
        }
        let tip = self.stack.len() - 1;
        if self.stack[tip].accepts_lines() {
            let rest = self.rest_of_line();
            let start = self.line.start + self.offset;
            self.stack[tip].lines.push((rest, start));
            self.stack[tip].end = line_end;
            self.check_html_end();
        } else if !self.blank {
            let start = self.line.start + self.offset;
            self.open_leaf(Kind::Paragraph, start);
            let rest = self.rest_of_line();
            let tip = self.stack.len() - 1;
            self.stack[tip].lines.push((rest, start));
            self.stack[tip].end = line_end;
        }
        if self.blank {
            self.note_blank_line();
        }
    }

    // A blank line makes the enclosing list loose, and lets a list item
    // that has content end.
    fn note_blank_line(&mut self) {
        // Only the innermost of each: a blank line after a whole nested
        // list has nothing to say about the list outside it, and a blank
        // line trailing the document must not loosen anything at all
        // (which is why this records rather than acts -- see
        // `push_block` and `start_list_item`).
        if let Some(open) = self.stack.iter_mut().rev().find(|o| matches!(o.kind, Kind::Item { .. }))
            && let Kind::Item { saw_blank_child, .. } = &mut open.kind
        {
            *saw_blank_child = true;
        }
        if let Some(open) = self.stack.iter_mut().rev().find(|o| matches!(o.kind, Kind::List { .. }))
            && let Kind::List { saw_blank, .. } = &mut open.kind
        {
            *saw_blank = true;
        }
    }

    fn rest_of_line(&self) -> String {
        // A tab consumed part-way leaves its remaining columns as
        // spaces, which is what makes indented code inside a list line
        // up the way the source looks.
        let mut out = " ".repeat(self.partial_tab);
        out.extend(self.line.chars[self.offset.min(self.line.chars.len())..].iter());
        out
    }

    fn find_next_nonspace(&mut self) {
        let mut i = self.offset;
        let mut col = self.column;
        while let Some(&c) = self.line.chars.get(i) {
            match c {
                ' ' => {
                    i += 1;
                    col += 1;
                }
                '\t' => {
                    i += 1;
                    col += TAB_STOP - (col % TAB_STOP);
                }
                _ => break,
            }
        }
        self.blank = i >= self.line.chars.len();
        self.next_nonspace = i;
        self.next_nonspace_column = col;
        self.indent = col - self.column;
    }

    fn advance_next_nonspace(&mut self) {
        self.offset = self.next_nonspace;
        self.column = self.next_nonspace_column;
        self.partial_tab = 0;
    }

    // Consumes `count` columns, splitting a tab if it straddles the
    // boundary.
    fn advance_columns(&mut self, count: usize) {
        let mut left = count;
        while left > 0 {
            let Some(&c) = self.line.chars.get(self.offset) else { break };
            if c == '\t' {
                let width = TAB_STOP - (self.column % TAB_STOP);
                if width > left {
                    self.partial_tab = width - left;
                    self.column += left;
                    self.offset += 1;
                    return;
                }
                self.offset += 1;
                self.column += width;
                left -= width;
            } else {
                self.offset += 1;
                self.column += 1;
                left -= 1;
            }
        }
        self.partial_tab = 0;
    }

    fn peek(&self) -> Option<char> {
        self.line.chars.get(self.next_nonspace).copied()
    }
}

// Continuation: whether `index`'s open block still contains this line.
impl Parser {
    fn continues(&mut self, index: usize) -> bool {
        match self.stack[index].kind.clone() {
            Kind::Document => true,
            Kind::BlockQuote => {
                if self.indent > CODE_INDENT - 1 || self.peek() != Some('>') {
                    return false;
                }
                self.advance_next_nonspace();
                self.advance_columns(1);
                // "> " -- one optional space after the marker, which a
                // tab satisfies too.
                if matches!(self.line.chars.get(self.offset), Some(' ') | Some('\t')) {
                    self.advance_columns(1);
                }
                true
            }
            Kind::List { .. } => true,
            Kind::Item { indent, .. } => {
                if self.blank {
                    // A blank line continues an item that already has
                    // content, and ends one that doesn't (`-` alone
                    // followed by a blank line is an empty item).
                    if self.stack[index].children.is_empty() && index + 1 >= self.stack.len() {
                        return false;
                    }
                    self.advance_next_nonspace();
                    return true;
                }
                if self.indent >= indent {
                    self.advance_columns(indent);
                    true
                } else {
                    false
                }
            }
            Kind::Paragraph => !self.blank,
            Kind::Code { fenced, fence_char, fence_len, indent, .. } => {
                if !fenced {
                    if self.indent >= CODE_INDENT {
                        self.advance_columns(CODE_INDENT);
                        return true;
                    }
                    return self.blank;
                }
                // A closing fence is at least as long as the opening
                // one, of the same character, and has nothing after it.
                if self.indent < CODE_INDENT && self.peek() == Some(fence_char) {
                    let run = self.run_length(self.next_nonspace, fence_char);
                    let after = self.next_nonspace + run;
                    let tail_blank = self.line.chars[after..].iter().all(|c| c.is_whitespace());
                    if run >= fence_len && tail_blank {
                        // Closed by phase 3, which is the only place
                        // that may change the stack -- this walk is
                        // still iterating over it.
                        self.stack[index].end = self.line.start + self.line.chars.len();
                        self.offset = self.line.chars.len();
                        self.consumed = true;
                        return false;
                    }
                }
                // Up to the fence's own indentation is stripped from
                // each content line, so indented fenced code doesn't
                // gain leading spaces.
                let strip = indent.min(self.indent);
                self.advance_columns(strip);
                true
            }
            Kind::Html { condition } => {
                // Conditions 6 and 7 end at a blank line; 1-5 have
                // already ended themselves by matching their end
                // condition when the line was added.
                if condition >= 6 && self.blank {
                    return false;
                }
                true
            }
            Kind::Table { .. } => !self.blank && self.line.chars.contains(&'|'),
        }
    }

    fn run_length(&self, from: usize, c: char) -> usize {
        self.line.chars[from..].iter().take_while(|&&x| x == c).count()
    }
}

// New block starts, tried in the spec's own order. Returns `Some(true)`
// when something opened, `Some(false)` when nothing did, and `None` when
// the line is blank and there is nothing to try.
impl Parser {
    fn try_start(&mut self, container: usize, started_any: &mut bool) -> Option<bool> {
        if self.blank {
            return Some(false);
        }
        let indented = self.indent >= CODE_INDENT;
        // "Interrupting a paragraph" means one that is actually still
        // open: a paragraph whose own container the line already left is
        // going to close regardless, so nothing interrupts it.
        let in_paragraph = self.all_matched && matches!(self.stack[self.stack.len() - 1].kind, Kind::Paragraph);
        let last_matched = container;

        if !indented && self.peek() == Some('>') && self.can_nest() {
            self.close_unmatched(last_matched, started_any);
            let start = self.line.start + self.next_nonspace;
            self.advance_next_nonspace();
            self.advance_columns(1);
            if matches!(self.line.chars.get(self.offset), Some(' ') | Some('\t')) {
                self.advance_columns(1);
            }
            self.open_leaf(Kind::BlockQuote, start);
            return Some(true);
        }
        if !indented && let Some((level, marker, content, content_start)) = self.atx_heading() {
            self.close_unmatched(last_matched, started_any);
            let start = self.line.start + self.next_nonspace;
            let end = self.line.start + self.line.chars.len();
            let content = Content::from_line(&content, content_start);
            self.push_block(Raw::Heading { level, content, marker, span: start..end });
            self.offset = self.line.chars.len();
            self.consumed = true;
            return Some(true);
        }
        if !indented && let Some((fence_char, fence_len, info, info_span)) = self.fence() {
            self.close_unmatched(last_matched, started_any);
            let start = self.line.start + self.next_nonspace;
            let indent = self.indent;
            self.advance_next_nonspace();
            self.advance_columns(fence_len);
            self.open_leaf(Kind::Code { fenced: true, fence_char, fence_len, indent, info, info_span }, start);
            let tip = self.stack.len() - 1;
            self.stack[tip].end = self.line.start + self.line.chars.len();
            self.offset = self.line.chars.len();
            self.consumed = true;
            return Some(true);
        }
        if !indented && let Some(condition) = self.html_block_start(in_paragraph) {
            self.close_unmatched(last_matched, started_any);
            let start = self.line.start + self.next_nonspace;
            self.open_leaf(Kind::Html { condition }, start);
            return Some(true);
        }
        // A GFM table: the line under a paragraph's last line is a
        // delimiter row with the same number of columns. Checked before
        // the setext underline, since `---` is both -- the pipes are
        // what tell them apart.
        if !indented
            && in_paragraph
            && let Some(align) = delimiter_row(&self.line.chars[self.next_nonspace..])
        {
            let tip = self.stack.len() - 1;
            let header_has_pipe =
                self.stack[tip].lines.last().is_some_and(|(l, _)| l.chars().enumerate().any(|(i, c)| c == '|' && !escaped_at(l, i)));
            let columns_match =
                self.stack[tip].lines.last().map(|(l, o)| split_row(&Content::from_line(l, *o), None).len() == align.len()).unwrap_or(false);
            if header_has_pipe && columns_match {
                // The paragraph is taken *before* closing anything:
                // close_unmatched would close the very block this needs.
                let mut open = self.stack.pop().expect("a paragraph was open");
                *started_any = true;
                self.close_unmatched(last_matched, &mut true);
                let header = open.lines.pop().expect("a paragraph has at least one line");
                let table_start = header.1;
                // Any earlier lines of the paragraph stay a paragraph.
                if !open.lines.is_empty() {
                    let content = Content::from_lines(&open.lines);
                    let consumed = self.take_link_refs(&content);
                    let content = content.slice_from(consumed);
                    if !content.is_blank() {
                        let start = content.source_offset(0).unwrap_or(open.start);
                        let end = open.lines.last().map(|(l, o)| o + l.chars().count()).unwrap_or(start);
                        self.push_block(Raw::Paragraph { content, span: start..end });
                    }
                }
                self.open_leaf(Kind::Table { align, header: vec![header] }, table_start);
                let tip = self.stack.len() - 1;
                self.stack[tip].end = self.line.start + self.line.chars.len();
                self.offset = self.line.chars.len();
                self.consumed = true;
                return Some(true);
            }
        }
        if !indented
            && in_paragraph
            && let Some(level) = self.setext_underline()
        {
            // The paragraph's own accumulated lines become the heading
            // -- taken before closing anything, since close_unmatched
            // would close the very block this needs.
            let popped = self.stack.pop();
            *started_any = true;
            self.close_unmatched(last_matched, &mut true);
            if let Some(open) = popped {
                let marker = (self.line.start + self.next_nonspace)..(self.line.start + self.line.chars.len());
                let content = Content::from_lines(&open.lines);
                let span = open.start..(self.line.start + self.line.chars.len());
                self.push_block(Raw::Heading { level, content, marker, span });
            }
            self.offset = self.line.chars.len();
            self.consumed = true;
            return Some(true);
        }
        if !indented && self.thematic_break() {
            self.close_unmatched(last_matched, started_any);
            let start = self.line.start + self.next_nonspace;
            let end = self.line.start + self.line.chars.len();
            self.push_block(Raw::ThematicBreak { span: start..end });
            self.offset = self.line.chars.len();
            self.consumed = true;
            return Some(true);
        }
        if self.can_nest()
            && let Some(item) = self.list_item_start(in_paragraph)
        {
            self.close_unmatched(last_matched, started_any);
            self.start_list_item(item);
            return Some(true);
        }
        if indented && !in_paragraph {
            self.close_unmatched(last_matched, started_any);
            self.advance_columns(CODE_INDENT);
            let start = self.line.start + self.offset;
            self.open_leaf(
                Kind::Code { fenced: false, fence_char: ' ', fence_len: 0, indent: 0, info: String::new(), info_span: start..start },
                start,
            );
            return Some(true);
        }
        Some(false)
    }

    fn close_unmatched(&mut self, last_matched: usize, started_any: &mut bool) {
        if !*started_any {
            self.close_to(last_matched);
            *started_any = true;
        }
    }

    // `# Heading ###` -- one to six hashes, a space (or end of line),
    // and an optional closing run of hashes that isn't part of the text.
    fn atx_heading(&mut self) -> Option<(u8, Range<usize>, String, usize)> {
        let hashes = self.run_length(self.next_nonspace, '#');
        if hashes == 0 || hashes > 6 {
            return None;
        }
        let after = self.next_nonspace + hashes;
        match self.line.chars.get(after) {
            None | Some(' ') | Some('\t') => {}
            _ => return None,
        }
        let marker = (self.line.start + self.next_nonspace)..(self.line.start + after);
        let mut content: Vec<char> = self.line.chars[after..].to_vec();
        // Strip the optional closing hashes and the space before them.
        let mut end = content.len();
        while end > 0 && (content[end - 1] == ' ' || content[end - 1] == '\t') {
            end -= 1;
        }
        let mut hash_end = end;
        while hash_end > 0 && content[hash_end - 1] == '#' {
            hash_end -= 1;
        }
        if hash_end < end && (hash_end == 0 || content[hash_end - 1] == ' ' || content[hash_end - 1] == '\t') {
            end = hash_end;
            while end > 0 && (content[end - 1] == ' ' || content[end - 1] == '\t') {
                end -= 1;
            }
        }
        content.truncate(end);
        let lead = content.iter().take_while(|c| **c == ' ' || **c == '\t').count();
        let text: String = content[lead..].iter().collect();
        Some((hashes as u8, marker, text, self.line.start + after + lead))
    }

    fn fence(&mut self) -> Option<(char, usize, String, Range<usize>)> {
        let c = self.peek()?;
        if c != '`' && c != '~' {
            return None;
        }
        let len = self.run_length(self.next_nonspace, c);
        if len < 3 {
            return None;
        }
        let info_start = self.next_nonspace + len;
        let raw: String = self.line.chars[info_start..].iter().collect();
        // A backtick fence's info string may not contain a backtick,
        // which is what keeps `` `a` `` from opening a code block.
        if c == '`' && raw.contains('`') {
            return None;
        }
        let lead = raw.len() - raw.trim_start().len();
        let info = raw.trim().to_string();
        let span_start = self.line.start + info_start + lead;
        Some((c, len, info.clone(), span_start..(span_start + info.chars().count())))
    }

    fn setext_underline(&mut self) -> Option<u8> {
        let c = self.peek()?;
        if c != '=' && c != '-' {
            return None;
        }
        let run = self.run_length(self.next_nonspace, c);
        let after = self.next_nonspace + run;
        if !self.line.chars[after..].iter().all(|x| x.is_whitespace()) {
            return None;
        }
        Some(if c == '=' { 1 } else { 2 })
    }

    fn thematic_break(&mut self) -> bool {
        let Some(c) = self.peek() else { return false };
        if !matches!(c, '-' | '_' | '*') {
            return false;
        }
        let mut count = 0;
        for &x in &self.line.chars[self.next_nonspace..] {
            if x == c {
                count += 1;
            } else if !x.is_whitespace() {
                return false;
            }
        }
        count >= 3
    }
}

// What a list item's marker turned out to be.
struct ItemStart {
    ordered: bool,
    number: u64,
    marker_char: char,
    delim: char,
    marker: Range<usize>,
    // How far content is indented from the line's own start, which is
    // what every following line of the item has to match.
    indent: usize,
    task: Option<bool>,
}

impl Parser {
    fn list_item_start(&mut self, in_paragraph: bool) -> Option<ItemStart> {
        if self.indent >= CODE_INDENT {
            return None;
        }
        let c = self.peek()?;
        let marker_start = self.next_nonspace;
        // For an ordered list the marker character is the *delimiter*:
        // `3.` and `4.` are the same list, while `3.` and `3)` are not.
        let (ordered, number, marker_char, delim, marker_len) = if matches!(c, '-' | '+' | '*') {
            (false, 1u64, c, ' ', 1)
        } else if c.is_ascii_digit() {
            let digits: String = self.line.chars[marker_start..].iter().take_while(|c| c.is_ascii_digit()).collect();
            // At most nine digits, per the spec -- past that it's text.
            if digits.len() > 9 {
                return None;
            }
            let delim = *self.line.chars.get(marker_start + digits.len())?;
            if delim != '.' && delim != ')' {
                return None;
            }
            (true, digits.parse().ok()?, delim, delim, digits.len() + 1)
        } else {
            return None;
        };
        let after = marker_start + marker_len;
        match self.line.chars.get(after) {
            None => {}
            Some(' ') | Some('\t') => {}
            _ => return None,
        }
        // A list item may only interrupt a paragraph when it starts with
        // 1 and isn't empty -- otherwise `I need 2. apples` would become
        // a list.
        if in_paragraph {
            let rest_blank = self.line.chars[after..].iter().all(|c| c.is_whitespace());
            if rest_blank || (ordered && number != 1) {
                return None;
            }
        }

        // How far the content is indented: the marker, plus the spaces
        // after it -- but a run of five or more (or a blank rest of
        // line) counts as exactly one, so that the extra spaces belong
        // to an indented code block inside the item instead.
        let mut spaces = 0;
        let mut i = after;
        let mut col = self.next_nonspace_column + marker_len;
        while let Some(&x) = self.line.chars.get(i) {
            let width = match x {
                ' ' => 1,
                '\t' => TAB_STOP - (col % TAB_STOP),
                _ => break,
            };
            spaces += width;
            col += width;
            i += 1;
        }
        let rest_blank = i >= self.line.chars.len();
        let padding = if rest_blank || spaces > CODE_INDENT { 1 } else { spaces };
        let indent = self.indent + marker_len + padding;

        // GFM task list items: `- [x] done`, immediately after the
        // marker.
        let content_at = after + if rest_blank || spaces > CODE_INDENT { 1.min(spaces) } else { spaces };
        let task = match self.line.chars.get(content_at..content_at + 3) {
            Some(['[', mark, ']']) if matches!(mark, ' ' | 'x' | 'X') => {
                let follows_space = matches!(self.line.chars.get(content_at + 3), Some(' ') | Some('\t') | None);
                follows_space.then_some(*mark != ' ')
            }
            _ => None,
        };

        Some(ItemStart { ordered, number, marker_char, delim, marker: (self.line.start + marker_start)..(self.line.start + after), indent, task })
    }

    fn start_list_item(&mut self, item: ItemStart) {
        let start = self.line.start + self.next_nonspace;
        self.advance_next_nonspace();
        self.advance_columns(item.indent - self.indent);

        // A matching list is continued rather than started again: same
        // kind of marker means the same list.
        let mut tip = self.stack.len() - 1;
        let same_list = match &self.stack[tip].kind {
            Kind::List { ordered, marker, delim, .. } => *ordered == item.ordered && *marker == item.marker_char && *delim == item.delim,
            _ => false,
        };
        // A different marker means a different list, so the open one has
        // to close -- otherwise the new list would nest inside it, which
        // is what `- a` followed by `* b` must not produce.
        if !same_list && matches!(self.stack[tip].kind, Kind::List { .. }) {
            self.close_to(tip - 1);
            tip = self.stack.len() - 1;
        }
        if same_list {
            // A blank line between two items is what makes a list loose
            // -- and it only counts once another item actually follows.
            let saw_blank = matches!(&self.stack[tip].kind, Kind::List { saw_blank: true, .. });
            if saw_blank && let Kind::List { tight, .. } = &mut self.stack[tip].kind {
                *tight = false;
            }
        } else {
            self.stack.push(Open::new(
                Kind::List { ordered: item.ordered, start: item.number, marker: item.marker_char, delim: item.delim, tight: true, saw_blank: false },
                start,
            ));
        }
        self.stack.push(Open::new(Kind::Item { indent: item.indent, marker: item.marker.clone(), task: item.task, saw_blank_child: false }, start));
        // The task marker itself is consumed, so it isn't also text.
        if item.task.is_some() {
            self.find_next_nonspace();
            self.advance_next_nonspace();
            self.advance_columns(3);
            if matches!(self.line.chars.get(self.offset), Some(' ') | Some('\t')) {
                self.advance_columns(1);
            }
        }
    }
}

// The seven HTML block conditions, §4.6. The tag lists are the spec's
// own, not a guess at what looks block-like.
const HTML_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

impl Parser {
    fn html_block_start(&mut self, in_paragraph: bool) -> Option<u8> {
        if self.peek() != Some('<') {
            return None;
        }
        let rest: String = self.line.chars[self.next_nonspace..].iter().collect();
        let lower = rest.to_lowercase();
        for tag in ["script", "pre", "style", "textarea"] {
            if lower.starts_with(&format!("<{tag}")) && matches!(lower.chars().nth(tag.len() + 1), None | Some(' ') | Some('\t') | Some('>')) {
                return Some(1);
            }
        }
        if rest.starts_with("<!--") {
            return Some(2);
        }
        if rest.starts_with("<?") {
            return Some(3);
        }
        if rest.starts_with("<!") && rest.chars().nth(2).is_some_and(|c| c.is_ascii_alphabetic()) {
            return Some(4);
        }
        if rest.starts_with("<![CDATA[") {
            return Some(5);
        }
        let after_slash = lower.strip_prefix("</").unwrap_or_else(|| lower.strip_prefix('<').unwrap_or(&lower));
        for tag in HTML_BLOCK_TAGS {
            if let Some(tail) = after_slash.strip_prefix(tag)
                && (tail.is_empty() || tail.starts_with(' ') || tail.starts_with('\t') || tail.starts_with('>') || tail.starts_with("/>"))
            {
                return Some(6);
            }
        }
        // Condition 7: a complete tag alone on the line. It may not
        // interrupt a paragraph, which is what stops `<https://x>` and
        // an inline `<em>` mid-sentence from becoming block HTML.
        if !in_paragraph && inline::is_complete_tag_line(&rest) {
            return Some(7);
        }
        None
    }

    // Conditions 1-5 end on the line their end condition appears on,
    // including the line that opened the block.
    fn check_html_end(&mut self) {
        let tip = self.stack.len() - 1;
        let Kind::Html { condition } = self.stack[tip].kind else { return };
        let Some((last, _)) = self.stack[tip].lines.last() else { return };
        let lower = last.to_lowercase();
        let ends = match condition {
            1 => ["</script>", "</pre>", "</style>", "</textarea>"].iter().any(|e| lower.contains(e)),
            2 => last.contains("-->"),
            3 => last.contains("?>"),
            4 => last.contains('>'),
            5 => last.contains("]]>"),
            _ => false,
        };
        if ends {
            self.close_to(tip - 1);
        }
    }
}

// Closing blocks, and turning them into the AST.
impl Parser {
    // A list holds *items* and nothing else, so any other block closes
    // it first -- and closes an outer list too, for a nested one. Without
    // this a `## heading` after a list would be pushed onto the list's
    // own `children`, which nothing ever reads, and vanish.
    fn close_lists_for_block(&mut self) {
        while self.stack.len() > 1 && matches!(self.stack[self.stack.len() - 1].kind, Kind::List { .. }) {
            let target = self.stack.len() - 2;
            self.close_to(target);
        }
    }

    // Opening any block that is not a list or a list item.
    // Whether another container may open here. The block parser itself
    // is iterative -- containers live on `self.stack` -- but the tree it
    // produces is walked recursively three times over: `resolve` turns
    // `Raw` into `Block`, the renderer descends it, and dropping it
    // descends it again. 50k nested `>` in a file overflowed the stack
    // during `resolve`, which is a crash on *opening a document*, not on
    // running anything.
    //
    // Bounding the depth at the one place depth is created makes all
    // three walks safe at once. Past the limit the markers are simply
    // text, which is what any reader would see in them anyway -- a
    // document nested 128 deep is not one anybody is reading.
    fn can_nest(&self) -> bool {
        self.stack.len() < MAX_NESTING
    }

    fn open_leaf(&mut self, kind: Kind, start: usize) {
        self.close_lists_for_block();
        self.stack.push(Open::new(kind, start));
    }

    fn push_block(&mut self, block: Raw) {
        self.close_lists_for_block();
        let tip = self.stack.len() - 1;
        // A blank line inside a list item, with content on both sides of
        // it, is what makes the enclosing list loose.
        if let Kind::Item { saw_blank_child, .. } = &self.stack[tip].kind
            && *saw_blank_child
            && !self.stack[tip].children.is_empty()
        {
            self.mark_list_loose();
        }
        self.stack[tip].children.push(block);
    }

    fn mark_list_loose(&mut self) {
        for open in self.stack.iter_mut().rev() {
            if let Kind::List { tight, .. } = &mut open.kind {
                *tight = false;
                return;
            }
        }
    }

    fn close_to(&mut self, target: usize) {
        while self.stack.len() > target + 1 {
            let open = self.stack.pop().expect("the document block is never closed");
            // An item becomes a ListItem on its parent list rather than
            // a Block among its siblings.
            if let Kind::Item { marker, task, .. } = open.kind.clone() {
                let item = RawItem { blocks: open.children, task, marker, span: open.start..open.end.max(open.start) };
                let tip = self.stack.len() - 1;
                self.stack[tip].items.push(item);
                continue;
            }
            if let Some(block) = self.finalize(open) {
                let tip = self.stack.len() - 1;
                self.stack[tip].children.push(block);
            }
        }
    }

    fn finish(mut self) -> Document {
        self.close_to(0);
        let root = self.stack.pop().expect("the document block is never closed");
        // The second pass: every link reference definition in the
        // document is known by now, so `[text][ref]` resolves whether
        // the definition came before it or after.
        let blocks = root.children.into_iter().map(|b| resolve(b, &self.link_refs)).collect();
        Document { blocks, link_refs: self.link_refs }
    }

    fn finalize(&mut self, open: Open) -> Option<Raw> {
        let span = open.start..open.end.max(open.start);
        match open.kind {
            Kind::Document => None,
            Kind::BlockQuote => Some(Raw::Quote { blocks: open.children, span }),
            Kind::List { ordered, start, tight, .. } => Some(Raw::List { ordered, start, tight, items: open.items, span }),
            // Closed by `close_to`, which knows where a ListItem goes.
            Kind::Item { .. } => None,
            Kind::Paragraph => {
                let content = Content::from_lines(&open.lines);
                // Link reference definitions come off the front of a
                // paragraph, and a paragraph made of nothing else
                // disappears entirely.
                let consumed = self.take_link_refs(&content);
                let content = content.slice_from(consumed);
                if content.is_blank() {
                    return None;
                }
                let start = content.source_offset(0).unwrap_or(open.start);
                Some(Raw::Paragraph { content, span: start..open.end })
            }
            Kind::Code { fenced, info, info_span, .. } => {
                let mut lines: Vec<String> = open.lines.iter().map(|(l, _)| l.clone()).collect();
                if !fenced {
                    // Trailing blank lines are not part of an indented
                    // code block.
                    while lines.last().is_some_and(|l| l.trim().is_empty()) {
                        lines.pop();
                    }
                }
                let mut literal = lines.join("\n");
                if !literal.is_empty() {
                    literal.push('\n');
                }
                let literal_start = open.lines.first().map(|(_, o)| *o).unwrap_or(open.start);
                let literal_end = open.lines.last().map(|(l, o)| o + l.chars().count()).unwrap_or(literal_start);
                Some(Raw::Code { info, literal, fenced, info_span, literal_span: literal_start..literal_end, span })
            }
            Kind::Html { .. } => {
                let raw = open.lines.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>().join("\n");
                Some(Raw::Html { raw, span })
            }
            Kind::Table { align, header } => {
                let rows = open.lines.iter().filter(|(l, _)| !l.trim().is_empty()).map(|(l, o)| Content::from_line(l, *o)).collect();
                Some(Raw::Table { align, header: Content::from_lines(&header), rows, span })
            }
        }
    }
}

impl Parser {
    // Link reference definitions (§4.7) come off the front of a
    // paragraph, one after another, and are not content. Returns how
    // many characters were consumed.
    fn take_link_refs(&mut self, content: &Content) -> usize {
        let mut at = 0;
        loop {
            let Some((label, dest, title, next)) = parse_link_ref(content, at) else { return at };
            let normalized = inline::normalize_label(&label);
            // The first definition of a label wins; a later one is
            // simply ignored.
            if !normalized.is_empty() && !self.link_refs.iter().any(|r| r.label == normalized) {
                let span = content.source_offset(at).unwrap_or(0)..content.source_offset(next.saturating_sub(1)).map(|o| o + 1).unwrap_or(0);
                self.link_refs.push(LinkRef { label: normalized, dest, title, span });
            }
            at = next;
        }
    }
}

// `[label]: /destination "optional title"`, possibly spread over lines.
fn parse_link_ref(content: &Content, from: usize) -> Option<(String, String, String, usize)> {
    let chars = &content.chars;
    let mut i = skip_spaces(chars, from);
    if chars.get(i) != Some(&'[') {
        return None;
    }
    let label_start = i + 1;
    let mut j = label_start;
    let mut depth = 0;
    loop {
        match chars.get(j) {
            None => return None,
            Some('\\') => j += 2,
            Some('[') => {
                depth += 1;
                j += 1;
            }
            Some(']') if depth == 0 => break,
            Some(']') => {
                depth -= 1;
                j += 1;
            }
            Some(_) => j += 1,
        }
    }
    let label: String = chars[label_start..j].iter().collect();
    if label.trim().is_empty() || label.chars().count() > 999 {
        return None;
    }
    i = j + 1;
    if chars.get(i) != Some(&':') {
        return None;
    }
    i += 1;
    i = skip_ws_one_newline(chars, i)?;
    let (dest, next) = read_destination(chars, i)?;
    if dest.is_empty() {
        return None;
    }
    i = next;

    // A title has to be on the same line as the destination or the next
    // one, and if what follows it isn't a line end, there is no title
    // *and* no definition -- so the whole thing is reconsidered without.
    let after_dest = i;
    let mut title = String::new();
    if let Some(title_start) = skip_ws_one_newline_optional(chars, i)
        && title_start > after_dest
        && let Some((t, next)) = read_title(chars, title_start)
        && line_ends_after(chars, next)
    {
        title = t;
        i = next;
    }
    if !line_ends_after(chars, i) {
        return None;
    }
    let end = match chars[i..].iter().position(|&c| c == '\n') {
        Some(n) => i + n + 1,
        None => chars.len(),
    };
    Some((label, dest, title, end))
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
    while matches!(chars.get(i), Some(' ') | Some('\t')) {
        i += 1;
    }
    i
}

// Whitespace including at most one newline -- the spec's own limit, so a
// blank line ends the definition rather than continuing it.
fn skip_ws_one_newline(chars: &[char], from: usize) -> Option<usize> {
    let mut i = skip_spaces(chars, from);
    if chars.get(i) == Some(&'\n') {
        i = skip_spaces(chars, i + 1);
        if chars.get(i) == Some(&'\n') {
            return None;
        }
    }
    Some(i)
}

fn skip_ws_one_newline_optional(chars: &[char], from: usize) -> Option<usize> {
    skip_ws_one_newline(chars, from)
}

fn line_ends_after(chars: &[char], from: usize) -> bool {
    chars[from.min(chars.len())..].iter().take_while(|c| **c != '\n').all(|c| c.is_whitespace())
}

fn read_destination(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) == Some(&'<') {
        let mut i = start + 1;
        let mut out = String::new();
        while let Some(&c) = chars.get(i) {
            match c {
                '>' => return Some((out, i + 1)),
                '\n' => return None,
                '\\' if chars.get(i + 1).is_some_and(|c| c.is_ascii_punctuation()) => {
                    out.push(chars[i + 1]);
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
    let mut out = String::new();
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() || c.is_control() {
            break;
        }
        if c == '\\' && chars.get(i + 1).is_some_and(|c| c.is_ascii_punctuation()) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    Some((out, i))
}

fn read_title(chars: &[char], start: usize) -> Option<(String, usize)> {
    let open = *chars.get(start)?;
    let close = match open {
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return None,
    };
    let mut i = start + 1;
    let mut out = String::new();
    while let Some(&c) = chars.get(i) {
        if c == close {
            return Some((out, i + 1));
        }
        if c == '\\' && chars.get(i + 1).is_some_and(|c| c.is_ascii_punctuation()) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    None
}

// GFM tables: a row is split on unescaped `|`, with the optional leading
// and trailing ones dropped, then padded or truncated to the header's
// own column count -- the spec's rule, so a ragged row still lines up.
fn split_row(content: &Content, columns: Option<usize>) -> Vec<Content> {
    let chars = &content.chars;
    let mut cells: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '|' => {
                cells.push((start, i));
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    cells.push((start, chars.len()));
    // A leading `|` produces an empty first cell, and a trailing one an
    // empty last cell; both are the delimiters, not content.
    if cells.first().is_some_and(|(s, e)| content.chars[*s..*e].iter().all(|c| c.is_whitespace()))
        && chars.contains(&'|')
        && chars.iter().find(|c| !c.is_whitespace()) == Some(&'|')
    {
        cells.remove(0);
    }
    if cells.len() > 1
        && cells.last().is_some_and(|(s, e)| content.chars[*s..*e].iter().all(|c| c.is_whitespace()))
        && chars.iter().rev().find(|c| !c.is_whitespace()) == Some(&'|')
    {
        cells.pop();
    }
    let mut out: Vec<Content> = cells
        .into_iter()
        .map(|(s, e)| {
            let mut s = s;
            let mut e = e;
            while s < e && chars[s].is_whitespace() {
                s += 1;
            }
            while e > s && chars[e - 1].is_whitespace() {
                e -= 1;
            }
            Content { chars: chars[s..e].to_vec(), offsets: content.offsets[s..e].to_vec() }
        })
        .collect();
    // `None` while counting the header's own columns; `Some` once that
    // count is known and every row has to match it.
    if let Some(columns) = columns {
        out.truncate(columns.max(1));
        while out.len() < columns {
            out.push(Content { chars: Vec::new(), offsets: Vec::new() });
        }
    }
    out
}

// The delimiter row under a table header: `| --- | :-: | ---: |`.
// Returns one alignment per column, or `None` when the line isn't one.
fn delimiter_row(chars: &[char]) -> Option<Vec<Align>> {
    let text: String = chars.iter().collect();
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ' | '\t')) {
        return None;
    }
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    let mut align = Vec::new();
    for cell in inner.split('|') {
        let cell = cell.trim();
        let left = cell.starts_with(':');
        let right = cell.ends_with(':');
        let dashes = cell.trim_start_matches(':').trim_end_matches(':');
        if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
            return None;
        }
        align.push(match (left, right) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            (false, false) => Align::None,
        });
    }
    (!align.is_empty()).then_some(align)
}

// Whether the character at `i` is preceded by an odd number of
// backslashes, and so escaped.
fn escaped_at(line: &str, i: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut n = 0;
    let mut j = i;
    while j > 0 && chars[j - 1] == '\\' {
        n += 1;
        j -= 1;
    }
    n % 2 == 1
}
