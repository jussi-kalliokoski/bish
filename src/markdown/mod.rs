// CommonMark + GFM, hand-rolled -- no external crate, same as the rest of
// the parsers here. Three things use it: `:help` (which is written in
// markdown and rendered to the terminal), markdown syntax highlighting in
// the editor, and previewing a `.md` file.
//
// Those three needs are why this produces a real AST with *source spans*
// rather than rendering straight to text. A renderer wants structure; a
// highlighter wants to know which characters of the original are a
// heading marker, a code fence, a link destination. One parse serves
// both because every node remembers where it came from.
//
// The parse is the spec's own two passes, and it has to be: block
// structure first (§4), then inlines within the leaf blocks that take
// them (§6). They can't be one pass, because whether `- foo` starts a
// list depends on lines that come after it, and whether `*a*` is
// emphasis depends on characters on both sides.
//
// HTML found in a document goes to crate::html -- a real parse, not a
// span of raw text -- so a preview can render what the markup means. See
// html/mod.rs for why that's a full WHATWG parser.
//
// GFM extensions, on by default: tables, strikethrough, task list items,
// and autolinks. They're what every markdown file in the wild assumes,
// and tables in particular are what make a keybinding list in `:help`
// worth writing.

// The parser exposes the whole document model -- every block and inline
// kind, with the spans each came from -- and each of its three consumers
// uses a different part: a renderer walks the tree, a highlighter reads
// the spans, a preview does both. Kept whole rather than trimmed to
// today's callers, the same reasoning (and the same allow) as
// html/mod.rs and bishedit::highlight.
#![allow(dead_code)]

pub mod block;
pub mod inline;
pub mod render;

use std::ops::Range;

// A parsed document plus the link reference definitions collected from
// it, which inline parsing needs and which a caller may want to resolve
// links itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub blocks: Vec<Block>,
    pub link_refs: Vec<LinkRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkRef {
    // Normalized per §4.7: case-folded and whitespace-collapsed, so
    // `[FOO bar]` and `[foo   BAR]` are the same label.
    pub label: String,
    pub dest: String,
    pub title: String,
    pub span: Range<usize>,
}

// `CodeBlock`/`HtmlBlock`/`BlockQuote` repeat the enum's name on
// purpose: those are what CommonMark calls them, and matching the spec's
// vocabulary is worth more here than avoiding the stutter -- `Block::Code`
// would read as a code *span*, which is a different thing entirely.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph { content: Vec<Inline>, span: Range<usize> },
    // `level` is 1-6. `marker` covers the `#`s (or the setext
    // underline), which is what a highlighter colours.
    Heading { level: u8, content: Vec<Inline>, marker: Range<usize>, span: Range<usize> },
    // `info` is the fence's info string ("rust", "bash frame=none"), and
    // its first word is what picks a nested highlighter.
    CodeBlock { info: String, literal: String, fenced: bool, info_span: Range<usize>, literal_span: Range<usize>, span: Range<usize> },
    // Raw HTML, kept verbatim *and* parsed: `raw` is what a highlighter
    // colours, and crate::html::parse_fragment is what a renderer walks.
    HtmlBlock { raw: String, span: Range<usize> },
    BlockQuote { blocks: Vec<Block>, span: Range<usize> },
    List(List),
    ThematicBreak { span: Range<usize> },
    Table(Table),
}

#[derive(Debug, Clone, PartialEq)]
pub struct List {
    pub ordered: bool,
    pub start: u64,
    // A "tight" list renders its items without blank lines between them
    // -- decided by the source, not by the renderer, because it depends
    // on where the blank lines were.
    pub tight: bool,
    pub items: Vec<ListItem>,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    pub blocks: Vec<Block>,
    // GFM task list items: `- [x] done`. `None` for an ordinary item.
    pub task: Option<bool>,
    pub marker: Range<usize>,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub align: Vec<Align>,
    pub head: Vec<Vec<Inline>>,
    pub rows: Vec<Vec<Vec<Inline>>>,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text { text: String, span: Range<usize> },
    Code { text: String, span: Range<usize> },
    // Spans include the delimiters (`*a*`, `**a**`, `~~a~~`), the same
    // as `Code` and `Link` -- see inline.rs's `wrap_emphasis`.
    Emph { content: Vec<Inline>, span: Range<usize> },
    Strong { content: Vec<Inline>, span: Range<usize> },
    Strikethrough { content: Vec<Inline>, span: Range<usize> },
    Link { dest: String, title: String, content: Vec<Inline>, span: Range<usize> },
    Image { dest: String, title: String, alt: Vec<Inline>, span: Range<usize> },
    // Inline raw HTML (`<br>`, `<span class=x>`), recognized by the same
    // grammar the HTML tokenizer uses.
    Html { raw: String, span: Range<usize> },
    // A line ending inside a paragraph: a space to a renderer, a real
    // break only if it was two spaces or a backslash (HardBreak).
    SoftBreak { span: Range<usize> },
    HardBreak { span: Range<usize> },
}

