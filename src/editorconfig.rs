// EditorConfig: `.editorconfig` files, the glob dialect their sections
// are written in, and the cascade that turns a stack of them into one
// answer for one file.
//
// The *file* is INI and `ini.rs` already reads it. What is not INI is
// what a section header means: `[*.{js,ts}]` is a path pattern, not a
// name, and matching it is this module's real work.
//
// **A fourth glob dialect**, after the shell's, gitignore's and
// pathspec's -- and genuinely a fourth rather than a rename of one of
// them:
//
//   - `*` does not cross a `/`, and `**` does. So far, gitignore's.
//   - ...but `**` is not a whole path segment here. `a**b` is legal and
//     crosses directories, where gitignore reads `**` only between
//     slashes. That alone rules out reusing `gitignore::Pattern`.
//   - `{js,ts}` alternation and `{1..9}` numeric ranges exist, and are
//     in nothing else here.
//
// Bracket expressions are the one piece that *is* shared:
// `glob::match_class` decides those, so `[!a-z]` means the same thing in
// a `.editorconfig` as it does everywhere else in bish.
#![allow(dead_code)]

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentStyle {
    Tab,
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndentSize {
    Columns(usize),
    /// `indent_size = tab`: whatever `tab_width` says.
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
    Cr,
}

impl Eol {
    pub fn text(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::Crlf => "\r\n",
            Eol::Cr => "\r",
        }
    }
}

/// What a `.editorconfig` says about one file. Every field is optional
/// because "said nothing" and "said the default" are different: only
/// the first leaves the editor's own setting alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Properties {
    pub indent_style: Option<IndentStyle>,
    pub indent_size: Option<IndentSize>,
    pub tab_width: Option<usize>,
    pub end_of_line: Option<Eol>,
    /// Parsed and deliberately never applied -- see `for_file`.
    pub charset: Option<String>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    /// `None` for "said nothing"; `Some(None)` for an explicit `off`.
    pub max_line_length: Option<Option<usize>>,
}

impl Properties {
    // Later wins, which is what makes a deeper `.editorconfig` and a
    // later section both override what came before them.
    fn merge(&mut self, other: &Properties) {
        macro_rules! take {
            ($($field:ident),*) => { $( if other.$field.is_some() { self.$field = other.$field.clone(); } )* };
        }
        take!(indent_style, indent_size, tab_width, end_of_line, charset, trim_trailing_whitespace, insert_final_newline, max_line_length);
    }

    /// How wide one indent is, resolving `indent_size = tab` and the
    /// spec's own "`tab_width` defaults to `indent_size`" rule. `None`
    /// when nothing here decides it.
    pub fn width(&self) -> Option<usize> {
        match self.indent_size {
            Some(IndentSize::Columns(n)) => Some(n),
            Some(IndentSize::Tab) => self.tab_width,
            None => self.tab_width,
        }
    }
}

/// Everything the `.editorconfig` files above `path` say about it.
///
/// Walks up from the file's own directory, stopping *after* the first
/// one that declares `root = true`, and merges from the outermost
/// inwards so a deeper file wins. Within a file, every matching section
/// applies in order, so a later one wins too.
///
/// `charset` is read but never acted on anywhere: bish reads files as
/// UTF-8 (`std::fs::read_to_string`), and latin-1 or UTF-16 would need
/// real decoding, a BOM concept and an encoding to write back with.
/// Refusing it outright is the honest answer -- half-supporting an
/// encoding is how a file gets silently mangled on save.
pub fn for_file(path: &Path) -> Properties {
    let mut files = Vec::new();
    let mut dir = path.parent();
    while let Some(here) = dir {
        let candidate = here.join(".editorconfig");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            let root = is_root(&text);
            files.push((here.to_path_buf(), text));
            if root {
                break;
            }
        }
        dir = here.parent();
    }
    let mut out = Properties::default();
    // Outermost first, so the deepest file has the last word.
    for (dir, text) in files.into_iter().rev() {
        out.merge(&properties_for(&text, path, &dir));
    }
    out
}

