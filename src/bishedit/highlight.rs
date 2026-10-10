// Syntax highlighting, driven by each language's real parser rather than
// a parallel one: BashHighlighter over `lexer::tokenize_spanned`,
// JsonHighlighter over `json::tokens`, MarkdownHighlighter over
// `crate::markdown`. Used by editor.rs's `redraw` (the prompt, the colon
// line, Ctrl-E's view) and by fileeditor.rs, which picks by the buffer's
// language.
//
// `dead_code` is allowed because `HighlightKind::Number` is reserved for
// bash and the trait has room for implementors not written yet.
#![allow(dead_code)]

use crate::bishedit::manpages;
use crate::lexer::{self, Chunk, SpannedItem, Tok};
use crate::vt100;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;

// Session state the highlighter needs beyond the text, as one `Copy`
// bundle instead of a parameter per feature down the whole recursion.
// `Default` is "no context", for the colon line.
#[derive(Clone, Copy, Default)]
pub struct HighlightContext<'a> {
    pub cwd: Option<&'a Path>,
    // The session's function names, so calling a function just defined isn't
    // an invalid command. Aliases aren't included.
    pub known_functions: Option<&'a HashSet<String>>,
    // Command names the line has that the shell doesn't: on the editor's
    // colon line `:preview` and `:diag` are real commands found nowhere on
    // PATH. `None` at the ordinary prompt.
    pub line_commands: Option<&'a [&'a str]>,
    // The session's `PATH`, which the command-validity check resolves names
    // against. The process environment's is frozen at startup (exports reach
    // children, not the environment), so a directory added by `.bishrc` or a
    // `cd` hook would otherwise make its commands look invalid. `None` falls
    // back to the environment, for a context with no shell (a test,
    // `bish tool check`).
    pub path: Option<&'a str>,
    // Not read by the highlighting, which only produces kinds: carried here
    // because every call site that turns kinds into styles (`resolve_style`)
    // already has `ctx`.
    pub color_overrides: Option<&'a ColorOverrides>,
    // The `hyperlinks` bishopt, carried for the same reason. `false` by
    // default, so an analysis context (a test, `bish tool check`) emits none.
    pub hyperlinks: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HighlightKind {
    Keyword,
    Operator,
    Redirect,
    String,
    Variable,
    Substitution,
    Comment,
    // Every JSON number. Never produced for bash, which would need a scanner
    // for `$((...))`'s interior.
    Number,
    // An argument that is one of its command's flags (mined from man pages;
    // see manpages.rs), in any position, matched exactly.
    Flag,
    // The argument right after a command name, when it's a known subcommand:
    // `commit` in `git commit`. One level only.
    Subcommand,
    // An argument that isn't a flag or subcommand but names a file or
    // directory under the shell's cwd.
    Link,
    // A non-literal piece inside a string, layered over its span: printf's
    // `%s` in an argument, or a JSON escape.
    FormatSpecifier,
    // A command name that isn't a builtin, a function, on PATH, or (for a
    // name with `/`) an executable file. A valid command name gets no styling
    // at all.
    InvalidCommand,
    // Markdown's text styles. No colour of their own: the weight or slant is
    // the meaning.
    Emphasis,
    Strong,
    Struck,
    // A field name, such as a JSON key. Separate from Variable (`$foo`) but
    // the same default colour, since both name the thing beside them.
    Key,
}

/// A run of `text` that reads as one thing, in character offsets, which is
/// what `compose` indexes by.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
    pub kind: HighlightKind,
    // The URL for a Link span, for OSC 8 hyperlinks; `None` otherwise.
    pub link: Option<String>,
}

/// Spans are in character offsets, not bytes: `compose` paints over a
/// `&[char]`, and `lint::Diagnostic`, `url::find` and converted LSP
/// positions count the same way. `CodeHighlighter`'s scanners count bytes
/// and are converted at that seam.
pub trait Highlighter {
    fn highlight(&self, text: &str, ctx: HighlightContext) -> Vec<HighlightSpan>;
}

/// Rewrites byte offsets into character offsets in place, in one walk over
/// sorted offsets. An offset inside a character maps to that character
/// rather than panicking.
fn char_offsets(text: &str, spans: &mut [HighlightSpan]) {
    // ASCII: bytes and characters are the same.
    if text.is_ascii() {
        return;
    }
    let mut wanted: Vec<usize> = spans.iter().flat_map(|s| [s.start, s.end]).collect();
    crate::sort::sort(&mut wanted);
    wanted.dedup();
    let mut answer = vec![0usize; wanted.len()];
    let mut next = 0;
    let mut count = 0;
    for (index, (byte, ch)) in text.char_indices().enumerate() {
        let after = byte + ch.len_utf8();
        while next < wanted.len() && wanted[next] < after {
            answer[next] = index;
            next += 1;
        }
        count = index + 1;
    }
    answer[next..].fill(count);
    for span in spans.iter_mut() {
        span.start = answer[wanted.partition_point(|&w| w < span.start)];
        span.end = answer[wanted.partition_point(|&w| w < span.end)];
    }
}

pub struct BashHighlighter;

impl Highlighter for BashHighlighter {
    fn highlight(&self, text: &str, ctx: HighlightContext) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        highlight_into(text, 0, ctx, &mut out);
        out
    }
}

// JSON, over the `json::tokens` the `json` builtin's parser reads, so
// what's painted and what `json` accepts can't drift apart. It ignores
// `ctx`, and works from the flat token stream rather than a parsed value
// so it keeps going through invalid input. An invalid token gets no span:
// highlighting isn't diagnostics.
pub struct JsonHighlighter;

impl Highlighter for JsonHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        json_spans(&crate::json::tokens(text))
    }
}

// JSON with comments, as `tsconfig.json` and `.vscode` files are written,
// from `json::tokens_with_comments`.
pub struct JsoncHighlighter;

impl Highlighter for JsoncHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        json_spans(&crate::json::tokens_with_comments(text))
    }
}

fn json_spans(toks: &[crate::json::Token]) -> Vec<HighlightSpan> {
    let mut out = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        let kind = match &tok.kind {
            // A string is a key when a colon follows it, which works on the
            // flat stream in a buffer that doesn't parse yet.
            crate::json::TokenKind::Str(_) => {
                // Past any comment, so `"key" /* why */: 1` is still a key.
                let next = toks[i + 1..].iter().find(|t| t.kind != crate::json::TokenKind::Comment);
                match next.map(|t| &t.kind) {
                    Some(crate::json::TokenKind::Colon) => HighlightKind::Key,
                    _ => HighlightKind::String,
                }
            }
            crate::json::TokenKind::Number(_) => HighlightKind::Number,
            crate::json::TokenKind::Bool(_) | crate::json::TokenKind::Null => HighlightKind::Keyword,
            crate::json::TokenKind::OpenBrace
            | crate::json::TokenKind::CloseBrace
            | crate::json::TokenKind::OpenBracket
            | crate::json::TokenKind::CloseBracket
            | crate::json::TokenKind::Comma
            | crate::json::TokenKind::Colon => HighlightKind::Operator,
            crate::json::TokenKind::Comment => HighlightKind::Comment,
            crate::json::TokenKind::Invalid(_) => continue,
        };
        out.push(HighlightSpan { start: tok.start, end: tok.end, kind, link: None });
        // Over the string's span, as printf's specifiers sit over their
        // argument: `compose` paints later spans over earlier ones.
        for esc in &tok.escapes {
            out.push(HighlightSpan { start: esc.start, end: esc.end, kind: HighlightKind::FormatSpecifier, link: None });
        }
    }
    out
}

// Markdown, over `crate::markdown`'s parse, so the editor and a preview
// can't disagree. The spans come from the parse, which gets emphasis
// right where a scan gets `snake_case` and `a * b` wrong.
//
// A fenced code block is highlighted in its own language, by handing its
// text to the Highlighter its info string names and shifting the spans.
pub struct MarkdownHighlighter;

impl Highlighter for MarkdownHighlighter {
    fn highlight(&self, text: &str, ctx: HighlightContext) -> Vec<HighlightSpan> {
        let doc = crate::markdown::parse(text);
        let mut out = Vec::new();
        for block in &doc.blocks {
            markdown_block(block, text, ctx, &mut out);
        }
        // A link reference definition belongs to no block, so its span comes
        // from the document.
        for link_ref in &doc.link_refs {
            out.push(span(link_ref.span.clone(), HighlightKind::Link));
        }
        out
    }
}

fn span(range: std::ops::Range<usize>, kind: HighlightKind) -> HighlightSpan {
    HighlightSpan { start: range.start, end: range.end, kind, link: None }
}

fn markdown_block(block: &crate::markdown::Block, source: &str, ctx: HighlightContext, out: &mut Vec<HighlightSpan>) {
    use crate::markdown::Block;
    match block {
        Block::Heading { content, span: whole, .. } => {
            // The whole line, marker included: a heading is one thing.
            out.push(span(whole.clone(), HighlightKind::Keyword));
            markdown_inlines(content, out);
        }
        Block::Paragraph { content, .. } => markdown_inlines(content, out),
        Block::CodeBlock { info, info_span, literal_span, .. } => {
            out.push(span(literal_span.clone(), HighlightKind::String));
            out.push(span(info_span.clone(), HighlightKind::Variable));
            // ...then the block's own language painted over it.
            let lang = info.split_whitespace().next().unwrap_or("");
            if let Some(inner) = highlighter_for_language(lang) {
                let chars: Vec<char> = source.chars().collect();
                let start = literal_span.start.min(chars.len());
                let end = literal_span.end.min(chars.len());
                let text: String = chars[start..end].iter().collect();
                for s in inner.highlight(&text, ctx) {
                    out.push(HighlightSpan { start: start + s.start, end: start + s.end, kind: s.kind, link: s.link });
                }
            }
        }
        Block::HtmlBlock { span: whole, .. } => out.push(span(whole.clone(), HighlightKind::Comment)),
        Block::ThematicBreak { span: whole } => out.push(span(whole.clone(), HighlightKind::Comment)),
        Block::BlockQuote { blocks, .. } => {
            for b in blocks {
                markdown_block(b, source, ctx, out);
            }
        }
        Block::List(list) => {
            for item in &list.items {
                out.push(span(item.marker.clone(), HighlightKind::Keyword));
                for b in &item.blocks {
                    markdown_block(b, source, ctx, out);
                }
            }
        }
        Block::Table(table) => {
            for cell in table.head.iter().chain(table.rows.iter().flatten()) {
                markdown_inlines(cell, out);
            }
        }
    }
}

fn markdown_inlines(inlines: &[crate::markdown::Inline], out: &mut Vec<HighlightSpan>) {
    use crate::markdown::Inline;
    for inline in inlines {
        // Each construct's span first, then its children over it, so
        // `**a `b` c**` is bold with the code span still coloured.
        match inline {
            Inline::Code { span: s, .. } => out.push(span(s.clone(), HighlightKind::String)),
            Inline::Emph { content, span: s } => {
                out.push(span(s.clone(), HighlightKind::Emphasis));
                markdown_inlines(content, out);
            }
            Inline::Strong { content, span: s } => {
                out.push(span(s.clone(), HighlightKind::Strong));
                markdown_inlines(content, out);
            }
            Inline::Strikethrough { content, span: s } => {
                out.push(span(s.clone(), HighlightKind::Struck));
                markdown_inlines(content, out);
            }
            Inline::Link { content, span: s, dest, .. } => {
                let mut hs = span(s.clone(), HighlightKind::Link);
                hs.link = Some(dest.clone());
                out.push(hs);
                markdown_inlines(content, out);
            }
            Inline::Image { alt, span: s, dest, .. } => {
                let mut hs = span(s.clone(), HighlightKind::Link);
                hs.link = Some(dest.clone());
                out.push(hs);
                markdown_inlines(alt, out);
            }
            Inline::Html { span: s, .. } => out.push(span(s.clone(), HighlightKind::Comment)),
            Inline::Text { .. } | Inline::SoftBreak { .. } | Inline::HardBreak { .. } => {}
        }
    }
}

// `.env` files, from `dotenv::parse`'s items. An interpolation
// (`$DATABASE_URL`) is the one part of a line that isn't what it says, so
// it gets `Variable`, as a shell expansion does.
pub struct DotenvHighlighter;

impl Highlighter for DotenvHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        for item in &crate::dotenv::parse(text).items {
            match item {
                crate::dotenv::Item::Comment { span: s } => out.push(span(s.clone(), HighlightKind::Comment)),
                crate::dotenv::Item::Entry { export, key, separator, value, comment, .. } => {
                    if let Some(export) = export {
                        out.push(span(export.clone(), HighlightKind::Keyword));
                    }
                    out.push(span(key.clone(), HighlightKind::Key));
                    if let Some(at) = separator {
                        out.push(span(*at..*at + 1, HighlightKind::Operator));
                    }
                    if let Some(value) = value {
                        dotenv_value(value, &mut out);
                    }
                    if let Some(comment) = comment {
                        out.push(span(comment.clone(), HighlightKind::Comment));
                    }
                }
                crate::dotenv::Item::Blank { .. } | crate::dotenv::Item::Junk { .. } => {}
            }
        }
        out
    }
}