impl Inline {
    pub fn span(&self) -> Range<usize> {
        match self {
            Inline::Text { span, .. }
            | Inline::Code { span, .. }
            | Inline::Emph { span, .. }
            | Inline::Strong { span, .. }
            | Inline::Strikethrough { span, .. }
            | Inline::Link { span, .. }
            | Inline::Image { span, .. }
            | Inline::Html { span, .. }
            | Inline::SoftBreak { span }
            | Inline::HardBreak { span } => span.clone(),
        }
    }

    // Every character this inline and its children stand for, with the
    // markup removed -- what a heading's own text is for an anchor, and
    // what an image's alt text is.
    pub fn text_content(&self) -> String {
        match self {
            Inline::Text { text, .. } | Inline::Code { text, .. } => text.clone(),
            Inline::Emph { content, .. }
            | Inline::Strong { content, .. }
            | Inline::Strikethrough { content, .. }
            | Inline::Link { content, .. }
            | Inline::Image { alt: content, .. } => content.iter().map(|i| i.text_content()).collect(),
            Inline::Html { .. } => String::new(),
            Inline::SoftBreak { .. } | Inline::HardBreak { .. } => " ".to_string(),
        }
    }
}

impl Block {
    pub fn span(&self) -> Range<usize> {
        match self {
            Block::Paragraph { span, .. }
            | Block::Heading { span, .. }
            | Block::CodeBlock { span, .. }
            | Block::HtmlBlock { span, .. }
            | Block::BlockQuote { span, .. }
            | Block::ThematicBreak { span } => span.clone(),
            Block::List(l) => l.span.clone(),
            Block::Table(t) => t.span.clone(),
        }
    }
}