/// One file's worth of properties for one path -- every section whose
/// pattern matches, merged in order.
pub fn properties_for(text: &str, path: &Path, config_dir: &Path) -> Properties {
    let relative = match path.strip_prefix(config_dir) {
        Ok(rest) => rest.to_string_lossy().replace('\\', "/"),
        // Not under this config's directory at all, so none of its
        // sections can be about it.
        Err(_) => return Properties::default(),
    };
    let mut out = Properties::default();
    for (pattern, properties) in sections(text) {
        if matches(&pattern, &relative) {
            out.merge(&properties);
        }
    }
    out
}

/// Whether a `.editorconfig`'s preamble declares it the top of the
/// cascade. Only the lines before the first section count, which is
/// what "preamble" means here.
pub fn is_root(text: &str) -> bool {
    for item in &crate::ini::parse(text).items {
        match item {
            crate::ini::Item::Section { .. } => return false,
            crate::ini::Item::Entry { key, value: Some(value), .. } if slice(text, key).eq_ignore_ascii_case("root") => {
                return slice(text, &value.span).trim_matches('"').eq_ignore_ascii_case("true");
            }
            _ => {}
        }
    }
    false
}

/// Each `[pattern]` section and the properties under it, in file order.
pub fn sections(text: &str) -> Vec<(String, Properties)> {
    let mut out: Vec<(String, Properties)> = Vec::new();
    for item in &crate::ini::parse(text).items {
        match item {
            crate::ini::Item::Section { span, .. } => {
                // The pattern is everything between the brackets, taken
                // raw: a `.editorconfig` header is a path glob, and
                // `ini.rs`'s own name/subsection split (built for
                // `[remote "origin"]`) would cut one containing a quote
                // in the wrong place.
                let raw = slice(text, span);
                let pattern = raw.trim_start_matches('[').trim_end_matches(']').to_string();
                out.push((pattern, Properties::default()));
            }
            crate::ini::Item::Entry { key, value: Some(value), .. } => {
                let Some((_, properties)) = out.last_mut() else { continue };
                // Names and values are both case-insensitive, and the
                // spec says to lowercase them.
                let key = slice(text, key).to_ascii_lowercase();
                let raw = slice(text, &value.span).trim_matches('"').to_string();
                set(properties, &key, &raw.to_ascii_lowercase(), &raw);
            }
            _ => {}
        }
    }
    out
}