fn dotenv_value(value: &crate::dotenv::Value, out: &mut Vec<HighlightSpan>) {
    let base = match value.kind {
        crate::dotenv::ValueKind::Quoted | crate::dotenv::ValueKind::Literal => Some(HighlightKind::String),
        crate::dotenv::ValueKind::Number => Some(HighlightKind::Number),
        crate::dotenv::ValueKind::Bool => Some(HighlightKind::Keyword),
        // Most values are a path, a URL or a token; colouring them would only
        // make the file louder.
        crate::dotenv::ValueKind::Bare => None,
    };
    if let Some(base) = base {
        out.push(span(value.span.clone(), base));
    }
    // Over the value's own kind, as a JSON string's escapes sit over it.
    for esc in &value.escapes {
        out.push(span(esc.clone(), HighlightKind::FormatSpecifier));
    }
    for expansion in &value.expansions {
        out.push(span(expansion.clone(), HighlightKind::Variable));
    }
}

// TOML, straight from `toml::tokens`, which already knows whether a word
// is in key or value position (see toml.rs).
pub struct TomlHighlighter;

impl Highlighter for TomlHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        for tok in crate::toml::tokens(text) {
            let kind = match &tok.kind {
                crate::toml::TokenKind::Comment => HighlightKind::Comment,
                // The whole header, brackets included: a table is one thing.
                crate::toml::TokenKind::TableHeader => HighlightKind::Keyword,
                crate::toml::TokenKind::Key => HighlightKind::Key,
                crate::toml::TokenKind::Str { escapes } => {
                    out.push(span(tok.start..tok.end, HighlightKind::String));
                    for esc in escapes {
                        out.push(span(esc.clone(), HighlightKind::FormatSpecifier));
                    }
                    continue;
                }
                crate::toml::TokenKind::Number => HighlightKind::Number,
                crate::toml::TokenKind::Bool => HighlightKind::Keyword,
                // A date is an unquoted literal scalar and reads like the
                // numbers beside it; there's no narrower kind.
                crate::toml::TokenKind::DateTime => HighlightKind::Number,
                crate::toml::TokenKind::Punctuation => HighlightKind::Operator,
                // Uncoloured, as JSON's Invalid is.
                crate::toml::TokenKind::Invalid => continue,
            };
            out.push(span(tok.start..tok.end, kind));
        }
        out
    }
}

// INI and the formats that are INI in all but name, from `ini::parse`'s
// items, so the highlighter can't disagree with the parser. An INI parse
// never fails; see ini.rs for the dialect choices.
//
// Only numbers, booleans and quoted strings are coloured: most values (a
// path, a command line, a URL) are arbitrary text.
pub struct IniHighlighter;

impl Highlighter for IniHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        let mut out = Vec::new();
        for item in &crate::ini::parse(text).items {
            match item {
                crate::ini::Item::Comment { span: s } => out.push(span(s.clone(), HighlightKind::Comment)),
                crate::ini::Item::Section { sub, span: s, .. } => {
                    // The whole header, brackets included: a section is one
                    // thing.
                    out.push(span(s.clone(), HighlightKind::Keyword));
                    // Over it: a subsection is a string inside the header.
                    if let Some(sub) = sub {
                        out.push(span(sub.clone(), HighlightKind::String));
                    }
                }
                crate::ini::Item::Entry { key, separator, value, .. } => {
                    out.push(span(key.clone(), HighlightKind::Key));
                    if let Some(at) = separator {
                        out.push(span(*at..*at + 1, HighlightKind::Operator));
                    }
                    if let Some(value) = value {
                        ini_value(value, &mut out);
                    }
                }
                crate::ini::Item::Continuation { value, .. } => ini_value(value, &mut out),
                crate::ini::Item::Blank { .. } => {}
            }
        }
        out
    }
}

fn ini_value(value: &crate::ini::Value, out: &mut Vec<HighlightSpan>) {
    let kind = match value.kind {
        crate::ini::ValueKind::Quoted => HighlightKind::String,
        crate::ini::ValueKind::Number => HighlightKind::Number,
        crate::ini::ValueKind::Bool => HighlightKind::Keyword,
        crate::ini::ValueKind::Plain => return,
    };
    out.push(span(value.span.clone(), kind));
    // Over the string's span, as JSON escapes are.
    for esc in &value.escapes {
        out.push(span(esc.clone(), HighlightKind::FormatSpecifier));
    }
}

// roff (man page source), over `crate::roff`'s lexical pass rather than
// its interpreter: a highlighter colours the source as written, so the
// `.de` and `.ie` scaffolding a generated page opens with stays visible
// rather than being executed away.
pub struct RoffHighlighter;

impl Highlighter for RoffHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        use crate::roff::lexer::{TokenKind, lex};
        lex(text)
            .into_iter()
            .filter_map(|token| {
                let kind = match token.kind {
                    TokenKind::Comment => HighlightKind::Comment,
                    // The `.` that makes a line a request, and the request:
                    // roff's only syntax.
                    TokenKind::Control => HighlightKind::Operator,
                    TokenKind::Request => HighlightKind::Keyword,
                    // `\fB`, `\(bu`, `\*[x]`: substitutions within the text.
                    TokenKind::Escape => HighlightKind::Substitution,
                    TokenKind::Argument => HighlightKind::String,
                    TokenKind::Text => return None,
                };
                Some(span(token.span, kind))
            })
            .collect()
    }
}

// The highlighter for a language name, shared by the file editor and by
// fenced code blocks in markdown.
pub fn highlighter_for_language(language: &str) -> Option<Box<dyn Highlighter>> {
    match language {
        "bash" | "sh" | "shell" => Some(Box::new(BashHighlighter)),
        "json" => Some(Box::new(JsonHighlighter)),
        "jsonc" => Some(Box::new(JsoncHighlighter)),
        "markdown" | "md" => Some(Box::new(MarkdownHighlighter)),
        "roff" | "man" | "troff" | "groff" => Some(Box::new(RoffHighlighter)),
        "ini" => Some(Box::new(IniHighlighter)),
        "toml" => Some(Box::new(TomlHighlighter)),
        "dotenv" => Some(Box::new(DotenvHighlighter)),
        // Token scanners rather than parsers; see codehighlight.rs.
        "rust" => Some(Box::new(CodeHighlighter(super::codehighlight::rust))),
        "python" => Some(Box::new(CodeHighlighter(super::codehighlight::python))),
        "javascript" => Some(Box::new(CodeHighlighter(super::codehighlight::javascript))),
        "typescript" => Some(Box::new(CodeHighlighter(super::codehighlight::typescript))),
        "go" | "golang" => Some(Box::new(CodeHighlighter(super::codehighlight::go))),
        "yaml" | "yml" => Some(Box::new(CodeHighlighter(super::codehighlight::yaml))),
        "css" => Some(Box::new(CodeHighlighter(super::codehighlight::css))),
        "html" | "htm" => Some(Box::new(CodeHighlighter(super::codehighlight::html))),
        _ => None,
    }
}

// Adapts codehighlight.rs's scanners, which have no use for `ctx`.
pub struct CodeHighlighter(CodeScanner);

type CodeScanner = fn(&str) -> Vec<(std::ops::Range<usize>, HighlightKind)>;

impl Highlighter for CodeHighlighter {
    fn highlight(&self, text: &str, _ctx: HighlightContext) -> Vec<HighlightSpan> {
        // Scanners over a `&str` produce byte offsets, and everything
        // downstream counts characters; the two agree until the text isn't
        // ASCII. Converted here once rather than in each scanner.
        let mut spans: Vec<HighlightSpan> = (self.0)(text).into_iter().map(|(range, kind)| span(range, kind)).collect();
        char_offsets(text, &mut spans);
        spans
    }
}

// A span resolved to a colour and attributes. Separate from
// HighlightSpan so other layers (selections, search matches, diffs,
// completions) can build their own and hand them to `compose` without
// knowing about HighlightKind.
#[derive(Debug, Clone, PartialEq)]
pub struct StyledSpan {
    pub start: usize,
    pub end: usize,
    pub fg: vt100::Color,
    pub attrs: vt100::CellAttrs,
}

// A run that is a hyperlink, for OSC 8. Not a field on `StyledSpan`
// because a link composes with the text's styling rather than replacing
// it, and nearly every StyledSpan has nothing to say about links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

// OSC 8: `ESC ] 8 ; params ; URI ST` opens a link and the same with an
// empty URI closes it. A terminal ignores an OSC it doesn't know and
// prints the text either way, so no capability check is needed.
fn osc8_open(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}

const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

// Low-8 ANSI colours, matching prompt.rs, rather than RGB, which would
// need to know whether the background is light or dark.
pub fn default_style(kind: HighlightKind) -> (vt100::Color, vt100::CellAttrs) {
    let bold = vt100::CellAttrs { bold: true, ..vt100::CellAttrs::default() };
    let dim = vt100::CellAttrs { dim: true, ..vt100::CellAttrs::default() };
    let underline = vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() };
    match kind {
        HighlightKind::Keyword => (vt100::Color::Indexed(3), bold),
        HighlightKind::String => (vt100::Color::Indexed(2), vt100::CellAttrs::default()),
        HighlightKind::Variable => (vt100::Color::Indexed(6), bold),
        HighlightKind::Substitution => (vt100::Color::Indexed(4), bold),
        HighlightKind::Redirect => (vt100::Color::Indexed(5), bold),
        HighlightKind::Operator => (vt100::Color::Indexed(7), vt100::CellAttrs::default()),
        HighlightKind::Comment => (vt100::Color::Indexed(8), dim),
        HighlightKind::Number => (vt100::Color::Indexed(6), vt100::CellAttrs::default()),
        // Weight only, so flags don't compete with the grammar's colours.
        HighlightKind::Flag => (vt100::Color::Default, bold),
        HighlightKind::Subcommand => (vt100::Color::Default, bold),
        HighlightKind::Link => (vt100::Color::Default, underline),
        HighlightKind::FormatSpecifier => (vt100::Color::Indexed(1), bold),
        // Plain red, not bold; FormatSpecifier's bold red never appears
        // near it.
        HighlightKind::InvalidCommand => (vt100::Color::Indexed(1), vt100::CellAttrs::default()),
        HighlightKind::Key => (vt100::Color::Indexed(6), bold),
        HighlightKind::Emphasis => (vt100::Color::Default, vt100::CellAttrs { italic: true, ..vt100::CellAttrs::default() }),
        HighlightKind::Strong => (vt100::Color::Default, bold),
        HighlightKind::Struck => (vt100::Color::Default, vt100::CellAttrs { strikethrough: true, ..vt100::CellAttrs::default() }),
    }
}

// A session's foreground overrides by HighlightKind, from `::bish hl`
// (see `Shell::hl` and `HL_NAMES`). Built by a caller with a live Shell
// (repl.rs's prompt loop); this module doesn't know about bishopts, so a
// non-shell consumer needn't bring exec.rs along.
pub type ColorOverrides = std::collections::HashMap<HighlightKind, vt100::Color>;

// `default_style`, with the foreground from `overrides` if set. The
// attributes always come from `default_style`: a colour can't express
// weight. `None` (no live Shell, e.g. fileeditor.rs) is `default_style`.
pub fn resolve_style(kind: HighlightKind, overrides: Option<&ColorOverrides>) -> (vt100::Color, vt100::CellAttrs) {
    let (default_fg, attrs) = default_style(kind);
    let fg = overrides.and_then(|o| o.get(&kind)).copied().unwrap_or(default_fg);
    (fg, attrs)
}

// The `::bish hl` name for each colourable kind. Not everything that
// command accepts: its namespace is open, so a language server's semantic
// token types are settable before anything here knows them.
//
// Flag, Subcommand and Link draw in the terminal's foreground, and are
// here anyway: an unset name produces no override (see repl.rs's
// `hl_colors`), which is the default, so naming one is what lets a theme
// colour it. `ui_col_*` (theme.rs) is a closed registry with a parseable
// default per option, which is why LineNumber and Divider have no option
// there.
//
// Markdown's Emphasis, Strong and Struck stay out: their weight or slant is
// their meaning.
pub const HL_NAMES: &[(HighlightKind, &str)] = &[
    (HighlightKind::Keyword, "keyword"),
    (HighlightKind::Operator, "operator"),
    (HighlightKind::Redirect, "redirect"),
    (HighlightKind::String, "string"),
    (HighlightKind::Variable, "variable"),
    (HighlightKind::Substitution, "substitution"),
    (HighlightKind::Comment, "comment"),
    (HighlightKind::Number, "number"),
    (HighlightKind::FormatSpecifier, "format_specifier"),
    (HighlightKind::InvalidCommand, "invalid_command"),
    (HighlightKind::Key, "key"),
    (HighlightKind::Flag, "flag"),
    (HighlightKind::Subcommand, "subcommand"),
    (HighlightKind::Link, "link"),
];