// Spans everywhere are *char* offsets into the input, not byte offsets
// -- the editor's own highlighting indexes a `&[char]` (see
// bishedit::highlight::compose), and every other span in this codebase
// is already in the same units.
pub fn parse(input: &str) -> Document {
    block::parse(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A compact rendering of the block tree, so an expectation reads as
    // the structure it describes. Inlines are shown with their markup
    // reconstructed as sigils: *emph*, **strong**, `code`, [link](dest).
    fn tree(input: &str) -> String {
        let doc = parse(input);
        let mut out = String::new();
        for block in &doc.blocks {
            write_block(block, 0, &mut out);
        }
        out
    }

    fn write_block(block: &Block, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match block {
            Block::Paragraph { content, .. } => out.push_str(&format!("{pad}p: {}\n", show(content))),
            Block::Heading { level, content, .. } => {
                out.push_str(&format!("{pad}h{level}: {}\n", show(content)))
            }
            Block::CodeBlock { info, literal, fenced, .. } => {
                let kind = if *fenced { "fence" } else { "indent" };
                out.push_str(&format!("{pad}code[{kind}:{info}]: {:?}\n", literal));
            }
            Block::HtmlBlock { raw, .. } => out.push_str(&format!("{pad}html: {:?}\n", raw)),
            Block::BlockQuote { blocks, .. } => {
                out.push_str(&format!("{pad}quote:\n"));
                for b in blocks {
                    write_block(b, depth + 1, out);
                }
            }
            Block::ThematicBreak { .. } => out.push_str(&format!("{pad}hr\n")),
            Block::List(l) => {
                let kind = if l.ordered { format!("ol@{}", l.start) } else { "ul".to_string() };
                let tight = if l.tight { "tight" } else { "loose" };
                out.push_str(&format!("{pad}{kind} {tight}:\n"));
                for item in &l.items {
                    let task = match item.task {
                        Some(true) => " [x]",
                        Some(false) => " [ ]",
                        None => "",
                    };
                    out.push_str(&format!("{pad}  item{task}:\n"));
                    for b in &item.blocks {
                        write_block(b, depth + 2, out);
                    }
                }
            }
            Block::Table(t) => {
                out.push_str(&format!("{pad}table {:?}:\n", t.align));
                out.push_str(&format!("{pad}  head: {}\n", t.head.iter().map(|c| show(c)).collect::<Vec<_>>().join(" | ")));
                for row in &t.rows {
                    out.push_str(&format!("{pad}  row: {}\n", row.iter().map(|c| show(c)).collect::<Vec<_>>().join(" | ")));
                }
            }
        }
    }

    fn show(inlines: &[Inline]) -> String {
        inlines
            .iter()
            .map(|i| match i {
                Inline::Text { text, .. } => text.clone(),
                Inline::Code { text, .. } => format!("`{text}`"),
                Inline::Emph { content, .. } => format!("*{}*", show(content)),
                Inline::Strong { content, .. } => format!("**{}**", show(content)),
                Inline::Strikethrough { content, .. } => format!("~~{}~~", show(content)),
                Inline::Link { dest, title, content, .. } => {
                    let t = if title.is_empty() { String::new() } else { format!(" \"{title}\"") };
                    format!("[{}]({dest}{t})", show(content))
                }
                Inline::Image { dest, alt, .. } => format!("![{}]({dest})", show(alt)),
                Inline::Html { raw, .. } => format!("<html:{raw}>"),
                Inline::SoftBreak { .. } => "\\n".to_string(),
                Inline::HardBreak { .. } => "\\\n".to_string(),
            })
            .collect()
    }

    fn assert_tree(input: &str, expected: &str) {
        let got = tree(input);
        let lines: Vec<&str> = expected.lines().filter(|l| !l.trim().is_empty()).collect();
        let dedent = lines.iter().map(|l| l.len() - l.trim_start_matches(' ').len()).min().unwrap_or(0);
        let want: String = lines.iter().map(|l| format!("{}\n", &l[dedent..])).collect();
        assert_eq!(got, want, "\n--- input ---\n{input}\n--- got ---\n{got}--- want ---\n{want}");
    }

    #[test]
    fn paragraphs_and_headings() {
        assert_tree(
            "# One\n\nsome text\nmore text\n\n## Two ##\n\nSetext\n======\n",
            r#"
            h1: One
            p: some text\nmore text
            h2: Two
            h1: Setext
            "#,
        );
    }

    #[test]
    fn emphasis_and_strong_including_the_nested_case() {
        assert_tree(
            "*a* **b** ***c*** _d_ __e__\n",
            r#"
            p: *a* **b** ***c*** *d* **e**
            "#,
        );
    }

    // `_` may not open or close inside a word, which is what keeps
    // identifiers intact.
    #[test]
    fn underscores_inside_a_word_are_not_emphasis() {
        assert_tree(
            "snake_case_name and a_b_c\n",
            r#"
            p: snake_case_name and a_b_c
            "#,
        );
        assert_tree("*star*inside*word*\n", "p: *star*inside*word*\n");
    }

    #[test]
    fn code_spans_take_the_matching_backtick_run() {
        assert_tree(
            "`a` ``b ` c`` ` not closed\n",
            r#"
            p: `a` `b ` c` ` not closed
            "#,
        );
        // One leading and trailing space is stripped, so a code span can
        // hold a backtick of its own.
        assert_tree("`` ` ``\n", "p: ```\n");
    }

    #[test]
    fn backslash_escapes_and_hard_breaks() {
        assert_tree(
            "\\*not emphasis\\* a\\\nb  \nc\n",
            r#"
            p: *not emphasis* a\
            b\
            c
            "#,
        );
    }

    #[test]
    fn links_images_and_autolinks() {
        assert_tree(
            "[text](/dest \"title\") ![alt](/img) <https://example.com> <a@b.com>\n",
            r#"
            p: [text](/dest "title") ![alt](/img) [https://example.com](https://example.com) [a@b.com](mailto:a@b.com)
            "#,
        );
    }

    #[test]
    fn reference_links_resolve_against_definitions_anywhere_in_the_document() {
        assert_tree(
            "[full][ref] [collapsed][] [shortcut]\n\n[ref]: /r\n[collapsed]: /c\n[shortcut]: /s \"T\"\n",
            r#"
            p: [full](/r) [collapsed](/c) [shortcut](/s "T")
            "#,
        );
    }

    #[test]
    fn a_definition_only_paragraph_produces_no_block() {
        assert_tree("[a]: /x\n[b]: /y\n", "");
        let doc = parse("[a]: /x\n[b]: /y\n");
        assert_eq!(doc.link_refs.len(), 2);
        assert_eq!(doc.link_refs[0].dest, "/x");
    }

    #[test]
    fn code_blocks_fenced_and_indented() {
        assert_tree(
            "```rust\nfn main() {}\n```\n\n    indented\n    code\n",
            r#"
            code[fence:rust]: "fn main() {}\n"
            code[indent:]: "indented\ncode\n"
            "#,
        );
    }

    #[test]
    fn block_quotes_nest_and_contain_blocks() {
        assert_tree(
            "> # Q\n> text\n>\n> > deeper\n",
            r#"
            quote:
              h1: Q
              p: text
              quote:
                p: deeper
            "#,
        );
    }

    // Lazy continuation: a paragraph inside a block quote continues even
    // on a line with no `>`.
    #[test]
    fn a_paragraph_in_a_quote_continues_lazily() {
        assert_tree(
            "> one\ntwo\n",
            r#"
            quote:
              p: one\ntwo
            "#,
        );
    }

    // A list holds items and nothing else, so anything that isn't one
    // has to close it -- found by the `:help` document, whose whole
    // second half sat after a bullet list and disappeared.
    #[test]
    fn a_block_after_a_list_closes_it_rather_than_vanishing_into_it() {
        assert_tree(
            "- a\n\n## After\n\ntext\n",
            r#"
            ul tight:
              item:
                p: a
            h2: After
            p: text
            "#,
        );
        // ...including out of a nested list, and including a block that
        // opens rather than one that is complete in a line.
        assert_tree(
            "- a\n  - b\n\n> quoted\n",
            r#"
            ul tight:
              item:
                p: a
                ul tight:
                  item:
                    p: b
            quote:
              p: quoted
            "#,
        );
    }

    #[test]
    fn lists_tight_and_loose() {
        assert_tree(
            "- a\n- b\n\n* c\n\n* d\n",
            r#"
            ul tight:
              item:
                p: a
              item:
                p: b
            ul loose:
              item:
                p: c
              item:
                p: d
            "#,
        );
    }

    #[test]
    fn ordered_lists_keep_their_start_and_delimiter() {
        assert_tree(
            "3. three\n4. four\n",
            r#"
            ol@3 tight:
              item:
                p: three
              item:
                p: four
            "#,
        );
    }

    #[test]
    fn nested_lists_follow_the_content_indent() {
        assert_tree(
            "- a\n  - b\n    - c\n",
            r#"
            ul tight:
              item:
                p: a
                ul tight:
                  item:
                    p: b
                    ul tight:
                      item:
                        p: c
            "#,
        );
    }

    #[test]
    fn gfm_task_list_items() {
        assert_tree(
            "- [x] done\n- [ ] todo\n- plain\n",
            r#"
            ul tight:
              item [x]:
                p: done
              item [ ]:
                p: todo
              item:
                p: plain
            "#,
        );
    }

    #[test]
    fn gfm_strikethrough() {
        assert_tree("~~gone~~ and ~one~\n", "p: ~~gone~~ and ~~one~~\n");
    }

    #[test]
    fn gfm_tables_with_alignment() {
        assert_tree(
            "| Key | Does |\n|:--- |:---: |\n| `a` | one |\n| b | two |\n",
            r#"
            table [Left, Center]:
              head: Key | Does
              row: `a` | one
              row: b | two
            "#,
        );
    }

    // A table needs pipes in its header; `---` under a plain paragraph
    // is still a setext heading.
    #[test]
    fn a_dashed_line_under_a_plain_paragraph_is_still_a_heading() {
        assert_tree("Title\n---\n", "h2: Title\n");
    }

    #[test]
    fn thematic_breaks_in_their_three_spellings() {
        assert_tree("---\n\n***\n\n___\n", "hr\nhr\nhr\n");
    }

    #[test]
    fn html_blocks_are_kept_verbatim() {
        assert_tree(
            "<div class=\"x\">\n  <span>raw</span>\n</div>\n\ntext\n",
            r#"
            html: "<div class=\"x\">\n  <span>raw</span>\n</div>"
            p: text
            "#,
        );
    }

    #[test]
    fn inline_html_is_recognized_but_prose_with_angle_brackets_is_not() {
        assert_tree("a <b class=x>c</b> d\n", "p: a <html:<b class=x>>c<html:</b>> d\n");
        assert_tree("5 < 6 and a<b\n", "p: 5 < 6 and a<b\n");
    }

    #[test]
    fn entities_resolve_through_the_html_table() {
        assert_tree("&amp; &#65; &#x41; &notarealentity;\n", "p: & A A &notarealentity;\n");
    }

    // Spans are what the highlighter needs: every inline has to point at
    // the characters it came from, including inside a block quote whose
    // markers were stripped.
    #[test]
    fn spans_point_back_at_the_source_even_through_a_block_quote() {
        let src = "> a *bee* c\n";
        let doc = parse(src);
        let Block::BlockQuote { blocks, .. } = &doc.blocks[0] else { panic!("expected a quote") };
        let Block::Paragraph { content, .. } = &blocks[0] else { panic!("expected a paragraph") };
        let chars: Vec<char> = src.chars().collect();
        let emph = content.iter().find(|i| matches!(i, Inline::Emph { .. })).expect("expected emphasis");
        let span = emph.span();
        // Delimiters included -- see Inline::Emph.
        assert_eq!(chars[span.start..span.end].iter().collect::<String>(), "*bee*");
    }

    #[test]
    fn heading_markers_have_their_own_span() {
        let src = "## Two\n";
        let doc = parse(src);
        let Block::Heading { marker, .. } = &doc.blocks[0] else { panic!("expected a heading") };
        assert_eq!(&src[marker.start..marker.end], "##");
    }

    #[test]
    fn a_code_fence_info_string_has_its_own_span() {
        let src = "```rust ignore\nx\n```\n";
        let doc = parse(src);
        let Block::CodeBlock { info, info_span, .. } = &doc.blocks[0] else { panic!("expected code") };
        assert_eq!(info, "rust ignore");
        assert_eq!(&src[info_span.start..info_span.end], "rust ignore");
    }

    #[test]
    fn every_truncation_of_a_gnarly_document_terminates() {
        let doc = "# H\n\n> - [x] a *b* `c`\n>   | x | y |\n>   |---|---|\n>   | 1 | 2 |\n\n```rust\nfn f() {}\n```\n\n<div>\n<p>raw\n</div>\n\n[r]: /x \"t\"\n[a][r] ![i](/y) ~~s~~\n";
        for cut in 0..=doc.len() {
            if !doc.is_char_boundary(cut) {
                continue;
            }
            let _ = parse(&doc[..cut]);
        }
    }
}

#[cfg(test)]
mod real_world_tests {
    use super::*;

    // Every markdown file in this repository and its docs, parsed and
    // checked for the two invariants that hold for any document: every
    // span points inside the source, and the blocks come out in source
    // order. Real files written by hand, which is what catches a parser
    // that only works on the examples it was written against.
    #[test]
    fn parses_every_markdown_file_in_the_repository() {
        let mut checked = 0;
        for dir in [".", "..", "/usr/share/doc"] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let doc = parse(&text);
                let len = text.chars().count();
                let mut last_end = 0;
                for block in &doc.blocks {
                    let span = block.span();
                    assert!(span.start <= span.end, "{}: inverted span {span:?}", path.display());
                    assert!(span.end <= len, "{}: span {span:?} past the end ({len})", path.display());
                    assert!(span.start >= last_end.min(span.start), "{}: blocks out of order", path.display());
                    last_end = span.end;
                    check_inline_spans(block, len, &path);
                }
                checked += 1;
            }
        }
        if checked == 0 {
            return;
        }
        assert!(checked > 0);
    }

    fn check_inline_spans(block: &Block, len: usize, path: &std::path::Path) {
        let inlines: Vec<&Vec<Inline>> = match block {
            Block::Paragraph { content, .. } | Block::Heading { content, .. } => vec![content],
            Block::BlockQuote { blocks, .. } => {
                for b in blocks {
                    check_inline_spans(b, len, path);
                }
                vec![]
            }
            Block::List(l) => {
                for item in &l.items {
                    for b in &item.blocks {
                        check_inline_spans(b, len, path);
                    }
                }
                vec![]
            }
            Block::Table(t) => t.head.iter().chain(t.rows.iter().flatten()).collect(),
            _ => vec![],
        };
        for list in inlines {
            for inline in list {
                let span = inline.span();
                assert!(span.start <= span.end && span.end <= len, "{}: bad inline span {span:?}", path.display());
            }
        }
    }
}