// One property. Anything unrecognized is ignored in silence, which the
// spec asks for: a `.editorconfig` is shared between editors, and every
// editor sees properties meant for the others.
fn set(properties: &mut Properties, key: &str, value: &str, raw: &str) {
    match key {
        "indent_style" => {
            properties.indent_style = match value {
                "tab" => Some(IndentStyle::Tab),
                "space" => Some(IndentStyle::Space),
                _ => None,
            }
        }
        "indent_size" => {
            properties.indent_size = match value {
                "tab" => Some(IndentSize::Tab),
                _ => value.parse().ok().map(IndentSize::Columns),
            }
        }
        "tab_width" => properties.tab_width = value.parse().ok(),
        "end_of_line" => {
            properties.end_of_line = match value {
                "lf" => Some(Eol::Lf),
                "crlf" => Some(Eol::Crlf),
                "cr" => Some(Eol::Cr),
                _ => None,
            }
        }
        // Kept in its original case: an encoding name is the one value
        // here that isn't a keyword.
        "charset" => properties.charset = Some(raw.to_string()),
        "trim_trailing_whitespace" => properties.trim_trailing_whitespace = parse_bool(value),
        "insert_final_newline" => properties.insert_final_newline = parse_bool(value),
        "max_line_length" => {
            properties.max_line_length = match value {
                "off" => Some(None),
                _ => value.parse().ok().map(Some),
            }
        }
        _ => {}
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn slice(text: &str, span: &std::ops::Range<usize>) -> String {
    text.chars().skip(span.start).take(span.end - span.start).collect()
}

/// Whether an EditorConfig section pattern matches a path, where the
/// path is relative to the `.editorconfig`'s own directory and
/// `/`-separated.
pub fn matches(pattern: &str, path: &str) -> bool {
    // A pattern with no `/` at all is about a *name*, and matches at any
    // depth; one with a `/` anywhere is anchored to the config's own
    // directory. A leading `/` anchors and is otherwise not part of it.
    let anchored = pattern.trim_end_matches('/').contains('/');
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    expand_braces(pattern).into_iter().any(|alternative| {
        let pat = alternative.as_bytes();
        if anchored {
            return match_here(pat, path.as_bytes());
        }
        // Unanchored: try the whole path and every suffix that starts
        // after a `/`, which is what "at any depth" comes to.
        let bytes = path.as_bytes();
        match_here(pat, bytes) || bytes.iter().enumerate().any(|(i, b)| *b == b'/' && match_here(pat, &bytes[i + 1..]))
    })
}

fn match_here(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    // `\c` is a literal `c`, whatever `c` is -- the same escape every
    // other matcher here honours.
    if pat[0] == b'\\' && pat.len() > 1 {
        return !text.is_empty() && text[0] == pat[1] && match_here(&pat[2..], &text[1..]);
    }
    // `**` crosses directories; `*` does not. Checked in that order,
    // since one is a prefix of the other.
    if pat.starts_with(b"**") {
        return (0..=text.len()).any(|i| match_here(&pat[2..], &text[i..]));
    }
    match pat[0] {
        b'*' => (0..=text.len()).take_while(|i| text[..*i].iter().all(|b| *b != b'/')).any(|i| match_here(&pat[1..], &text[i..])),
        b'?' => !text.is_empty() && text[0] != b'/' && match_here(&pat[1..], &text[1..]),
        b'[' => match crate::glob::match_class(pat, text.first().copied()) {
            Some((true, rest)) => !text.is_empty() && text[0] != b'/' && match_here(rest, &text[1..]),
            Some((false, _)) => false,
            // Malformed, so the `[` is an ordinary character.
            None => !text.is_empty() && text[0] == b'[' && match_here(&pat[1..], &text[1..]),
        },
        c => !text.is_empty() && text[0] == c && match_here(&pat[1..], &text[1..]),
    }
}

/// `a{b,c}d` into `abd` and `acd`, and `{1..3}` into `1`, `2`, `3`.
/// Nested braces expand too, which falls out of doing the tail
/// recursively.
///
/// A group with no top-level comma and no range is *literal*: `{foo}`
/// matches the four characters `{foo}`, which is what the reference
/// implementations do and what a lone brace in a filename needs.
pub fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = find_group_start(bytes) else { return vec![pattern.to_string()] };
    let Some(close) = find_group_end(bytes, open) else { return vec![pattern.to_string()] };
    let head = &pattern[..open];
    let body = &pattern[open + 1..close];
    let tail = &pattern[close + 1..];
    // `nested` says whether the alternatives are themselves patterns
    // that may hold groups. A *literal* group is not: re-expanding
    // `{foo}` would find the same group again, forever.
    let (alternatives, nested) = match split_alternatives(body) {
        Some(parts) => (parts, true),
        None => match numeric_range(body) {
            Some(numbers) => (numbers, false),
            None => (vec![format!("{{{body}}}")], false),
        },
    };
    let tails = expand_braces(tail);
    let mut out = Vec::new();
    for alternative in alternatives {
        // An alternative is expanded too, not just the tail -- that is
        // the whole of what makes `{a,b{c,d}}` work.
        let expansions = if nested { expand_braces(&alternative) } else { vec![alternative] };
        for expanded in expansions {
            for expanded_tail in &tails {
                out.push(format!("{head}{expanded}{expanded_tail}"));
            }
        }
    }
    out
}

fn find_group_start(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'{' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn find_group_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// The top-level comma-separated parts, or `None` when there are none --
// nested braces and escapes don't count as separators.
fn split_alternatives(body: &str) -> Option<Vec<String>> {
    let bytes = body.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(body[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(body[start..].to_string());
    Some(parts)
}

// `{1..9}`, inclusive at both ends and happy to count downwards.
fn numeric_range(body: &str) -> Option<Vec<String>> {
    let (from, to) = body.split_once("..")?;
    let from: i64 = from.trim().parse().ok()?;
    let to: i64 = to.trim().parse().ok()?;
    // Bounded so a typo can't ask for millions of alternatives.
    if (to - from).abs() > 1_000 {
        return None;
    }
    let range: Vec<String> =
        if from <= to { (from..=to).map(|n| n.to_string()).collect() } else { (to..=from).rev().map(|n| n.to_string()).collect() };
    Some(range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_does_not_cross_a_slash_and_a_double_star_does() {
        assert!(matches("*.rs", "main.rs"));
        assert!(!matches("src/*.rs", "src/deep/main.rs"));
        assert!(matches("src/**.rs", "src/deep/main.rs"));
        assert!(matches("**/main.rs", "a/b/main.rs"));
    }

    // The difference from gitignore that makes this its own dialect:
    // `**` is not a whole path segment here.
    #[test]
    fn a_double_star_may_sit_inside_a_segment() {
        assert!(matches("lib/**/*.rs", "lib/a/b/c.rs"));
        assert!(matches("a**b", "a/x/y/b"));
    }

    // A pattern with no `/` is a name and matches at any depth; one with
    // a `/` is anchored to the config's own directory.
    #[test]
    fn a_slash_anchors_the_pattern() {
        assert!(matches("*.rs", "deep/nested/main.rs"));
        assert!(matches("src/main.rs", "src/main.rs"));
        assert!(!matches("src/main.rs", "crate/src/main.rs"));
        assert!(matches("/main.rs", "main.rs"), "a leading slash anchors and is not part of it");
    }

    #[test]
    fn brace_alternation_expands() {
        assert_eq!(expand_braces("*.{js,ts}"), vec!["*.js", "*.ts"]);
        assert!(matches("*.{js,ts,tsx}", "app.tsx"));
        assert!(!matches("*.{js,ts}", "app.rs"));
    }

    #[test]
    fn nested_braces_expand_too() {
        assert_eq!(expand_braces("{a,b{c,d}}"), vec!["a", "bc", "bd"]);
    }

    #[test]
    fn a_numeric_range_expands() {
        assert_eq!(expand_braces("file{1..3}.txt"), vec!["file1.txt", "file2.txt", "file3.txt"]);
        assert!(matches("page{1..9}.md", "page7.md"));
        assert!(!matches("page{1..9}.md", "page10.md"));
    }

    // What the reference implementations do, and what a filename with a
    // brace in it needs.
    #[test]
    fn a_group_with_no_comma_is_literal() {
        assert_eq!(expand_braces("{foo}"), vec!["{foo}"]);
        assert!(matches("{foo}.txt", "{foo}.txt"));
    }

    #[test]
    fn bracket_classes_come_from_the_shared_glob() {
        assert!(matches("[abc].txt", "b.txt"));
        assert!(matches("[!abc].txt", "z.txt"));
        assert!(!matches("[!abc].txt", "a.txt"));
    }

    #[test]
    fn sections_carry_their_own_properties() {
        let text = "\
root = true

[*]
indent_style = space
indent_size = 4
insert_final_newline = true

[*.md]
trim_trailing_whitespace = false
max_line_length = off

[Makefile]
indent_style = tab
";
        let sections = sections(text);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].0, "*");
        assert_eq!(sections[0].1.indent_style, Some(IndentStyle::Space));
        assert_eq!(sections[0].1.indent_size, Some(IndentSize::Columns(4)));
        assert_eq!(sections[0].1.insert_final_newline, Some(true));
        assert_eq!(sections[1].1.trim_trailing_whitespace, Some(false));
        assert_eq!(sections[1].1.max_line_length, Some(None), "an explicit `off`");
        assert_eq!(sections[2].1.indent_style, Some(IndentStyle::Tab));
    }

    #[test]
    fn root_is_only_read_from_the_preamble() {
        assert!(is_root("root = true\n[*]\nindent_size = 2\n"));
        assert!(!is_root("[*]\nroot = true\n"), "inside a section it is an ordinary property");
        assert!(!is_root("[*]\nindent_size = 2\n"));
    }

    // Later sections win, which is what makes `[*]` a base that `[*.md]`
    // refines.
    #[test]
    fn a_later_matching_section_wins() {
        let text = "[*]\nindent_size = 4\n[*.md]\nindent_size = 2\n";
        let dir = Path::new("/p");
        assert_eq!(properties_for(text, Path::new("/p/a.rs"), dir).indent_size, Some(IndentSize::Columns(4)));
        assert_eq!(properties_for(text, Path::new("/p/a.md"), dir).indent_size, Some(IndentSize::Columns(2)));
    }

    // "Said nothing" and "said the default" are different answers.
    #[test]
    fn an_unset_property_stays_unset() {
        let p = properties_for("[*]\nindent_size = 4\n", Path::new("/p/a.rs"), Path::new("/p"));
        assert_eq!(p.indent_size, Some(IndentSize::Columns(4)));
        assert_eq!(p.trim_trailing_whitespace, None);
        assert_eq!(p.end_of_line, None);
    }

    // The spec's own resolution rules between the three indent
    // properties.
    #[test]
    fn indent_width_resolves_size_against_tab_width() {
        let width = |text: &str| properties_for(text, Path::new("/p/a.rs"), Path::new("/p")).width();
        assert_eq!(width("[*]\nindent_size = 2\n"), Some(2));
        assert_eq!(width("[*]\nindent_size = tab\ntab_width = 8\n"), Some(8));
        assert_eq!(width("[*]\ntab_width = 8\n"), Some(8), "tab_width alone decides it");
        assert_eq!(width("[*]\nindent_style = tab\n"), None, "nothing here says how wide");
    }

    #[test]
    fn names_and_values_are_case_insensitive() {
        let p = properties_for("[*]\nIndent_Style = TAB\nInsert_Final_Newline = True\n", Path::new("/p/a"), Path::new("/p"));
        assert_eq!(p.indent_style, Some(IndentStyle::Tab));
        assert_eq!(p.insert_final_newline, Some(true));
    }

    // A `.editorconfig` is shared between editors, so every editor sees
    // properties meant for the others.
    #[test]
    fn unknown_properties_are_ignored_in_silence() {
        let p = properties_for("[*]\nquote_type = single\nindent_size = 2\n", Path::new("/p/a"), Path::new("/p"));
        assert_eq!(p.indent_size, Some(IndentSize::Columns(2)));
    }

    #[test]
    fn nothing_typeable_panics() {
        for text in ["", "[", "[]", "[*]", "=", "[*]\n=\n", "{", "}", "{,}", "{..}", "[*]\nindent_size =\n"] {
            sections(text);
            is_root(text);
            expand_braces(text);
            matches(text, "a/b.txt");
        }
    }

    // The cascade, against real files since it is entirely about walking
    // a directory tree.
    #[test]
    fn a_deeper_config_wins_and_root_stops_the_walk() {
        let dir = std::env::temp_dir().join(format!("bish-editorconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project/src")).unwrap();
        // Outside the project, and never reached: the project's own
        // config is `root`.
        std::fs::write(dir.join(".editorconfig"), "[*]\nindent_size = 99\nend_of_line = cr\n").unwrap();
        std::fs::write(dir.join("project/.editorconfig"), "root = true\n[*]\nindent_size = 4\ninsert_final_newline = true\n").unwrap();
        std::fs::write(dir.join("project/src/.editorconfig"), "[*]\nindent_size = 2\n").unwrap();

        let p = for_file(&dir.join("project/src/main.rs"));
        assert_eq!(p.indent_size, Some(IndentSize::Columns(2)), "the deepest file wins");
        assert_eq!(p.insert_final_newline, Some(true), "and what it didn't mention comes from above");
        assert_eq!(p.end_of_line, None, "the walk stopped at `root = true`");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