// The kind a language server's semantic token type falls back to when
// `::bish hl` says nothing about that name. Partial on purpose: a type
// with no bish equivalent (`function`, `class`, `typeParameter`) gets
// nothing, keeping the local highlighter's answer until
// `::bish hl --set function ...` colours it.
pub fn kind_for_semantic_type(name: &str) -> Option<HighlightKind> {
    Some(match name {
        "keyword" | "modifier" => HighlightKind::Keyword,
        "string" | "regexp" => HighlightKind::String,
        "comment" => HighlightKind::Comment,
        "number" => HighlightKind::Number,
        "operator" => HighlightKind::Operator,
        // A parameter, property and enum member all stand for a value.
        "variable" | "parameter" | "property" | "enumMember" => HighlightKind::Variable,
        _ => return None,
    })
}

// One cell per char, then each layer's spans painted over it in order: a
// later layer, or a later span within one, wins. A plain function, so
// adding a layer is passing one more slice.
pub fn compose(chars: &[char], layers: &[&[StyledSpan]]) -> Vec<vt100::Cell> {
    let mut cells: Vec<vt100::Cell> = chars.iter().map(|&ch| vt100::Cell { ch, ..vt100::Cell::default() }).collect();
    for layer in layers {
        for span in layer.iter() {
            let end = span.end.min(cells.len());
            for cell in cells.iter_mut().take(end).skip(span.start) {
                cell.fg = span.fg;
                cell.attrs = span.attrs;
            }
        }
    }
    cells
}

/// Adds `spans`' attributes to composed cells, keeping every colour.
/// A `StyledSpan` replaces what's under it, which suits a selection, but a
/// mark like "same symbol as under the cursor" should add to the colour,
/// not strip it. Attributes are OR-ed, so underlined inside bold is both.
pub fn compose_attrs(cells: &mut [vt100::Cell], spans: &[StyledSpan]) {
    for span in spans {
        let end = span.end.min(cells.len());
        for cell in cells.iter_mut().take(end).skip(span.start) {
            cell.attrs.bold |= span.attrs.bold;
            cell.attrs.dim |= span.attrs.dim;
            cell.attrs.italic |= span.attrs.italic;
            cell.attrs.underline |= span.attrs.underline;
            cell.attrs.reverse |= span.attrs.reverse;
            cell.attrs.strikethrough |= span.attrs.strikethrough;
        }
    }
}

// Turns cells back into an SGR-coded string with `vt100::sgr_codes`, the
// run-coalescing repl.rs's `render_row` uses.
pub fn render_styled(cells: &[vt100::Cell]) -> String {
    render_linked(cells, &[])
}

// The same, with OSC 8 links opened and closed around the runs `links`
// covers, on the transition, so a link costs two sequences however long.
pub fn render_linked(cells: &[vt100::Cell], links: &[LinkSpan]) -> String {
    let mut out = String::new();
    let mut last: Option<(vt100::Color, vt100::Color, vt100::CellAttrs)> = None;
    let mut open: Option<&str> = None;
    for (i, cell) in cells.iter().enumerate() {
        let url = links.iter().find(|l| i >= l.start && i < l.end && crate::url::is_safe(&l.url)).map(|l| l.url.as_str());
        if url != open {
            if open.is_some() {
                out.push_str(OSC8_CLOSE);
            }
            if let Some(url) = url {
                out.push_str(&osc8_open(url));
            }
            open = url;
        }
        let key = (cell.fg, cell.bg, cell.attrs);
        if last != Some(key) {
            out.push_str(&vt100::sgr_codes(cell.fg, cell.bg, cell.attrs));
            last = Some(key);
        }
        // The last gate before text becomes terminal bytes. These cells
        // come from text bish didn't write (a filename in a completion
        // menu, a line of a file), and a control character in one would be
        // spliced into the escape stream: a file named `evil<ESC>[2J.txt`
        // would clear the screen when offered.
        out.push(crate::term::safe_char(cell.ch));
    }
    if open.is_some() {
        out.push_str(OSC8_CLOSE);
    }
    out.push_str("\x1b[0m");
    out
}

// A finished source line, highlighted, as an SGR-coded string ending in
// a reset: the same pipeline as editor.rs's `compose_redraw` without the
// live-edit layers. For echoing a submitted line (into a promoted pane's
// grid, say) so it keeps looking as it did while typed.
pub fn render_line(text: &str, ctx: HighlightContext) -> String {
    let chars: Vec<char> = text.chars().collect();
    let styled: Vec<StyledSpan> = BashHighlighter
        .highlight(text, ctx)
        .into_iter()
        .map(|s| {
            let (fg, attrs) = resolve_style(s.kind, ctx.color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();
    let cells = compose(&chars, &[&styled]);
    render_styled(&cells)
}

// The next `raw_capture_spans` entry, if any. `.get()` because the spans
// can desync from the tokens after a heredoc body, whose positions aren't
// tracked (see lexer.rs). That needs a multi-line `text`, which this is
// never called with, but a few wrong spans beat a panic.
fn next_span(raw_spans: &[Range<usize>], cursor: &mut usize) -> Option<Range<usize>> {
    let r = raw_spans.get(*cursor).cloned();
    *cursor += 1;
    r
}

// Lexes `text` (a command line, or the interior of a `$(...)`,
// backticks, `<(...)` or `>(...)`) and appends its spans to `out`,
// shifted by `offset` into the caller's coordinates. Each level re-derives
// its own `chars`, since `raw_capture_spans` positions are relative to the
// text they came from.
fn highlight_into(text: &str, offset: usize, ctx: HighlightContext, out: &mut Vec<HighlightSpan>) {
    let chars: Vec<char> = text.chars().collect();
    let res = lexer::tokenize_spanned(text);
    let mut cursor = 0usize;
    // Every level starts a new command: `git commit $(echo -m)` begins
    // afresh inside the substitution.
    let mut cmd_pos = CmdPos::ExpectCommand;
    for i in 0..res.items.len() {
        match &res.items[i] {
            SpannedItem::Comment(r) => {
                out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Comment, link: None });
            }
            SpannedItem::Tok(tok, span) => {
                // A command name followed by an empty `()` is a function
                // definition, and the name isn't invoked (it isn't even
                // defined yet), so it isn't checked. Detected by lookahead:
                // at worst a line that only looks like a definition skips a
                // check, never gets a false "invalid".
                let is_function_def_name = matches!(cmd_pos, CmdPos::ExpectCommand)
                    && matches!(tok, Tok::Word(chunks, _) if matches!(chunks.as_slice(), [Chunk::Str(_)]))
                    && matches!(res.items.get(i + 1), Some(SpannedItem::Tok(Tok::Subshell { raw, .. }, _)) if raw.is_empty());
                highlight_tok(tok, span, offset, &chars, &res.raw_capture_spans, &mut cursor, ctx, &mut cmd_pos, is_function_def_name, out);
            }
        }
    }
}

// Which command-name/argument-position state a word is in, resolved from
// `CmdPos` right before it's processed.
enum WordRole {
    // A leading `NAME=value` or `NAME+=value` before the command name.
    AssignmentPrefix,
    CommandName,
    // `command` is the command name when it was a plain word (`None` for
    // `$CMD arg`). `arg_index` counts arguments from 0, excluding the name.
    Argument { command: Option<String>, arg_index: usize },
}

#[derive(Clone)]
enum CmdPos {
    ExpectCommand,
    InCommand { name: Option<String>, arg_index: usize },
}

// Whether a new simple command can start after this token. Closers
// (`}`, `)`, `fi`, `done`, `[[`, `]]`) don't reset: a new command still
// needs a separator after them. Its own function so the rule can be
// tested directly.
pub(crate) fn resets_command_position(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Pipe
            | Tok::And
            | Tok::Or
            | Tok::Semi
            | Tok::DSemi
            | Tok::SemiAmp
            | Tok::DSemiAmp
            | Tok::Amp
            | Tok::LBrace
            | Tok::Newline
            | Tok::KwIf
            | Tok::KwThen
            | Tok::KwElif
            | Tok::KwElse
            | Tok::KwDo
            | Tok::KwWhile
            | Tok::KwUntil
            | Tok::KwFor
            | Tok::KwSelect
            | Tok::KwCoproc
            | Tok::KwIn
            | Tok::KwCase
            | Tok::KwEsac
            | Tok::KwFunction
    )
}

fn is_valid_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// The `NAME=value` shape, checked on chunks rather than with parser.rs's
// `word_as_assignment`, which works on a parsed Word.
pub(crate) fn is_assignment_prefix_word(chunks: &[Chunk]) -> bool {
    let [Chunk::Str(s)] = chunks else { return false };
    let Some(eq) = s.find('=') else { return false };
    let name = s[..eq].strip_suffix('+').unwrap_or(&s[..eq]);
    is_valid_ident(name)
}

// A copy of exec.rs's `KNOWN_BUILTINS`, keeping the editor's analysis
// independent of the execution engine.
pub(crate) const KNOWN_BUILTINS: &[&str] = &[
    ":",
    "cd",
    "e",
    "export",
    "let",
    "break",
    "continue",
    "test",
    "[",
    "[[",
    "return",
    "shift",
    "local",
    "exit",
    "logout",
    "read",
    "mapfile",
    "readarray",
    "eval",
    "source",
    ".",
    "trap",
    "jobs",
    "disown",
    "fg",
    "bg",
    "wait",
    "kill",
    "getopts",
    "unset",
    "set",
    "declare",
    "typeset",
    "readonly",
    "exec",
    "command",
    "builtin",
    "type",
    "hash",
    "shopt",
    "umask",
    "pushd",
    "popd",
    "dirs",
    "ulimit",
    "alias",
    "unalias",
    "abbr",
    "bishopt",
    "compgen",
    "complete",
    "compopt",
    "::bish",
    "echo",
    "printf",
];

// Valid: a builtin, one of the line's own commands, a session function,
// an executable file (for a name with `/`), or on PATH. Not aliases.
fn is_valid_command_name(name: &str, ctx: &HighlightContext) -> bool {
    if KNOWN_BUILTINS.contains(&name) {
        return true;
    }
    if ctx.line_commands.is_some_and(|c| c.contains(&name)) {
        return true;
    }
    if ctx.known_functions.is_some_and(|f| f.contains(name)) {
        return true;
    }
    if name.contains('/') {
        return is_executable_file(Path::new(name));
    }
    is_in_path(name, ctx.path)
}

#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

// The PATH half of exec.rs's `resolve_in_path`, which is private and also
// handles names with `/`. Not cached, like the real spawn path.
fn is_in_path(name: &str, path_var: Option<&str>) -> bool {
    let inherited;
    let path_var = match path_var {
        Some(p) => p,
        None => {
            let Ok(v) = std::env::var("PATH") else { return false };
            inherited = v;
            &inherited
        }
    };
    path_var.split(':').any(|dir| is_executable_file(&Path::new(dir).join(name)))
}

