// roff, the typesetting language man pages are written in, parsed here
// without `man`, groff or a crate.
//
// Three users: `bishedit::manpages` mines flags and descriptions from
// pages, the editor highlights `.1`/`.3` files, and `:preview` renders
// one.
//
// Two layers for two consumers. `lexer` finds where control lines,
// requests, escapes and comments are, for highlighting, which must never
// interpret. `interp` is a roff interpreter (registers, strings, macros,
// conditionals), and `man` is the man(7) package on it, producing the
// document model below. The same split as bash's highlighter and parser.
//
// What's implemented is what man pages use: macro definition,
// conditionals, registers and strings with arithmetic, the escapes, and
// the man(7) macros. Not page layout, traps, diversions, tbl/eqn/pic or
// `.so`; those are reported (see `Document::notes`), not mis-rendered.

// The model is kept whole though callers use parts of it (spans for the
// highlighter, structure for the miner, both for the preview), as in
// html/mod.rs and markdown/mod.rs.
#![allow(dead_code)]

pub mod interp;
pub mod lexer;
pub mod man;
pub mod render;

use std::ops::Range;

// A man page's `.TH` line: the metadata every page opens with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Header {
    pub title: String,
    pub section: String,
    pub date: String,
    pub source: String,
    pub manual: String,
}

// roff's font is a mode, not a nesting: `\fB` is bold until something
// switches away. So each run carries a style rather than being wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    // Constant-width (`\f(CW`, `.EX`), what pages use for code.
    pub mono: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text { text: String, style: Style, span: Range<usize> },
    // `.br` inside a paragraph, and the line breaks a `.nf` block keeps.
    Break { span: Range<usize> },
    // `.UR`/`.UE` (a URL) and `.MT`/`.ME` (an email address).
    Link { url: String, content: Vec<Inline>, span: Range<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    // `.SH` is level 1, `.SS` level 2.
    Heading { level: u8, content: Vec<Inline>, span: Range<usize> },
    Paragraph { content: Vec<Inline>, span: Range<usize> },
    // `.TP` and `.IP`: a tag (the flag, the term) and the indented body
    // documenting it, the most useful structure in a man page.
    Tagged { tag: Vec<Inline>, blocks: Vec<Block>, span: Range<usize> },
    // `.RS`/`.RE`.
    Indented { blocks: Vec<Block>, span: Range<usize> },
    // `.nf`/`.fi` and `.EX`/`.EE`: text kept exactly as written.
    Literal { lines: Vec<String>, span: Range<usize> },
}

impl Block {
    pub fn span(&self) -> Range<usize> {
        match self {
            Block::Heading { span, .. }
            | Block::Paragraph { span, .. }
            | Block::Tagged { span, .. }
            | Block::Indented { span, .. }
            | Block::Literal { span, .. } => span.clone(),
        }
    }
}

impl Inline {
    pub fn span(&self) -> Range<usize> {
        match self {
            Inline::Text { span, .. } | Inline::Break { span } | Inline::Link { span, .. } => span.clone(),
        }
    }

    // The characters with styling dropped: a flag name or section title as
    // text.
    pub fn text_content(&self) -> String {
        match self {
            Inline::Text { text, .. } => text.clone(),
            Inline::Break { .. } => " ".to_string(),
            Inline::Link { content, .. } => content.iter().map(|i| i.text_content()).collect(),
        }
    }
}