// Every executable name on `path_var` (the session's PATH; `None` falls
// back to the environment's) that starts with `prefix`, deduplicated.
// Filtered here so a keystroke doesn't score every executable on the
// system. A directory that won't open is skipped, as in PATH resolution.
pub(crate) fn enumerate_path_matches(prefix: &str, path_var: Option<&str>) -> Vec<String> {
    let inherited;
    let path_var = match path_var {
        Some(p) => p,
        None => {
            let Ok(v) = std::env::var("PATH") else { return Vec::new() };
            inherited = v;
            &inherited
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in path_var.split(':') {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !name.starts_with(prefix) {
                continue;
            }
            if !is_executable_file(&entry.path()) {
                continue;
            }
            if seen.insert(name.clone()) {
                out.push(name);
            }
        }
    }
    out
}

// Resolves `command`'s man-page data if ready (`manpages::query` never
// blocks) and delegates to `classify_plain_argument_core`, which tests call
// with canned data.
fn classify_plain_argument(
    text: &str,
    word_span: &Range<usize>,
    command: &str,
    arg_index: usize,
    ctx: HighlightContext,
    offset: usize,
) -> Option<HighlightSpan> {
    let man = match manpages::query(command) {
        manpages::ManStatus::Ready(data) => Some(data),
        manpages::ManStatus::Pending | manpages::ManStatus::Missing => None,
    };
    classify_plain_argument_core(text, word_span, arg_index, man.as_deref(), ctx.cwd, offset)
}

// A `-` word is checked only against flags, in any position, and never
// falls through, since it isn't meaningfully a path either. Subcommands
// only at `arg_index == 0`, one level. Anything else may be a file link.
fn classify_plain_argument_core(
    text: &str,
    word_span: &Range<usize>,
    arg_index: usize,
    man: Option<&manpages::ManPageData>,
    cwd: Option<&Path>,
    offset: usize,
) -> Option<HighlightSpan> {
    let out_span =
        |kind: HighlightKind, link: Option<String>| HighlightSpan { start: offset + word_span.start, end: offset + word_span.end, kind, link };

    if text.starts_with('-') {
        let flag_text = strip_flag_suffix(text);
        if let Some(man) = man {
            if man.flags.iter().any(|f| f == flag_text) {
                return Some(out_span(HighlightKind::Flag, None));
            }
        }
        return None;
    }

    if arg_index == 0 {
        if let Some(man) = man {
            if man.subcommands.iter().any(|s| s == text) {
                return Some(out_span(HighlightKind::Subcommand, None));
            }
        }
    }

    resolve_file_link(text, cwd).map(|url| out_span(HighlightKind::Link, Some(url)))
}

// Resolves `text` (plain and unquoted; a leading `~` always becomes a
// `Chunk::Var`, so never that) against `cwd` and checks that it exists.
// `cwd: None` (the colon line) skips links rather than guessing with the
// process's cwd, which can differ from the session's. Not cached: the
// filesystem changes.
fn resolve_file_link(text: &str, cwd: Option<&Path>) -> Option<String> {
    let cwd = cwd?;
    let candidate = if Path::new(text).is_absolute() { std::path::PathBuf::from(text) } else { cwd.join(text) };
    if candidate.exists() { Some(format!("file://{}", candidate.display())) } else { None }
}

// Strips a trailing `=value` or ` <placeholder>` from a flag. Matching is
// exact: `-la` is never split into `-l` and `-a`.
fn strip_flag_suffix(text: &str) -> &str {
    let end = text.find(|c: char| c == '=' || c == ' ').unwrap_or(text.len());
    &text[..end]
}

fn builtin_refine(command: &str, arg_index: usize, raw_text: &[char]) -> Vec<(Range<usize>, HighlightKind)> {
    match command {
        // printf's format is its first argument, except after `-v NAME`,
        // which this doesn't detect; `-v` itself has no `%` to match.
        "printf" if arg_index == 0 => scan_printf_directives(raw_text),
        _ => Vec::new(),
    }
}

// exec.rs's `printf_format_once` grammar, not full C printf: flags `-`
// and `0`, a width, a `.` precision, and one of `s b c q d i u o x X`, or
// `%%`. An unknown conversion gets no span, as `printf_format_once` prints
// it literally; `i` is already past the `%`, so this can't loop.
fn scan_printf_directives(text: &[char]) -> Vec<(Range<usize>, HighlightKind)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] != '%' {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        if i < text.len() && text[i] == '%' {
            i += 1;
            out.push((start..i, HighlightKind::FormatSpecifier));
            continue;
        }
        while i < text.len() && matches!(text[i], '-' | '0') {
            i += 1;
        }
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
        if i < text.len() && text[i] == '.' {
            i += 1;
            while i < text.len() && text[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < text.len() && matches!(text[i], 's' | 'b' | 'c' | 'q' | 'd' | 'i' | 'u' | 'o' | 'x' | 'X') {
            i += 1;
            out.push((start..i, HighlightKind::FormatSpecifier));
        }
    }
    out
}

fn highlight_tok(
    tok: &Tok,
    span: &Range<usize>,
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    ctx: HighlightContext,
    cmd_pos: &mut CmdPos,
    is_function_def_name: bool,
    out: &mut Vec<HighlightSpan>,
) {
    if resets_command_position(tok) {
        *cmd_pos = CmdPos::ExpectCommand;
    }
    // `for`/`select`'s NAME, `case`'s WORD and anything after `in` (a word
    // list or, indistinguishably here, a case pattern) are never commands.
    // They get the inert "no command in scope" argument role rather than
    // ExpectCommand, which would flag them as invalid commands.
    if matches!(tok, Tok::KwFor | Tok::KwSelect | Tok::KwCase | Tok::KwIn) {
        *cmd_pos = CmdPos::InCommand { name: None, arg_index: 0 };
    }
    let whole = |kind: HighlightKind| HighlightSpan { start: offset + span.start, end: offset + span.end, kind, link: None };
    match tok {
        Tok::KwIf
        | Tok::KwThen
        | Tok::KwElif
        | Tok::KwElse
        | Tok::KwFi
        | Tok::KwWhile
        | Tok::KwUntil
        | Tok::KwDo
        | Tok::KwDone
        | Tok::KwFor
        | Tok::KwSelect
        | Tok::KwCoproc
        | Tok::KwIn
        | Tok::KwCase
        | Tok::KwEsac
        | Tok::KwFunction
        | Tok::KwLBracket2
        | Tok::KwRBracket2 => out.push(whole(HighlightKind::Keyword)),

        Tok::Pipe
        | Tok::And
        | Tok::Or
        | Tok::Semi
        | Tok::DSemi
        | Tok::SemiAmp
        | Tok::DSemiAmp
        | Tok::Amp
        | Tok::LBrace
        | Tok::RBrace
        | Tok::RParen => out.push(whole(HighlightKind::Operator)),

        Tok::RedirOut { .. }
        | Tok::RedirIn
        | Tok::RedirInOut
        | Tok::RedirErr { .. }
        | Tok::RedirBoth { .. }
        | Tok::DupErrToOut
        | Tok::RedirFdOut { .. }
        | Tok::RedirVarFd { .. }
        | Tok::RedirFdIn { .. }
        | Tok::RedirFdInOut { .. }
        | Tok::RedirFdDup { .. }
        | Tok::RedirDupWord { .. }
        | Tok::RedirFdClose { .. }
        | Tok::RedirFdHere { .. }
        | Tok::HereString
        | Tok::HereDoc(_, _) => out.push(whole(HighlightKind::Redirect)),

        Tok::Newline => {}

        // A `(...)` subshell: the parens stay uncoloured, unlike
        // `Chunk::Sub`'s delimiters, and the interior is highlighted.
        Tok::Subshell { raw, .. } => {
            if let Some(inner) = next_span(raw_spans, cursor) {
                highlight_into(raw, offset + inner.start, ctx, out);
            }
        }

        // A bare `((...))`: one Substitution span over the whole token, not
        // recursed into, since `<` and `>` are comparisons there.
        Tok::Arith(_raw) => {
            next_span(raw_spans, cursor); // capture_double_paren's own capture_balanced_parens push
            out.push(whole(HighlightKind::Substitution));
        }

        Tok::Word(chunks, _plain) => highlight_word(chunks, span, offset, chars, raw_spans, cursor, ctx, cmd_pos, is_function_def_name, out),
    }
}

// A wholly plain word (one `Chunk::Str`) goes to
// `classify_plain_argument` for flags, subcommands and file links.
// Otherwise each chunk is walked in order, taking one `raw_capture_spans`
// entry per non-Str chunk (see lexer.rs), and literal strings are offered
// to `builtin_refine`.
//
// `cmd_pos` becomes this word's role first, since both paths need it: a
// leading `NAME=value`, the command name, or the Nth argument.
fn highlight_word(
    chunks: &[Chunk],
    word_span: &Range<usize>,
    offset: usize,
    chars: &[char],
    raw_spans: &[Range<usize>],
    cursor: &mut usize,
    ctx: HighlightContext,
    cmd_pos: &mut CmdPos,
    is_function_def_name: bool,
    out: &mut Vec<HighlightSpan>,
) {
    let role = match cmd_pos {
        CmdPos::ExpectCommand if is_assignment_prefix_word(chunks) => WordRole::AssignmentPrefix,
        CmdPos::ExpectCommand => {
            let name = if let [Chunk::Str(s)] = chunks { Some(s.clone()) } else { None };
            *cmd_pos = CmdPos::InCommand { name, arg_index: 0 };
            WordRole::CommandName
        }
        CmdPos::InCommand { name, arg_index } => {
            let role = WordRole::Argument { command: name.clone(), arg_index: *arg_index };
            *arg_index += 1;
            role
        }
    };

    if let [Chunk::Str(s)] = chunks {
        match &role {
            // A valid command name gets no span. A function definition's
            // name isn't an invocation, so it isn't checked.
            WordRole::CommandName => {
                if !is_function_def_name && !is_valid_command_name(s, &ctx) {
                    out.push(HighlightSpan {
                        start: offset + word_span.start,
                        end: offset + word_span.end,
                        kind: HighlightKind::InvalidCommand,
                        link: None,
                    });
                }
            }
            WordRole::Argument { command: Some(cmd), arg_index } => {
                if let Some(span) = classify_plain_argument(s, word_span, cmd, *arg_index, ctx, offset) {
                    out.push(span);
                }
            }
            WordRole::Argument { command: None, .. } | WordRole::AssignmentPrefix => {}
        }
        return;
    }
    for chunk in chunks {
        match chunk {
            Chunk::Str(_) | Chunk::Tilde { .. } => {}

            Chunk::LiteralStr(_, _) => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::String, link: None });
                    if let WordRole::Argument { command: Some(cmd), arg_index } = &role {
                        let raw_slice = &chars[r.start..r.end];
                        for (sub, kind) in builtin_refine(cmd, *arg_index, raw_slice) {
                            out.push(HighlightSpan { start: offset + r.start + sub.start, end: offset + r.start + sub.end, kind, link: None });
                        }
                    }
                }
            }

            // No recursion. The span covers the name, index or operator,
            // not the `$` or the braces: `$NAME` and `${NAME}` can't be
            // told apart from the chunk alone.
            Chunk::Var { .. }
            | Chunk::VarExpand { .. }
            | Chunk::ArrayVar { .. }
            | Chunk::ArrayLength { .. }
            | Chunk::ArrayVarExpand { .. }
            | Chunk::Indirect { .. }
            | Chunk::ArrayKeys { .. }
            | Chunk::VarNamesMatchingPrefix { .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    out.push(HighlightSpan { start: offset + r.start, end: offset + r.end, kind: HighlightKind::Variable, link: None });
                }
            }

            // `$(...)` or backticks: `Chunk::Sub` doesn't say which, so the
            // char before the interior decides: a backtick is a 1-char
            // delimiter each side, anything else `$(` and `)`.
            Chunk::Sub { raw, .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    let is_backtick = r.start >= 1 && chars.get(r.start - 1) == Some(&'`');
                    let delim_start = if is_backtick { r.start - 1 } else { r.start.saturating_sub(2) };
                    out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution, link: None });
                    out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution, link: None });
                    highlight_into(raw, offset + r.start, ctx, out);
                }
            }

            // `$((...))` in a word: flat like `((...))`, with the
            // delimiters computed since no token span includes them.
            Chunk::Arith { .. } => {
                if let Some(r) = next_span(raw_spans, cursor) {
                    let full_start = r.start.saturating_sub(3);
                    let full_end = r.end + 2;
                    out.push(HighlightSpan { start: offset + full_start, end: offset + full_end, kind: HighlightKind::Substitution, link: None });
                }
            }

            // `<(cmd)` and `>(cmd)`: like `Chunk::Sub`, with one possible
            // delimiter pair each.
            Chunk::ProcSubIn { raw } => push_procsub(raw, offset, raw_spans, cursor, ctx, out),
            Chunk::ProcSubOut { raw } => push_procsub(raw, offset, raw_spans, cursor, ctx, out),
        }
    }
}

fn push_procsub(raw: &str, offset: usize, raw_spans: &[Range<usize>], cursor: &mut usize, ctx: HighlightContext, out: &mut Vec<HighlightSpan>) {
    if let Some(r) = next_span(raw_spans, cursor) {
        let delim_start = r.start.saturating_sub(2);
        out.push(HighlightSpan { start: offset + delim_start, end: offset + r.start, kind: HighlightKind::Substitution, link: None });
        out.push(HighlightSpan { start: offset + r.end, end: offset + r.end + 1, kind: HighlightKind::Substitution, link: None });
        highlight_into(raw, offset + r.start, ctx, out);
    }
}