pub fn text_of(inlines: &[Inline]) -> String {
    inlines.iter().map(|i| i.text_content()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Document {
    pub header: Option<Header>,
    pub blocks: Vec<Block>,
    // What this parse met and couldn't honour: an unimplemented request, a
    // preprocessor block, a `.so`. Collected so a caller can say "this page
    // uses tbl" rather than render a table as garbage.
    pub notes: Vec<String>,
}

impl Document {
    // The blocks under the first heading matching `name` case-insensitively
    // (`NAME`, `OPTIONS`), up to the next heading at its level.
    pub fn section(&self, name: &str) -> Vec<&Block> {
        let mut out = Vec::new();
        let mut inside = false;
        for block in &self.blocks {
            if let Block::Heading { level: 1, content, .. } = block {
                inside = text_of(content).trim().eq_ignore_ascii_case(name);
                continue;
            }
            if inside {
                out.push(block);
            }
        }
        out
    }
}

// A whole man page, parsed.
pub fn parse(source: &str) -> Document {
    man::parse(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A compact view of the block tree, so an expectation reads like the
    // page. Fonts as sigils: *bold*, _italic_, `mono`.
    fn tree(source: &str) -> String {
        let doc = parse(source);
        let mut out = String::new();
        for block in &doc.blocks {
            write_block(block, 0, &mut out);
        }
        out
    }

    fn write_block(block: &Block, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match block {
            Block::Heading { level, content, .. } => out.push_str(&format!("{pad}h{level}: {}\n", show(content))),
            Block::Paragraph { content, .. } => out.push_str(&format!("{pad}p: {}\n", show(content))),
            Block::Literal { lines, .. } => out.push_str(&format!("{pad}literal: {:?}\n", lines)),
            Block::Tagged { tag, blocks, .. } => {
                out.push_str(&format!("{pad}tag: {}\n", show(tag)));
                for b in blocks {
                    write_block(b, depth + 1, out);
                }
            }
            Block::Indented { blocks, .. } => {
                out.push_str(&format!("{pad}indent:\n"));
                for b in blocks {
                    write_block(b, depth + 1, out);
                }
            }
        }
    }

    fn show(inlines: &[Inline]) -> String {
        inlines
            .iter()
            .map(|i| match i {
                Inline::Text { text, style, .. } => {
                    let mut s = text.clone();
                    if style.bold {
                        s = format!("*{s}*");
                    }
                    if style.italic {
                        s = format!("_{s}_");
                    }
                    if style.mono {
                        s = format!("`{s}`");
                    }
                    s
                }
                Inline::Break { .. } => "\\n".to_string(),
                Inline::Link { url, content, .. } => format!("[{}]({url})", show(content)),
            })
            .collect()
    }

    fn assert_tree(source: &str, expected: &str) {
        let got = tree(source);
        let lines: Vec<&str> = expected.lines().filter(|l| !l.trim().is_empty()).collect();
        let dedent = lines.iter().map(|l| l.len() - l.trim_start_matches(' ').len()).min().unwrap_or(0);
        let want: String = lines.iter().map(|l| format!("{}\n", &l[dedent..])).collect();
        assert_eq!(got, want, "\n--- source ---\n{source}\n--- got ---\n{got}--- want ---\n{want}");
    }

    #[test]
    fn the_th_line_becomes_the_header() {
        let doc = parse(".TH LS 1 \"March 2024\" \"GNU coreutils 9.4\" \"User Commands\"\n");
        let header = doc.header.expect("a .TH line");
        assert_eq!(header.title, "LS");
        assert_eq!(header.section, "1");
        assert_eq!(header.source, "GNU coreutils 9.4");
        assert_eq!(header.manual, "User Commands");
    }

    #[test]
    fn sections_and_subsections() {
        assert_tree(
            ".SH NAME\nls \\- list directory contents\n.SS Sorting\ntext here\n",
            r#"
            h1: NAME
            p: ls - list directory contents
            h2: Sorting
            p: text here
            "#,
        );
    }

    // A tagged paragraph pairs the flag with its prose.
    #[test]
    fn a_tagged_paragraph_pairs_its_tag_with_its_body() {
        assert_tree(
            ".TP\n\\fB\\-a\\fR, \\fB\\-\\-all\\fR\ndo not ignore entries starting with .\n.TP\n\\fB\\-l\\fR\nuse a long listing\n",
            r#"
            tag: *-a*, *--all*
              p: do not ignore entries starting with .
            tag: *-l*
              p: use a long listing
            "#,
        );
    }

    // Consecutive input lines are one paragraph: roff fills text.
    #[test]
    fn text_lines_are_filled_into_one_paragraph() {
        assert_tree(
            "first line\nsecond line\n.PP\nnew paragraph\n",
            r#"
            p: first line second line
            p: new paragraph
            "#,
        );
    }

    // `.BR ls (1)` is a cross reference: bold then roman, no space between.
    #[test]
    fn alternating_font_macros_join_their_arguments_without_spaces() {
        assert_tree(".BR ls (1)\n", "p: *ls*(1)\n");
        assert_tree(".BI file= name\n", "p: *file=*_name_\n");
        assert_tree(".IR a b c\n", "p: _a_b_c_\n");
    }

    // A request line ending in `\r\n` must split its arguments and stop;
    // macOS ships a page written that way.
    #[test]
    fn a_request_line_with_crlf_line_endings_parses() {
        let doc = parse(".Sh NAME\r\n.Nm foo\r\n");
        assert!(!doc.section("NAME").is_empty());
    }

    #[test]
    fn a_font_escape_stays_in_effect_across_lines() {
        assert_tree(
            "plain \\fBbold\nstill bold\\fP plain again\n",
            r#"
            p: plain *bold* *still bold* plain again
            "#,
        );
    }

    // The scaffolding a generated page opens with: a macro defined, invoked,
    // its argument substituted.
    #[test]
    fn a_user_defined_macro_is_expanded_with_its_arguments() {
        assert_tree(
            ".de Flag\n.B \\\\$1\n..\n.Flag --verbose\n",
            r#"
            p: *--verbose*
            "#,
        );
    }

    #[test]
    fn strings_and_registers_interpolate() {
        assert_tree(
            ".ds Nm bish\n.nr Vn 3\nthe \\*(Nm shell, version \\n(Vn\n",
            r#"
            p: the bish shell, version 3
            "#,
        );
    }

    // A terminal is nroff, so conditionals take the `n` branch.
    #[test]
    fn conditionals_take_the_nroff_branch() {
        assert_tree(".ie n .SH TERMINAL\n.el .SH TYPESET\n", "h1: TERMINAL\n");
        assert_tree(".if t .SH NEVER\n.if n .SH ALWAYS\n", "h1: ALWAYS\n");
    }

    // A `\{` block runs to its matching `\}`. `.\}` isn't a break, so the
    // line after the block fills into the paragraph inside it, as groff
    // does.
    #[test]
    fn a_braced_conditional_block_spans_lines() {
        assert_tree(
            ".if n \\{\\\n.SH TAKEN\ninside the block\n.\\}\n.PP\nafter\n",
            r#"
            h1: TAKEN
            p: inside the block
            p: after
            "#,
        );
        // ...and an untaken block contributes nothing, headings included.
        assert_tree(
            ".if t \\{\\\n.SH SKIPPED\nnot this either\n.\\}\n.PP\nafter\n",
            r#"
            p: after
            "#,
        );
    }

    #[test]
    fn ignored_blocks_are_skipped_entirely() {
        assert_tree(".ig\nthis is not content\n.SH NEITHER IS THIS\n..\n.SH REAL\n", "h1: REAL\n");
    }

    #[test]
    fn comments_are_not_content() {
        assert_tree(
            ".\\\" a whole-line comment\ntext \\\" a trailing comment\n",
            r#"
            p: text
            "#,
        );
    }

    #[test]
    fn no_fill_mode_keeps_the_lines_as_written() {
        assert_tree(
            ".nf\n  indented   exactly\n    like this\n.fi\nfilled again\n",
            r#"
            literal: ["  indented   exactly", "    like this"]
            p: filled again
            "#,
        );
    }

    #[test]
    fn indent_blocks_nest() {
        assert_tree(
            "outer\n.RS\ninner\n.RS\ndeeper\n.RE\n.RE\nback out\n",
            r#"
            p: outer
            indent:
              p: inner
              indent:
                p: deeper
            p: back out
            "#,
        );
    }

    #[test]
    fn url_macros_become_links() {
        assert_tree(
            ".UR https://example.com\nthe site\n.UE .\n",
            r#"
            p: [the site](https://example.com).
            "#,
        );
    }

    #[test]
    fn special_characters_and_escaped_hyphens_resolve() {
        assert_tree("\\-\\-flag \\(bu \\(em \\(aq \\e\n", "p: --flag \u{2022} \u{2014} ' \\\n");
    }

    // groff wraps every option name in a coreutils page in
    // `\X'tty: link ...'`; device control strings, not text.
    #[test]
    fn device_control_escapes_contribute_nothing() {
        assert_tree("\\X'tty: link https://example.com'\\fB\\-a\\fP\\X'tty: link'\n", "p: *-a*\n");
    }

    #[test]
    fn a_section_can_be_looked_up_by_name() {
        let doc = parse(".SH NAME\nls \\- list directory contents\n.SH SEE ALSO\nother\n");
        let name = doc.section("name");
        assert_eq!(name.len(), 1);
        match name[0] {
            Block::Paragraph { content, .. } => assert_eq!(text_of(content), "ls - list directory contents"),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_page_parses_to_nothing() {
        assert_eq!(parse("").blocks.len(), 0);
        assert_eq!(parse(".\n.\n").blocks.len(), 0);
    }

    // A page is untrusted input; this must terminate on anything.
    #[test]
    fn pathological_input_terminates() {
        // A macro that invokes itself.
        let _ = parse(".de X\n.X\n..\n.X\n");
        // A string that refers to itself.
        let _ = parse(".ds A \\*(A\nuse \\*(A\n");
        // Unbalanced everything.
        let _ = parse(".if n \\{\nno close brace\n");
        let _ = parse(".de never_ends\nbody\n");
        let _ = parse(".nf\nno end\n");
        let _ = parse(".TP\n");
        let _ = parse(".RE\n.RE\n.RE\n");
    }

    #[test]
    fn every_truncation_of_a_gnarly_page_terminates() {
        let page = ".TH T 1\n.SH NAME\n.de M\n.B \\\\$1\n..\n.ie n .M x\n.el .M y\n.TP\n\\fB\\-a\\fR\nbody\n.nf\nlit\n.fi\n.UR u\nl\n.UE\n";
        for cut in 0..=page.len() {
            if page.is_char_boundary(cut) {
                let _ = parse(&page[..cut]);
            }
        }
    }
}

#[cfg(test)]
mod real_world_tests {
    use super::*;

    // Every man page on this machine, parsed: real pages from help2man,
    // pod2man, asciidoctor and by hand. Skipped where there are none.
    #[test]
    fn parses_every_man_page_on_this_machine() {
        let mut checked = 0;
        let mut with_name = 0;
        for section in 1..=8 {
            let dir = format!("/usr/share/man/man{section}");
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten().take(150) {
                let path = entry.path();
                let Some(text) = read_page(&path) else { continue };
                if text.len() > 2_000_000 {
                    continue;
                }
                let doc = parse(&text);
                let len = text.chars().count();
                for block in &doc.blocks {
                    check_spans(block, len, &path);
                }
                // A page with any content mustn't render empty.
                if text.contains(".SH") {
                    assert!(!doc.blocks.is_empty(), "{} rendered to nothing", path.display());
                }
                if !doc.section("NAME").is_empty() {
                    with_name += 1;
                }
                checked += 1;
            }
        }
        if checked == 0 {
            return;
        }
        // Nearly all real pages have a NAME section; a floor that catches a
        // parse silently losing structure.
        assert!(with_name * 10 >= checked * 8, "only {with_name} of {checked} pages had a NAME section -- structure is being lost");
    }

    // Man pages are gzipped.
    fn read_page(path: &std::path::Path) -> Option<String> {
        if path.extension().is_some_and(|e| e == "gz") {
            let (_, bytes) = crate::archive::gunzip(path).ok()?;
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
        std::fs::read_to_string(path).ok()
    }

    fn check_spans(block: &Block, len: usize, path: &std::path::Path) {
        let span = block.span();
        assert!(span.start <= span.end && span.end <= len, "{}: bad block span {span:?}", path.display());
        match block {
            Block::Tagged { tag, blocks, .. } => {
                for i in tag {
                    let s = i.span();
                    assert!(s.start <= s.end && s.end <= len, "{}: bad tag span", path.display());
                }
                for b in blocks {
                    check_spans(b, len, path);
                }
            }
            Block::Indented { blocks, .. } => {
                for b in blocks {
                    check_spans(b, len, path);
                }
            }
            Block::Paragraph { content, .. } | Block::Heading { content, .. } => {
                for i in content {
                    let s = i.span();
                    assert!(s.start <= s.end && s.end <= len, "{}: bad inline span", path.display());
                }
            }
            Block::Literal { .. } => {}
        }
    }
}