#[cfg(test)]
mod tests {
    // A filename is text somebody else wrote, and `render_linked` stands
    // between it and the terminal's escape stream.
    #[test]
    fn a_control_character_never_reaches_the_terminal_as_one() {
        let render = |text: &str| {
            let chars: Vec<char> = text.chars().collect();
            (chars.len(), super::render_styled(&super::compose(&chars, &[])))
        };
        let (len, out) = render("evil\x1b[2J.txt");
        assert!(!out.contains("\x1b[2J"), "the screen-clearing sequence must not survive: {out:?}");
        assert!(out.contains("evil\u{FFFD}[2J.txt"), "{out:?}");

        // The only escapes left are this function's own SGR framing.
        let (_, benign) = render("evilX[2J.txt");
        assert_eq!(out.matches('\x1b').count(), benign.matches('\x1b').count(), "the text contributed an escape of its own");

        // One character in, one out, so match positions, selections and the
        // caret column still line up.
        assert_eq!(super::compose(&"evil\x1b[2J.txt".chars().collect::<Vec<_>>(), &[]).len(), len);
    }

    use super::*;

    // The text each span covers, so a failure reads as what's on screen.
    fn json_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = JsonHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    fn ini_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = IniHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    fn kinds(text: &str) -> Vec<(usize, usize, HighlightKind)> {
        let mut spans = BashHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (s.start, s.end, s.kind)).collect()
    }

    #[test]
    fn plain_word_command_has_no_spans() {
        assert_eq!(kinds("ls -la"), vec![]);
    }

    #[test]
    fn keywords_and_operators() {
        let text = "if true; then echo hi; fi";
        let spans = kinds(text);
        assert!(spans.contains(&(0, 2, HighlightKind::Keyword))); // if
        assert!(spans.contains(&(9, 13, HighlightKind::Keyword))); // then
        assert!(spans.contains(&(23, 25, HighlightKind::Keyword))); // fi
        assert!(spans.iter().any(|s| s.2 == HighlightKind::Operator));
    }

    // A here-string or heredoc on a numbered descriptor is one redirect, and
    // the script continues after it: `3<<<x` isn't `3<` plus a heredoc.
    #[test]
    fn a_numbered_here_string_or_heredoc_ends_where_it_ends() {
        let text = "cat 3<<<x\nif true; then :; fi";
        let spans = kinds(text);
        let redirect_at = |i: usize| spans.iter().any(|s| s.2 == HighlightKind::Redirect && s.0 <= i && i < s.1);
        assert!((4..8).all(redirect_at), "all of `3<<<` is the redirect: {spans:?}");
        assert!(spans.contains(&(10, 12, HighlightKind::Keyword)), "the next line is a command again: {spans:?}");

        let text = "cat 4<<EOF\nbody\nEOF\nif true; then :; fi";
        let spans = kinds(text);
        assert!(spans.contains(&(20, 22, HighlightKind::Keyword)), "after the document ends, the script resumes: {spans:?}");
    }

    #[test]
    fn redirect_gets_its_own_span() {
        let text = "ls > out.txt";
        let spans = kinds(text);
        assert!(spans.contains(&(3, 4, HighlightKind::Redirect)));
    }

    #[test]
    fn single_quoted_string_excludes_the_quote_marks() {
        let text = "echo 'hello world'";
        let spans = kinds(text);
        assert!(spans.contains(&(6, 17, HighlightKind::String)));
        assert_eq!(&text[6..17], "hello world");
    }

    #[test]
    fn bare_variable_gets_the_name_only() {
        let text = "echo $HOME";
        let spans = kinds(text);
        assert!(spans.contains(&(6, 10, HighlightKind::Variable)));
        assert_eq!(&text[6..10], "HOME");
    }

    #[test]
    fn braced_variable_gets_the_inner_content_only() {
        let text = "echo ${HOME}";
        let spans = kinds(text);
        assert!(spans.contains(&(7, 11, HighlightKind::Variable)));
        assert_eq!(&text[7..11], "HOME");
    }

    #[test]
    fn bare_arithmetic_command_is_one_flat_span() {
        let text = "((1 + 2))";
        let spans = kinds(text);
        assert_eq!(spans, vec![(0, 9, HighlightKind::Substitution)]);
    }

    #[test]
    fn arithmetic_expansion_within_a_word_is_one_flat_span() {
        let text = "echo $((1 + 2))";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 15, HighlightKind::Substitution)));
        assert_eq!(&text[5..15], "$((1 + 2))");
        // Nothing inside the parens gets its own span.
        assert_eq!(spans.iter().filter(|s| s.0 >= 5 && s.1 <= 15).count(), 1);
    }

    #[test]
    fn subshell_parens_are_uncolored_but_interior_recurses() {
        let text = "(echo 'hi')";
        let spans = kinds(text);
        // Neither paren gets a span; the nested string is offset back into
        // the outer coordinates.
        assert_eq!(spans, vec![(7, 9, HighlightKind::String)]);
        assert_eq!(&text[7..9], "hi");
    }

    #[test]
    fn dollar_paren_substitution_delimiters_and_interior() {
        let text = "echo $(echo hi)";
        let spans = kinds(text);
        // `$(` at 5..7, `)` at 14..15.
        assert!(spans.contains(&(5, 7, HighlightKind::Substitution)));
        assert!(spans.contains(&(14, 15, HighlightKind::Substitution)));
    }

    #[test]
    fn backtick_substitution_delimiters_are_one_char_each() {
        let text = "echo `echo hi`";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 6, HighlightKind::Substitution)));
        assert!(spans.contains(&(13, 14, HighlightKind::Substitution)));
    }

    // A single-quoted string inside `$(...)` gets its own String span.
    #[test]
    fn nested_single_quote_inside_substitution_recurses() {
        let text = "echo \"yooo, $(printf 'hello %' world)\"";
        let spans = kinds(text);
        let quote_start = text.find("'hello %'").unwrap() + 1;
        let quote_end = quote_start + "hello %".len();
        assert!(
            spans.contains(&(quote_start, quote_end, HighlightKind::String)),
            "expected a String span for the nested quote at {}..{}, got {:?}",
            quote_start,
            quote_end,
            spans
        );
        let outer_start = text.find("yooo, ").unwrap();
        assert!(spans.contains(&(outer_start, outer_start + "yooo, ".len(), HighlightKind::String)));
    }

    #[test]
    fn comment_gets_its_own_span() {
        let text = "echo hi # a comment";
        let spans = kinds(text);
        let start = text.find('#').unwrap();
        assert!(spans.contains(&(start, text.len(), HighlightKind::Comment)));
    }

    #[test]
    fn process_substitution_delimiters_and_interior() {
        let text = "diff <(sort a) <(sort b)";
        let spans = kinds(text);
        assert!(spans.contains(&(5, 7, HighlightKind::Substitution))); // "<("
        assert!(spans.contains(&(13, 14, HighlightKind::Substitution))); // ")"
    }

    #[test]
    fn incomplete_line_highlights_up_to_the_error_point() {
        // The lexer stops with an error at the unclosed quote, and `echo`
        // before it is still highlighted, as a plain word.
        let text = "echo 'unterminated";
        let spans = BashHighlighter.highlight(text, HighlightContext::default());
        // No panic, and no spans: `echo` is plain and the quote never
        // produced a literal chunk.
        assert_eq!(spans, vec![]);
    }

    #[test]
    fn compose_paints_default_uncolored_cells_for_an_empty_layer_set() {
        let chars: Vec<char> = "abc".chars().collect();
        let cells = compose(&chars, &[]);
        assert_eq!(cells.len(), 3);
        for (cell, expected) in cells.iter().zip("abc".chars()) {
            assert_eq!(cell.ch, expected);
            assert_eq!(cell.fg, vt100::Color::Default);
        }
    }

    #[test]
    fn compose_later_layer_overrides_earlier_layer_for_overlapping_chars() {
        let chars: Vec<char> = "abc".chars().collect();
        let base = [StyledSpan { start: 0, end: 3, fg: vt100::Color::Indexed(2), attrs: vt100::CellAttrs::default() }];
        let overlay = [StyledSpan { start: 1, end: 2, fg: vt100::Color::Indexed(5), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&base, &overlay]);
        assert_eq!(cells[0].fg, vt100::Color::Indexed(2));
        assert_eq!(cells[1].fg, vt100::Color::Indexed(5)); // overlay wins here
        assert_eq!(cells[2].fg, vt100::Color::Indexed(2));
    }

    #[test]
    fn compose_span_end_past_char_count_is_clamped_not_a_panic() {
        let chars: Vec<char> = "ab".chars().collect();
        let layer = [StyledSpan { start: 0, end: 100, fg: vt100::Color::Indexed(1), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|c| c.fg == vt100::Color::Indexed(1)));
    }

    #[test]
    fn render_styled_coalesces_runs_of_identical_style_into_one_sgr_code() {
        let chars: Vec<char> = "abcd".chars().collect();
        // Two runs, so two SGR escapes, not one per char.
        let layer = [StyledSpan { start: 2, end: 4, fg: vt100::Color::Indexed(3), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        let rendered = render_styled(&cells);
        assert_eq!(rendered.matches('\x1b').count(), 3); // 2 style changes + 1 trailing reset
        assert_eq!(
            rendered,
            format!(
                "{}ab{}cd\x1b[0m",
                vt100::sgr_codes(vt100::Color::Default, vt100::Color::Default, vt100::CellAttrs::default()),
                vt100::sgr_codes(vt100::Color::Indexed(3), vt100::Color::Default, vt100::CellAttrs::default())
            )
        );
    }

    #[test]
    fn render_styled_uniform_style_emits_a_single_run() {
        let chars: Vec<char> = "abc".chars().collect();
        let layer = [StyledSpan { start: 0, end: 3, fg: vt100::Color::Indexed(2), attrs: vt100::CellAttrs::default() }];
        let cells = compose(&chars, &[&layer]);
        let rendered = render_styled(&cells);
        // One SGR to enter the style, one to reset.
        assert_eq!(rendered.matches('\x1b').count(), 2);
    }

    // The scanners in codehighlight.rs count bytes and everything else counts
    // characters; the two agree only on ASCII.
    #[test]
    fn every_highlighter_reports_the_unit_compose_indexes_by() {
        // Each assertion is about the span after the non-ASCII character,
        // which an offset in the wrong unit moves.
        let cases: &[(&str, &str, HighlightKind, &str)] = &[
            ("rust", "let \u{e4} = 1;", HighlightKind::Number, "1"),
            ("go", "var \u{e4} = 1", HighlightKind::Number, "1"),
            ("python", "\u{e4} = 1", HighlightKind::Number, "1"),
            ("javascript", "let \u{e4} = 1", HighlightKind::Number, "1"),
            ("css", "a::after { content: \"\u{e4}\"; color: red }", HighlightKind::Key, "color"),
            ("yaml", "a: \u{e4}\nb: 1\n", HighlightKind::Number, "1"),
            ("html", "<p>\u{e4}</p>\n<b>x</b>\n", HighlightKind::Keyword, "<b"),
            ("bash", "echo \u{e4} # x", HighlightKind::Comment, "# x"),
            ("json", "{\"\u{e4}\": 1}", HighlightKind::Number, "1"),
            ("toml", "[\u{e4}]\nk = 1\n", HighlightKind::Number, "1"),
            ("markdown", "\u{e4}\n\n`code`\n", HighlightKind::String, "`code`"),
        ];
        for (language, text, kind, expected) in cases {
            let chars: Vec<char> = text.chars().collect();
            let spans = highlighter_for_language(language).expect(language).highlight(text, HighlightContext::default());
            let found: Vec<String> = spans.iter().filter(|s| s.kind == *kind).map(|s| chars[s.start..s.end].iter().collect::<String>()).collect();
            assert!(found.iter().any(|f| f == expected), "{language}: wanted {expected:?} among {kind:?} spans, got {found:?}");
        }
    }

    // An offset inside a character maps to that character instead of
    // panicking.
    #[test]
    fn an_offset_inside_a_character_does_not_panic() {
        let text = "a\u{e4}b";
        let mut spans = vec![span(0..2, HighlightKind::Keyword), span(2..4, HighlightKind::String)];
        char_offsets(text, &mut spans);
        assert_eq!((spans[0].start, spans[0].end), (0, 1));
        assert_eq!((spans[1].start, spans[1].end), (1, 3));
    }

    // `render_line` emits SGR codes, so a submitted line echoed into a
    // pane's grid stays highlighted.
    #[test]
    fn render_line_carries_syntax_highlighting_as_sgr_codes() {
        let rendered = render_line("if true; then echo hi; fi", HighlightContext::default());
        assert!(rendered.contains('\x1b'), "{rendered:?}");
        assert!(rendered.ends_with("\x1b[0m"), "{rendered:?}");
        // With the SGR sequences stripped the text is unchanged.
        let mut stripped = String::new();
        let mut in_escape = false;
        for ch in rendered.chars() {
            if ch == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if ch == 'm' {
                    in_escape = false;
                }
            } else {
                stripped.push(ch);
            }
        }
        assert_eq!(stripped, "if true; then echo hi; fi");
    }

    #[test]
    fn default_style_covers_every_highlight_kind_without_panicking() {
        for kind in [
            HighlightKind::Keyword,
            HighlightKind::Operator,
            HighlightKind::Redirect,
            HighlightKind::String,
            HighlightKind::Variable,
            HighlightKind::Substitution,
            HighlightKind::Comment,
            HighlightKind::Number,
            HighlightKind::Flag,
            HighlightKind::Subcommand,
            HighlightKind::Link,
            HighlightKind::FormatSpecifier,
            HighlightKind::InvalidCommand,
        ] {
            let _ = default_style(kind);
        }
    }

    #[test]
    fn resolve_style_falls_back_to_default_style_with_no_overrides() {
        assert_eq!(resolve_style(HighlightKind::Keyword, None), default_style(HighlightKind::Keyword));
        let empty = ColorOverrides::new();
        assert_eq!(resolve_style(HighlightKind::Keyword, Some(&empty)), default_style(HighlightKind::Keyword));
    }

    #[test]
    fn resolve_style_substitutes_only_the_foreground_never_the_attrs() {
        let mut overrides = ColorOverrides::new();
        overrides.insert(HighlightKind::Keyword, vt100::Color::Rgb(1, 2, 3));
        let (fg, attrs) = resolve_style(HighlightKind::Keyword, Some(&overrides));
        assert_eq!(fg, vt100::Color::Rgb(1, 2, 3));
        assert_eq!(attrs, default_style(HighlightKind::Keyword).1, "attrs must stay whatever default_style says regardless of a color override");
    }

    #[test]
    fn hl_names_covers_every_kind_a_colour_could_mean_anything_for() {
        // Markdown's text styles stay out: their weight or slant is their
        // meaning.
        for attribute_only in [HighlightKind::Emphasis, HighlightKind::Strong, HighlightKind::Struck] {
            assert!(!HL_NAMES.iter().any(|(k, _)| *k == attribute_only), "{attribute_only:?}");
        }
        // Flag, Subcommand and Link draw in the terminal's foreground until
        // a theme says otherwise. Being in the table and having a colour of
        // one's own are different questions: absence from a theme means
        // inherit.
        for inherits in [HighlightKind::Flag, HighlightKind::Subcommand, HighlightKind::Link] {
            assert!(HL_NAMES.iter().any(|(k, _)| *k == inherits), "{inherits:?}");
            assert_eq!(default_style(inherits).0, vt100::Color::Default, "{inherits:?} must still inherit with nothing set");
        }
        // Bump when a kind is added, and give it a colour in every shipped
        // theme; theme.rs's guard insists.
        assert_eq!(HL_NAMES.len(), 14);
    }

    fn tok(word: &str) -> Tok {
        // Lexes just enough to get a real Tok of the right variant.
        match lexer::Lexer::new(word).tokenize() {
            Ok(mut toks) if toks.len() == 1 => toks.pop().unwrap().0,
            other => panic!("expected exactly one token from {word:?}, got {other:?}"),
        }
    }

    #[test]
    fn resets_command_position_on_separators_and_command_list_keywords() {
        for src in ["|", "||", "&&", ";", ";;", "&", "{", "if", "then", "do", "while", "for"] {
            assert!(resets_command_position(&tok(src)), "expected {src:?} to reset command position");
        }
        assert!(resets_command_position(&Tok::Newline));
    }

    #[test]
    fn does_not_reset_command_position_on_closing_tokens() {
        for src in ["}", ")", "fi", "done"] {
            assert!(!resets_command_position(&tok(src)), "expected {src:?} to NOT reset command position");
        }
    }

    #[test]
    fn does_not_reset_command_position_on_an_ordinary_word() {
        assert!(!resets_command_position(&tok("echo")));
    }

    #[test]
    fn is_valid_ident_accepts_typical_shell_variable_names() {
        assert!(is_valid_ident("FOO"));
        assert!(is_valid_ident("_foo123"));
        assert!(!is_valid_ident(""));
        assert!(!is_valid_ident("1FOO")); // leading digit
        assert!(!is_valid_ident("FOO-BAR")); // hyphen not allowed
    }

    fn plain_chunks(s: &str) -> Vec<Chunk> {
        vec![Chunk::Str(s.to_string())]
    }

    #[test]
    fn is_assignment_prefix_word_recognizes_name_equals_and_name_plus_equals() {
        assert!(is_assignment_prefix_word(&plain_chunks("FOO=bar")));
        assert!(is_assignment_prefix_word(&plain_chunks("FOO+=bar")));
        assert!(is_assignment_prefix_word(&plain_chunks("FOO=")));
    }

    #[test]
    fn is_assignment_prefix_word_rejects_invalid_shapes() {
        assert!(!is_assignment_prefix_word(&plain_chunks("1FOO=bar"))); // invalid ident
        assert!(!is_assignment_prefix_word(&plain_chunks("echo"))); // no '='
        assert!(!is_assignment_prefix_word(&plain_chunks("=bar"))); // empty name
        // A word that isn't a single plain `Chunk::Str` is never an
        // assignment prefix.
        assert!(!is_assignment_prefix_word(&[Chunk::Var { name: "FOO".to_string(), quoted: false, braced: false }]));
    }

    #[test]
    fn command_name_and_argument_positions_produce_no_spurious_spans() {
        // Exercises the state machine: an assignment prefix, a command name
        // and two arguments, none producing a span. A first query for
        // `echo`'s man page is Pending anyway, and spans need an exact
        // match.
        assert_eq!(kinds("FOO=bar echo one two"), vec![]);
    }

    fn man(flags: &[&str], subcommands: &[&str]) -> manpages::ManPageData {
        manpages::ManPageData {
            flags: flags.iter().map(|s| s.to_string()).collect(),
            subcommands: subcommands.iter().map(|s| s.to_string()).collect(),
            name_section: None,
            flag_descriptions: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn classify_plain_argument_core_matches_an_exact_flag_at_any_arg_index() {
        let data = man(&["-a", "--all"], &[]);
        let span = 5..7;
        let got = classify_plain_argument_core("-a", &span, 3, Some(&data), None, 0);
        assert_eq!(got, Some(HighlightSpan { start: 5, end: 7, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn classify_plain_argument_core_strips_a_value_suffix_before_matching() {
        let data = man(&["--color"], &[]);
        let span = 0..13;
        let got = classify_plain_argument_core("--color=auto", &span, 1, Some(&data), None, 0);
        assert_eq!(got, Some(HighlightSpan { start: 0, end: 13, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn classify_plain_argument_core_flag_miss_never_falls_through() {
        let data = man(&["-a"], &["commit"]);
        // `-x` matches no flag and, starting with `-`, is never a subcommand.
        let got = classify_plain_argument_core("-x", &(0..2), 0, Some(&data), None, 0);
        assert_eq!(got, None);
    }

    #[test]
    fn classify_plain_argument_core_no_man_data_yields_no_match() {
        let got = classify_plain_argument_core("-a", &(0..2), 0, None, None, 0);
        assert_eq!(got, None);
    }

    #[test]
    fn classify_plain_argument_core_matches_subcommand_only_at_arg_index_zero() {
        let data = man(&[], &["commit", "push"]);
        let span = 4..10;
        assert_eq!(
            classify_plain_argument_core("commit", &span, 0, Some(&data), None, 0),
            Some(HighlightSpan { start: 4, end: 10, kind: HighlightKind::Subcommand, link: None })
        );
        // Subcommands only at `arg_index == 0`.
        assert_eq!(classify_plain_argument_core("commit", &span, 1, Some(&data), None, 0), None);
    }

    #[test]
    fn classify_plain_argument_core_offset_shifts_the_returned_span() {
        let data = man(&["-a"], &[]);
        let got = classify_plain_argument_core("-a", &(2..4), 0, Some(&data), None, 100);
        assert_eq!(got, Some(HighlightSpan { start: 102, end: 104, kind: HighlightKind::Flag, link: None }));
    }

    #[test]
    fn strip_flag_suffix_stops_at_equals_or_space() {
        assert_eq!(strip_flag_suffix("--color=auto"), "--color");
        assert_eq!(strip_flag_suffix("-C <path>"), "-C");
        assert_eq!(strip_flag_suffix("-a"), "-a");
    }

    #[test]
    fn resolve_file_link_finds_a_real_relative_path() {
        let cwd = std::env::current_dir().unwrap();
        let url = resolve_file_link("Cargo.toml", Some(&cwd));
        assert!(url.as_deref().is_some_and(|u| u.starts_with("file://")), "{url:?}");
    }

    #[test]
    fn resolve_file_link_returns_none_for_a_nonexistent_path() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve_file_link("definitely-does-not-exist-xyz", Some(&cwd)), None);
    }

    #[test]
    fn resolve_file_link_returns_none_without_a_cwd() {
        // The colon line passes `None`: no guessing with the process's cwd.
        assert_eq!(resolve_file_link("Cargo.toml", None), None);
    }

    #[test]
    fn resolve_file_link_handles_an_absolute_path_directly() {
        let cwd = std::env::current_dir().unwrap();
        let abs = cwd.join("Cargo.toml");
        let url = resolve_file_link(abs.to_str().unwrap(), Some(&cwd));
        assert!(url.as_deref().is_some_and(|u| u.starts_with("file://")), "{url:?}");
    }

    #[test]
    fn classify_plain_argument_core_falls_back_to_link_when_no_flag_or_subcommand_matches() {
        let cwd = std::env::current_dir().unwrap();
        let data = man(&[], &[]);
        let span = 0..10;
        match classify_plain_argument_core("Cargo.toml", &span, 1, Some(&data), Some(&cwd), 0) {
            Some(HighlightSpan { start: 0, end: 10, kind: HighlightKind::Link, link: Some(url) }) => {
                assert!(url.starts_with("file://"), "{url}");
            }
            other => panic!("expected a Link span, got {other:?}"),
        }
    }

    #[test]
    fn classify_plain_argument_core_no_link_when_cwd_is_none() {
        let data = man(&[], &[]);
        assert_eq!(classify_plain_argument_core("Cargo.toml", &(0..10), 1, Some(&data), None, 0), None);
    }

    fn chars_of(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn scan_printf_directives_recognizes_every_supported_conversion() {
        for conv in ["s", "b", "c", "q", "d", "i", "u", "o", "x", "X"] {
            let text = format!("%{conv}");
            let chars = chars_of(&text);
            assert_eq!(scan_printf_directives(&chars), vec![(0..2, HighlightKind::FormatSpecifier)], "conv={conv}");
        }
    }

    #[test]
    fn scan_printf_directives_handles_flags_width_and_precision() {
        assert_eq!(scan_printf_directives(&chars_of("%-10s")), vec![(0..5, HighlightKind::FormatSpecifier)]);
        assert_eq!(scan_printf_directives(&chars_of("%05d")), vec![(0..4, HighlightKind::FormatSpecifier)]);
        assert_eq!(scan_printf_directives(&chars_of("%5.2f")), vec![]); // 'f' isn't supported by this shell's printf
        assert_eq!(scan_printf_directives(&chars_of("%.3s")), vec![(0..4, HighlightKind::FormatSpecifier)]);
    }

    #[test]
    fn scan_printf_directives_treats_literal_percent_as_its_own_directive() {
        assert_eq!(scan_printf_directives(&chars_of("100%% done")), vec![(3..5, HighlightKind::FormatSpecifier)]);
    }

    #[test]
    fn scan_printf_directives_skips_unrecognized_conversions_without_a_span() {
        assert_eq!(scan_printf_directives(&chars_of("%z")), vec![]);
    }

    #[test]
    fn scan_printf_directives_does_not_panic_on_a_trailing_percent() {
        assert_eq!(scan_printf_directives(&chars_of("done%")), vec![]);
    }

    #[test]
    fn scan_printf_directives_finds_multiple_directives_in_order() {
        assert_eq!(
            scan_printf_directives(&chars_of("%s is %d years old")),
            vec![(0..2, HighlightKind::FormatSpecifier), (6..8, HighlightKind::FormatSpecifier)]
        );
    }

    #[test]
    fn builtin_refine_only_fires_for_printf_at_arg_index_zero() {
        let text = chars_of("%s");
        assert_eq!(builtin_refine("printf", 0, &text), vec![(0..2, HighlightKind::FormatSpecifier)]);
        // `-v NAME FORMAT` moves the format to `arg_index` 2, which isn't
        // detected: no highlighting rather than wrong highlighting.
        assert_eq!(builtin_refine("printf", 2, &text), vec![]);
        assert_eq!(builtin_refine("echo", 0, &text), vec![]);
    }

    #[test]
    fn printf_format_specifier_is_highlighted_within_the_quoted_string() {
        let text = "printf 'hello %s' world";
        let spans = kinds(text);
        let quote_start = text.find("'hello %s'").unwrap() + 1;
        let quote_end = quote_start + "hello %s".len();
        assert!(spans.contains(&(quote_start, quote_end, HighlightKind::String)), "{spans:?}");
        let pct_start = text.find("%s").unwrap();
        assert!(spans.contains(&(pct_start, pct_start + 2, HighlightKind::FormatSpecifier)), "{spans:?}");
    }

    #[test]
    fn is_valid_command_name_recognizes_known_builtins() {
        let ctx = HighlightContext::default();
        for name in [":", "cd", "echo", "printf", "[[", "export", "read", "set"] {
            assert!(is_valid_command_name(name, &ctx), "{name} should be a known builtin");
        }
    }

    #[test]
    fn is_valid_command_name_recognizes_a_real_path_executable() {
        let ctx = HighlightContext::default();
        // coreutils, assumed present on any Unix test runner.
        assert!(is_valid_command_name("true", &ctx));
        assert!(is_valid_command_name("ls", &ctx));
    }

    // Commands resolve against the session's PATH, which a `cd` hook or
    // `export PATH=...` can change after the process environment was fixed.
    #[test]
    fn a_bare_name_resolves_against_the_sessions_path_not_the_processs() {
        let dir = crate::tempdir::TempDir::new("highlight-path-tests");
        let exe = dir.path().join("bish-test-hookbin");
        std::fs::write(&exe, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.path().display().to_string();
        let ctx = HighlightContext { path: Some(&path), ..HighlightContext::default() };
        assert!(is_valid_command_name("bish-test-hookbin", &ctx));
        // And the environment isn't consulted once a session hands its PATH
        // over: `ls` is on every runner's PATH and nowhere on this one.
        assert!(!is_valid_command_name("ls", &ctx));
        // A context with no shell still falls back to the environment.
        assert!(is_valid_command_name("ls", &HighlightContext::default()));
    }

    #[test]
    fn is_valid_command_name_rejects_an_unknown_bare_name() {
        let ctx = HighlightContext::default();
        assert!(!is_valid_command_name("bish-definitely-not-a-real-command-xyz", &ctx));
    }

    #[test]
    fn is_valid_command_name_recognizes_a_known_function() {
        let mut functions = HashSet::new();
        functions.insert("my_func".to_string());
        let ctx = HighlightContext { cwd: None, known_functions: Some(&functions), ..HighlightContext::default() };
        assert!(is_valid_command_name("my_func", &ctx));
        assert!(!is_valid_command_name("other_func", &ctx));
    }

    // The colon line's `:preview` is real and on no PATH, while a name that
    // is nothing is still flagged.
    #[test]
    fn is_valid_command_name_recognizes_one_of_the_lines_own_commands() {
        let commands = ["preview", "diag"];
        let ctx = HighlightContext { line_commands: Some(&commands), ..HighlightContext::default() };
        assert!(is_valid_command_name("preview", &ctx));
        assert!(!is_valid_command_name("bish-definitely-not-a-real-command-xyz", &ctx));
        // ...and only on that line.
        assert!(!is_valid_command_name("preview", &HighlightContext::default()));
    }

    #[test]
    fn is_valid_command_name_recognizes_a_direct_executable_path() {
        let ctx = HighlightContext::default();
        // The test binary: exists, executable, and an absolute path, so the
        // `/` branch is taken rather than the PATH walk.
        let exe = std::env::current_exe().unwrap();
        assert!(is_valid_command_name(exe.to_str().unwrap(), &ctx));
    }

    #[test]
    fn is_valid_command_name_rejects_a_non_executable_path() {
        let ctx = HighlightContext::default();
        let cwd = std::env::current_dir().unwrap();
        let toml = cwd.join("Cargo.toml");
        // A real file without the execute bit fails.
        assert!(!is_valid_command_name(toml.to_str().unwrap(), &ctx));
    }

    #[test]
    fn is_valid_command_name_rejects_a_nonexistent_direct_path() {
        let ctx = HighlightContext::default();
        assert!(!is_valid_command_name("/definitely/not/a/real/path/xyz", &ctx));
    }

    // A stale PATH entry ahead of a real one holding an executable and a
    // non-executable file. Handed in, not exported, so other tests can't see
    // it.
    #[test]
    fn enumerate_path_matches_filters_by_prefix_and_executable_bit() {
        let dir = std::env::temp_dir().join(format!("bish-completion-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe_path = dir.join("bish-test-widget");
        std::fs::write(&exe_path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("bish-test-plain"), b"not executable").unwrap();

        let stale_dir = dir.join("does-not-exist-as-a-dir");
        let path = format!("{}:{}", stale_dir.display(), dir.display());

        let matches = enumerate_path_matches("bish-test-w", Some(&path));

        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(matches, vec!["bish-test-widget".to_string()]);
    }

    #[test]
    fn invalid_command_name_is_highlighted_red() {
        let bogus = "bish-definitely-not-a-real-command-xyz";
        let text = format!("{bogus} arg");
        let spans = kinds(&text);
        assert!(spans.contains(&(0, bogus.len(), HighlightKind::InvalidCommand)), "{spans:?}");
    }

    #[test]
    fn valid_command_name_gets_no_new_styling() {
        // A valid command name gets no span.
        assert_eq!(kinds("true"), vec![]);
    }

    #[test]
    fn a_call_to_a_known_function_is_not_flagged_invalid() {
        let mut functions = HashSet::new();
        functions.insert("my_func".to_string());
        let ctx = HighlightContext { cwd: None, known_functions: Some(&functions), ..HighlightContext::default() };
        let spans = BashHighlighter.highlight("my_func arg", ctx);
        assert!(!spans.iter().any(|s| s.kind == HighlightKind::InvalidCommand), "{spans:?}");
    }

    #[test]
    fn command_validity_is_checked_after_every_reset_point() {
        // Every command-name position is checked, not just the first word.
        let text = "true | bish-definitely-not-a-real-command-xyz";
        let spans = kinds(text);
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand && s.0 == 0), "{spans:?}");
        let second_start = text.find("bish-").unwrap();
        assert!(spans.contains(&(second_start, text.len(), HighlightKind::InvalidCommand)), "{spans:?}");
    }

    // A function definition's own name isn't flagged: on the defining line
    // it isn't registered yet, and `myfunc()` looks like an invocation to the
    // lexer.
    #[test]
    fn function_definition_name_is_never_flagged_invalid() {
        let text = "myfunc() { echo hi; }";
        let spans = kinds(text);
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand), "{spans:?}");
        // Nothing for `myfunc`, only the braces and `;` as operators.
        assert_eq!(
            spans,
            vec![
                (9, 10, HighlightKind::Operator),  // '{'
                (18, 19, HighlightKind::Operator), // ';'
                (20, 21, HighlightKind::Operator), // '}'
            ]
        );
    }

    // `for i in 1 2 3`: neither the loop variable nor a list word is a
    // command name.
    #[test]
    fn for_loop_variable_and_list_words_are_never_flagged_invalid() {
        let text = "for i in 1 2 3; do echo $i; done";
        let spans = kinds(text);
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand), "{spans:?}");
    }

    #[test]
    fn select_loop_variable_and_list_words_are_never_flagged_invalid() {
        let text = "select i in 1 2 3; do echo $i; done";
        let spans = kinds(text);
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand), "{spans:?}");
    }

    #[test]
    fn case_subject_word_is_never_flagged_invalid() {
        let text = "case bogus-not-a-command in *) echo yes ;; esac";
        let spans = kinds(text);
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand), "{spans:?}");
    }

    #[test]
    fn function_definition_name_with_a_space_before_parens_is_also_recognized() {
        // `name () { ... }` is a definition too. The tokenizer has no
        // whitespace tokens, so this holds by construction.
        let spans = kinds("myfunc () { :; }");
        assert!(!spans.iter().any(|s| s.2 == HighlightKind::InvalidCommand), "{spans:?}");
    }

    #[test]
    fn json_highlights_keys_apart_from_string_values() {
        assert_eq!(
            json_kinds(r#"{"name": "bish"}"#),
            vec![
                ("{".to_string(), HighlightKind::Operator),
                ("\"name\"".to_string(), HighlightKind::Key),
                (":".to_string(), HighlightKind::Operator),
                ("\"bish\"".to_string(), HighlightKind::String),
                ("}".to_string(), HighlightKind::Operator),
            ]
        );
    }

    // A string is a key only when a colon follows it, so an array of strings
    // stays strings.
    #[test]
    fn json_strings_in_an_array_are_not_keys() {
        let kinds: Vec<HighlightKind> = json_kinds(r#"["a", "b"]"#).into_iter().map(|(_, k)| k).collect();
        assert_eq!(
            kinds,
            vec![HighlightKind::Operator, HighlightKind::String, HighlightKind::Operator, HighlightKind::String, HighlightKind::Operator]
        );
    }

    #[test]
    fn json_highlights_numbers_and_the_three_literals() {
        assert_eq!(
            json_kinds("[1, -2.5e3, true, false, null]").into_iter().filter(|(_, k)| *k != HighlightKind::Operator).collect::<Vec<_>>(),
            vec![
                ("1".to_string(), HighlightKind::Number),
                ("-2.5e3".to_string(), HighlightKind::Number),
                ("true".to_string(), HighlightKind::Keyword),
                ("false".to_string(), HighlightKind::Keyword),
                ("null".to_string(), HighlightKind::Keyword),
            ]
        );
    }

    // Escape spans come after the string's span, so `compose` paints them
    // over it.
    #[test]
    fn json_marks_string_escapes_inside_the_string_that_holds_them() {
        let spans = JsonHighlighter.highlight(r#""a\nb""#, HighlightContext::default());
        assert_eq!(spans[0].kind, HighlightKind::String);
        assert_eq!((spans[0].start, spans[0].end), (0, 6));
        assert_eq!(spans[1].kind, HighlightKind::FormatSpecifier);
        assert_eq!((spans[1].start, spans[1].end), (2, 4));
    }

    // Half-typed JSON: the mistake goes uncoloured, the rest doesn't.
    #[test]
    fn json_keeps_highlighting_past_invalid_input() {
        assert_eq!(
            json_kinds(r#"{"a": tru, "b": 2}"#),
            vec![
                ("{".to_string(), HighlightKind::Operator),
                ("\"a\"".to_string(), HighlightKind::Key),
                (":".to_string(), HighlightKind::Operator),
                (",".to_string(), HighlightKind::Operator),
                ("\"b\"".to_string(), HighlightKind::Key),
                (":".to_string(), HighlightKind::Operator),
                ("2".to_string(), HighlightKind::Number),
                ("}".to_string(), HighlightKind::Operator),
            ]
        );
    }

    #[test]
    fn json_highlights_nothing_in_empty_input() {
        assert!(json_kinds("").is_empty());
    }

    fn md_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = MarkdownHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    #[test]
    fn markdown_highlights_a_heading_whole() {
        assert_eq!(md_kinds("## Two\n"), vec![("## Two".to_string(), HighlightKind::Keyword)]);
    }

    // The spans include the delimiters, so `**` is bold too.
    #[test]
    fn markdown_emphasis_spans_cover_their_delimiters() {
        assert_eq!(
            md_kinds("*a* **b** ~~c~~ `d`\n"),
            vec![
                ("*a*".to_string(), HighlightKind::Emphasis),
                ("**b**".to_string(), HighlightKind::Strong),
                ("~~c~~".to_string(), HighlightKind::Struck),
                ("`d`".to_string(), HighlightKind::String),
            ]
        );
    }

    // A scan would colour these, wrongly.
    #[test]
    fn markdown_does_not_highlight_what_is_not_markup() {
        assert!(md_kinds("snake_case_name and 2 * 3 * 4\n").is_empty());
    }

    #[test]
    fn markdown_links_carry_their_destination() {
        let spans = MarkdownHighlighter.highlight("[text](/dest)\n", HighlightContext::default());
        let link = spans.iter().find(|s| s.kind == HighlightKind::Link).expect("a link");
        assert_eq!(link.link.as_deref(), Some("/dest"));
    }

    #[test]
    fn a_fenced_code_block_is_highlighted_in_its_own_language() {
        let src = "text\n\n```bash\nif true; then echo hi; fi\n```\n";
        let kinds = md_kinds(src);
        assert!(
            kinds.iter().any(|(text, kind)| text == "if" && *kind == HighlightKind::Keyword),
            "the bash keyword inside the fence is highlighted as bash: {kinds:?}"
        );
        assert!(kinds.iter().any(|(text, kind)| text == "bash" && *kind == HighlightKind::Variable));
    }

    #[test]
    fn markdown_treats_html_as_something_to_ignore() {
        assert_eq!(
            md_kinds("a <span>b</span>\n"),
            vec![("<span>".to_string(), HighlightKind::Comment), ("</span>".to_string(), HighlightKind::Comment)]
        );
    }

    fn roff_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = RoffHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    #[test]
    fn roff_highlights_a_control_line_by_its_parts() {
        assert_eq!(
            roff_kinds(".SH NAME\n"),
            vec![(".".to_string(), HighlightKind::Operator), ("SH".to_string(), HighlightKind::Keyword), ("NAME".to_string(), HighlightKind::String),]
        );
    }

    #[test]
    fn roff_highlights_escapes_inside_text() {
        assert_eq!(
            roff_kinds("plain \\fBbold\\fP text\n"),
            vec![("\\fB".to_string(), HighlightKind::Substitution), ("\\fP".to_string(), HighlightKind::Substitution),]
        );
    }

    #[test]
    fn roff_highlights_both_comment_forms() {
        assert_eq!(
            roff_kinds(".\\\" a whole line\ntext \\\" trailing\n"),
            vec![
                (".".to_string(), HighlightKind::Operator),
                ("\\\" a whole line".to_string(), HighlightKind::Comment),
                ("\\\" trailing".to_string(), HighlightKind::Comment),
            ]
        );
    }

    // The highlighter doesn't interpret: a page's macro definitions stay
    // visible as source.
    #[test]
    fn roff_highlighting_does_not_expand_macros() {
        let kinds = roff_kinds(".de Flag\n.B \\\\$1\n..\n.Flag x\n");
        assert!(kinds.iter().any(|(t, k)| t == "de" && *k == HighlightKind::Keyword));
        assert!(kinds.iter().any(|(t, k)| t == "Flag" && *k == HighlightKind::Keyword));
    }

    #[test]
    fn ini_highlights_a_section_its_key_and_its_value() {
        assert_eq!(
            ini_kinds("[core]\nbare = true\n"),
            vec![
                ("[core]".to_string(), HighlightKind::Keyword),
                ("bare".to_string(), HighlightKind::Key),
                ("=".to_string(), HighlightKind::Operator),
                ("true".to_string(), HighlightKind::Keyword),
            ]
        );
    }

    // Both spans are emitted, and `compose` paints the subsection's over the
    // header's.
    #[test]
    fn ini_paints_a_subsection_over_its_header() {
        assert_eq!(
            ini_kinds("[remote \"origin\"]"),
            vec![("[remote \"origin\"]".to_string(), HighlightKind::Keyword), ("\"origin\"".to_string(), HighlightKind::String),]
        );
    }

    #[test]
    fn ini_leaves_an_ordinary_value_uncoloured() {
        assert_eq!(
            ini_kinds("ExecStart=/usr/bin/env sh"),
            vec![("ExecStart".to_string(), HighlightKind::Key), ("=".to_string(), HighlightKind::Operator)]
        );
    }

    #[test]
    fn ini_colours_numbers_and_quoted_strings_but_not_paths() {
        let kinds = |text: &str| ini_kinds(text).into_iter().map(|(_, k)| k).last().unwrap();
        assert_eq!(kinds("port = 8080"), HighlightKind::Number);
        assert_eq!(kinds("mask = 0xFF"), HighlightKind::Number);
        assert_eq!(kinds("name = \"bish\""), HighlightKind::String);
        assert_eq!(kinds("enabled = no"), HighlightKind::Keyword);
        assert_eq!(kinds("path = /var/log"), HighlightKind::Operator, "the `=` is the last span; the value got none");
    }

    #[test]
    fn ini_marks_escapes_inside_a_quoted_value() {
        assert_eq!(
            ini_kinds("a = \"x\\ny\""),
            vec![
                ("a".to_string(), HighlightKind::Key),
                ("=".to_string(), HighlightKind::Operator),
                ("\"x\\ny\"".to_string(), HighlightKind::String),
                ("\\n".to_string(), HighlightKind::FormatSpecifier),
            ]
        );
    }

    // A `#` comments only at the start of a line: in a systemd command it's
    // part of the command (see ini.rs).
    #[test]
    fn ini_highlights_whole_line_comments_only() {
        assert_eq!(ini_kinds("; a note"), vec![("; a note".to_string(), HighlightKind::Comment)]);
        let trailing = ini_kinds("a = 1 ; not a comment");
        assert!(!trailing.iter().any(|(_, k)| *k == HighlightKind::Comment));
    }

    // A continuation's text is value, not a key.
    #[test]
    fn ini_does_not_read_a_continuation_line_as_a_key() {
        let spans = ini_kinds("install_requires =\n    foo >= 1.0\n");
        assert_eq!(spans.iter().filter(|(_, k)| *k == HighlightKind::Key).count(), 1, "only `install_requires`");
    }

    #[test]
    fn ini_highlights_nothing_in_empty_input() {
        assert_eq!(ini_kinds(""), vec![]);
    }

    fn jsonc_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = JsoncHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    #[test]
    fn jsonc_highlights_both_comment_forms() {
        assert_eq!(jsonc_kinds("// why\n1"), vec![("// why".to_string(), HighlightKind::Comment), ("1".to_string(), HighlightKind::Number)]);
        assert_eq!(jsonc_kinds("/* why */ 1"), vec![("/* why */".to_string(), HighlightKind::Comment), ("1".to_string(), HighlightKind::Number)]);
    }

    // The lookahead skips comments to find the colon.
    #[test]
    fn jsonc_still_sees_a_key_through_a_comment() {
        let kinds = jsonc_kinds("{\"a\" /* c */: 1}");
        assert!(kinds.iter().any(|(t, k)| t == "\"a\"" && *k == HighlightKind::Key));
    }

    // An unterminated comment runs to the end of the input.
    #[test]
    fn jsonc_tolerates_an_unterminated_block_comment() {
        assert_eq!(jsonc_kinds("/* open"), vec![("/* open".to_string(), HighlightKind::Comment)]);
    }

    // In strict JSON a `/` isn't a comment, just a character that can't
    // start a value.
    #[test]
    fn plain_json_has_no_comments() {
        assert!(!json_kinds("// not a comment").iter().any(|(_, k)| *k == HighlightKind::Comment));
    }

    fn toml_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = TomlHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    #[test]
    fn toml_highlights_a_table_a_key_and_its_value() {
        assert_eq!(
            toml_kinds("[package]\nname = \"bish\"\n"),
            vec![
                ("[package]".to_string(), HighlightKind::Keyword),
                ("name".to_string(), HighlightKind::Key),
                ("=".to_string(), HighlightKind::Operator),
                ("\"bish\"".to_string(), HighlightKind::String),
            ]
        );
    }

    #[test]
    fn toml_colours_the_same_word_by_position() {
        let kinds = |text: &str| toml_kinds(text).into_iter().map(|(_, k)| k).collect::<Vec<_>>();
        assert_eq!(kinds("true = false"), vec![HighlightKind::Key, HighlightKind::Operator, HighlightKind::Keyword]);
    }

    #[test]
    fn toml_colours_numbers_dates_and_booleans() {
        let last = |text: &str| toml_kinds(text).pop().unwrap().1;
        assert_eq!(last("k = 0xFF"), HighlightKind::Number);
        assert_eq!(last("k = 1979-05-27T07:32:00Z"), HighlightKind::Number);
        assert_eq!(last("k = true"), HighlightKind::Keyword);
    }

    #[test]
    fn toml_marks_escapes_inside_a_basic_string() {
        let spans = toml_kinds("k = \"a\\nb\"");
        assert!(spans.iter().any(|(t, k)| t == "\\n" && *k == HighlightKind::FormatSpecifier));
    }

    #[test]
    fn toml_highlights_nothing_in_empty_input() {
        assert_eq!(toml_kinds(""), vec![]);
    }

    fn dotenv_kinds(text: &str) -> Vec<(String, HighlightKind)> {
        let chars: Vec<char> = text.chars().collect();
        let mut spans = DotenvHighlighter.highlight(text, HighlightContext::default());
        spans.sort_by_key(|s| (s.start, s.end));
        spans.into_iter().map(|s| (chars[s.start..s.end].iter().collect(), s.kind)).collect()
    }

    #[test]
    fn dotenv_highlights_a_key_its_value_and_a_trailing_comment() {
        assert_eq!(
            dotenv_kinds("PORT=3000 # the port"),
            vec![
                ("PORT".to_string(), HighlightKind::Key),
                ("=".to_string(), HighlightKind::Operator),
                ("3000".to_string(), HighlightKind::Number),
                ("# the port".to_string(), HighlightKind::Comment),
            ]
        );
    }

    #[test]
    fn dotenv_highlights_export_as_a_keyword() {
        let kinds = dotenv_kinds("export A=1");
        assert_eq!(kinds[0], ("export".to_string(), HighlightKind::Keyword));
        assert_eq!(kinds[1], ("A".to_string(), HighlightKind::Key));
    }

    // An interpolation is the part of the value that isn't what it says.
    #[test]
    fn dotenv_marks_interpolation_inside_the_value_that_holds_it() {
        let kinds = dotenv_kinds("A=\"${HOME}/bin\"");
        assert!(kinds.iter().any(|(t, k)| t == "\"${HOME}/bin\"" && *k == HighlightKind::String));
        assert!(kinds.iter().any(|(t, k)| t == "${HOME}" && *k == HighlightKind::Variable));
    }

    #[test]
    fn dotenv_leaves_a_single_quoted_value_alone() {
        let kinds = dotenv_kinds("A='$HOME'");
        assert!(!kinds.iter().any(|(_, k)| *k == HighlightKind::Variable), "single quotes interpolate nothing");
    }

    #[test]
    fn dotenv_leaves_a_bare_value_uncoloured() {
        assert_eq!(dotenv_kinds("PATH_TO=/usr/bin"), vec![("PATH_TO".to_string(), HighlightKind::Key), ("=".to_string(), HighlightKind::Operator)]);
    }

    #[test]
    fn dotenv_highlights_nothing_in_empty_input() {
        assert_eq!(dotenv_kinds(""), vec![]);
    }

    fn cells_of(text: &str) -> Vec<vt100::Cell> {
        text.chars().map(|ch| vt100::Cell { ch, ..vt100::Cell::default() }).collect()
    }

    fn link(start: usize, end: usize, url: &str) -> LinkSpan {
        LinkSpan { start, end, url: url.to_string() }
    }

    #[test]
    fn render_linked_wraps_the_run_in_osc_8() {
        let out = render_linked(&cells_of("see here now"), &[link(4, 8, "https://x")]);
        assert!(out.contains("\x1b]8;;https://x\x1b\\here\x1b]8;;\x1b\\"), "got {out:?}");
    }

    #[test]
    fn render_linked_opens_and_closes_once_per_run() {
        let out = render_linked(&cells_of("aaaa"), &[link(0, 4, "https://x")]);
        assert_eq!(out.matches("\x1b]8;;https://x").count(), 1);
        assert_eq!(out.matches("\x1b]8;;\x1b\\").count(), 1);
    }

    #[test]
    fn render_linked_closes_a_link_that_runs_to_the_end() {
        let out = render_linked(&cells_of("abc"), &[link(0, 3, "https://x")]);
        assert!(out.contains("abc\x1b]8;;\x1b\\"), "got {out:?}");
    }

    #[test]
    fn render_linked_handles_two_adjacent_links() {
        let out = render_linked(&cells_of("abcd"), &[link(0, 2, "https://a"), link(2, 4, "https://b")]);
        // The first run's close and the second's open sit back to back,
        // with the default style's SGR reset between the open and the text,
        // so the pieces are checked separately.
        assert!(out.contains("\x1b]8;;https://a"), "got {out:?}");
        assert!(out.contains("ab\x1b]8;;\x1b\\\x1b]8;;https://b\x1b\\cd"), "got {out:?}");
    }

    // A URL holding a control character is dropped, not repaired: it would be
    // spliced into an escape sequence.
    #[test]
    fn render_linked_refuses_a_url_that_could_break_out_of_the_sequence() {
        for url in ["https://x\x1b]0;pwned\x07", "https://x\x07", "https://x\n", ""] {
            let out = render_linked(&cells_of("abc"), &[link(0, 3, url)]);
            assert!(!out.contains("\x1b]8;;"), "emitted a hyperlink for {url:?}");
            assert!(out.contains("abc"));
        }
    }

    // With no links the output is exactly `render_styled`'s.
    #[test]
    fn render_styled_is_render_linked_with_nothing_to_link() {
        let cells = cells_of("hello");
        assert_eq!(render_styled(&cells), render_linked(&cells, &[]));
        assert!(!render_styled(&cells).contains("\x1b]8"));
    }
}
