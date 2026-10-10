// URLs: parsing one, and finding them in text.
//
// `parse` takes a string meant to be a URL and names its parts. `find`
// takes arbitrary text (a comment, a commit message, a `.env` value)
// and says which stretches are URLs, where the hard part is the end,
// as prose has no delimiters:
//
//     see https://example.com.          <- the full stop is a sentence's
//     (https://en.wikipedia.org/x_(y))  <- but one of those parens isn't
//
// So `find` scans to the first character that can't be in a URL, then
// backs off trailing punctuation, keeping a closing bracket only if an
// opening one inside the URL matches it. A candidate that doesn't
// `parse` is dropped, keeping the two halves consistent.
//
// Offsets are char offsets, as in `highlight::HighlightSpan`.
#![allow(dead_code)]

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub userinfo: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Always present, maybe empty: `https://example.com` has an empty path,
    /// not `/`.
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl Url {
    /// `https://example.com:8080`: where this points, without the path.
    /// `None` for a scheme with no authority (`mailto:`).
    pub fn origin(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        Some(match self.port {
            Some(port) => format!("{}://{}:{}", self.scheme, host, port),
            None => format!("{}://{}", self.scheme, host),
        })
    }
}

/// RFC 3986's shape, leniently: this names the parts without checking
/// each one's characters. A URL from a real file is more likely slightly
/// out of spec than not a URL, and a consumer opening it wants the parts.
/// `None` only for text that isn't a URL: no scheme, or nothing after it.
pub fn parse(text: &str) -> Option<Url> {
    let (scheme, rest) = split_scheme(text)?;
    // `//` introduces an authority; without it everything after the colon
    // is the path (`mailto:a@b`, `file:relative`).
    let (authority, rest) = match rest.strip_prefix("//") {
        Some(rest) => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            (Some(&rest[..end]), &rest[end..])
        }
        None => (None, rest),
    };
    let (rest, fragment) = split_once(rest, '#');
    let (path, query) = split_once(rest, '?');
    if authority.is_none() && path.is_empty() && query.is_none() && fragment.is_none() {
        return None;
    }
    let (userinfo, hostport) = match authority {
        Some(authority) => match authority.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_string()), Some(host)),
            None => (None, Some(authority)),
        },
        None => (None, None),
    };
    let (host, port) = match hostport {
        Some(hostport) => split_host_port(hostport),
        None => (None, None),
    };
    Some(Url { scheme, userinfo, host, port, path: path.to_string(), query: query.map(str::to_string), fragment: fragment.map(str::to_string) })
}

// `scheme:`: a letter, then letters, digits, `+`, `-` or `.`. Shared
// with `find`'s `://` check, so the detector can't find what the parser
// rejects.
fn split_scheme(text: &str) -> Option<(String, &str)> {
    let colon = text.find(':')?;
    let scheme = &text[..colon];
    let mut chars = scheme.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), &text[colon + 1..]))
}

// The host and port, keeping an IPv6 literal's brackets: the colons in
// `[::1]` aren't a port separator. Called only with an authority, so an
// empty one is `Some("")`: `file:///x` has an empty authority, unlike
// `mailto:`, which has none.
fn split_host_port(hostport: &str) -> (Option<String>, Option<u16>) {
    if let Some(close) = hostport.strip_prefix('[').and_then(|rest| rest.find(']')) {
        let host = &hostport[..close + 2];
        let port = hostport[close + 2..].strip_prefix(':').and_then(|p| p.parse().ok());
        return (Some(host.to_string()), port);
    }
    match hostport.rsplit_once(':') {
        // A trailing `:` with no digits, or a non-port, stays in the host
        // rather than being dropped.
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (Some(host.to_string()), Some(port)),
            Err(_) => (Some(hostport.to_string()), None),
        },
        None => (Some(hostport.to_string()), None),
    }
}

fn split_once(text: &str, sep: char) -> (&str, Option<&str>) {
    match text.split_once(sep) {
        Some((before, after)) => (before, Some(after)),
        None => (text, None),
    }
}

/// Every URL in `text`, as char ranges: schemes with an authority
/// (`https://`, `file://`, `ssh://`, ...) plus the few authority-less
/// ones worth finding in prose, `mailto:` above all. A bare
/// `www.example.com` isn't found: guessing a scheme underlines ordinary
/// sentences with a dot in them.
pub fn find(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Range<usize>> = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        if *c != ':' {
            continue;
        }
        // Inside a URL already found, so not another's start.
        if out.last().is_some_and(|last| last.end > i) {
            continue;
        }
        let Some(start) = scheme_start(&chars, i) else { continue };
        if !opens_a_url(&chars, i) {
            continue;
        }
        let end = trim_trailing(&chars, start, url_end(&chars, i));
        if end <= i + 1 {
            continue;
        }
        let candidate: String = chars[start..end].iter().collect();
        if parse(&candidate).is_some() {
            out.push(start..end);
        }
    }
    out
}

// Walks back from a `:` over the scheme, returning where it starts.
// `None` when there's none, as in the common `a:b` in prose.
fn scheme_start(chars: &[char], colon: usize) -> Option<usize> {
    let mut start = colon;
    while start > 0 && is_scheme_char(chars[start - 1]) {
        start -= 1;
    }
    if start == colon || !chars[start].is_ascii_alphabetic() {
        return None;
    }
    Some(start)
}

// Whether the colon opens a URL: `//` follows, or the scheme is one of
// the few authority-less ones worth finding.
fn opens_a_url(chars: &[char], colon: usize) -> bool {
    if chars.get(colon + 1) == Some(&'/') && chars.get(colon + 2) == Some(&'/') {
        return true;
    }
    let scheme: String = chars[scheme_start(chars, colon).unwrap_or(colon)..colon].iter().collect();
    matches!(scheme.to_ascii_lowercase().as_str(), "mailto" | "tel")
}

// The first character that can't be in a URL: whitespace, and what
// habitually surrounds one in text: quotes, backticks, angle brackets.
fn url_end(chars: &[char], colon: usize) -> usize {
    let mut end = colon + 1;
    while end < chars.len() {
        let c = chars[end];
        if c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '\\' | '|' | '^') {
            break;
        }
        end += 1;
    }
    end
}

// Backs off punctuation belonging to the sentence. A closing bracket
// stays only if an opening one inside the URL matches it, telling
// `(https://x/a)` from `https://x/a_(b)`.
fn trim_trailing(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start {
        let last = chars[end - 1];
        let balanced = |open: char, close: char| {
            let inner = &chars[start..end - 1];
            inner.iter().filter(|c| **c == open).count() > inner.iter().filter(|c| **c == close).count()
        };
        let drop = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' => true,
            ')' => !balanced('(', ')'),
            ']' => !balanced('[', ']'),
            '}' => !balanced('{', '}'),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
}

/// Whether a URL is safe inside an OSC 8 escape sequence. It comes from a
/// file (a markdown link, a string in source), and a control character
/// would end the sequence early and hand the terminal what follows as
/// commands. One needing repair is refused rather than sanitised.
pub fn is_safe(url: &str) -> bool {
    !url.is_empty() && !url.chars().any(|c| c.is_control())
}

/// A link target a terminal can open. A URL is returned as is; a
/// relative path (a markdown link to a sibling document) is resolved
/// against `base_dir` into a `file://` URL. Without a base directory a
/// relative target gets no link rather than a guess.
pub fn absolute(target: &str, base_dir: Option<&std::path::Path>) -> Option<String> {
    if parse(target).is_some() {
        return Some(target.to_string());
    }
    let joined = base_dir?.join(target);
    let resolved = std::fs::canonicalize(&joined).unwrap_or(joined);
    Some(format!("file://{}", resolved.display()))
}

/// A percent-encoded `file://` URL for an absolute path, for a language
/// server, which matches `rootUri` and `textDocument.uri` exactly, so a
/// space or `#` must be encoded. (`absolute` is for terminals, which are
/// forgiving.)
///
/// Everything outside RFC 3986's unreserved set is encoded, byte by byte
/// of the UTF-8, keeping `/`. Over-encoding is safe; under-encoding isn't.
pub fn from_file_path(path: &std::path::Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => out.push(*byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// `from_file_path`'s inverse: the path a `file://` URL names, or `None`.
/// Diagnostics arrive addressed by URI, and the buffer is found by path.
///
/// Percent-decoding is byte-wise and the result must be UTF-8: a path
/// bish couldn't have encoded is rejected. So is an authority
/// (`file://host/path`): bish has no remote files.
pub fn to_file_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` (empty authority) is the usual form; anything before
    // the first `/` is a host.
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(std::path::PathBuf::from(String::from_utf8(out).ok()?))
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> Vec<String> {
        find(text).into_iter().map(|r| text.chars().skip(r.start).take(r.end - r.start).collect()).collect()
    }

    #[test]
    fn a_file_path_round_trips_through_a_uri_including_the_awkward_characters() {
        for path in [
            "/plain/x.sh",
            // What breaks a naive encoder: a space and a `#` mean something
            // in a URI.
            "/a dir/my file.rs",
            "/proj/notes#1.md",
            "/proj/100% done/x.py",
            "/proj/a+b/c?d.ts",
            // Non-ASCII goes byte-wise through UTF-8.
            "/proj/\u{e4}\u{e4}kk\u{f6}nen/\u{1f30d}.md",
        ] {
            let uri = from_file_path(std::path::Path::new(path));
            assert!(uri.starts_with("file:///"), "{uri}");
            assert!(!uri.contains(' '), "a space must not survive unencoded: {uri}");
            assert_eq!(to_file_path(&uri).as_deref(), Some(std::path::Path::new(path)), "via {uri}");
        }
    }

    #[test]
    fn only_a_local_file_url_is_a_path() {
        assert_eq!(to_file_path("https://example.com/x"), None);
        // An authority names a host; bish has no remote buffers.
        assert_eq!(to_file_path("file://host/x"), None);
        // Truncated or non-hex escapes are refused.
        assert_eq!(to_file_path("file:///a%"), None);
        assert_eq!(to_file_path("file:///a%zz"), None);
        // Non-UTF-8 bytes are a path bish couldn't have named.
        assert_eq!(to_file_path("file:///a%FF"), None);
    }

    #[test]
    fn parses_every_part() {
        let u = parse("https://user:pw@example.com:8443/a/b?q=1&r=2#frag").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.userinfo.as_deref(), Some("user:pw"));
        assert_eq!(u.host.as_deref(), Some("example.com"));
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("q=1&r=2"));
        assert_eq!(u.fragment.as_deref(), Some("frag"));
    }

    #[test]
    fn a_url_with_no_path_has_an_empty_one() {
        let u = parse("https://example.com").unwrap();
        assert_eq!(u.path, "", "which is not the same as `/`");
        assert_eq!(parse("https://example.com/").unwrap().path, "/");
    }

    #[test]
    fn the_scheme_is_case_folded() {
        assert_eq!(parse("HTTPS://example.com").unwrap().scheme, "https");
    }

    // No `//`, so everything after the colon is the path.
    #[test]
    fn a_scheme_without_an_authority_is_all_path() {
        let u = parse("mailto:someone@example.com").unwrap();
        assert_eq!(u.scheme, "mailto");
        assert_eq!(u.host, None);
        assert_eq!(u.path, "someone@example.com");
    }

    #[test]
    fn a_file_url_keeps_its_absolute_path() {
        let u = parse("file:///home/jussi/bish/src/url.rs").unwrap();
        assert_eq!(u.host.as_deref(), Some(""), "the empty authority `file://` really has");
        assert_eq!(u.path, "/home/jussi/bish/src/url.rs");
    }

    // The colons in an IPv6 literal are the address.
    #[test]
    fn an_ipv6_host_keeps_its_brackets_and_finds_its_port() {
        let u = parse("http://[::1]:8080/x").unwrap();
        assert_eq!(u.host.as_deref(), Some("[::1]"));
        assert_eq!(u.port, Some(8080));
        assert_eq!(parse("http://[::1]/x").unwrap().port, None);
    }

    // A non-port stays part of the host.
    #[test]
    fn a_colon_that_is_not_a_port_is_left_in_the_host() {
        let u = parse("http://example.com:notaport/x").unwrap();
        assert_eq!(u.host.as_deref(), Some("example.com:notaport"));
        assert_eq!(u.port, None);
    }

    #[test]
    fn text_that_is_not_a_url_does_not_parse() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("example.com"), None, "no scheme");
        assert_eq!(parse("2:30"), None, "a digit can't start a scheme");
        assert_eq!(parse("https:"), None, "nothing after it");
    }

    #[test]
    fn finds_a_url_in_the_middle_of_a_sentence() {
        assert_eq!(found("see https://example.com/x for more"), vec!["https://example.com/x"]);
    }

    // The sentence's full stop isn't the URL's.
    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_url() {
        assert_eq!(found("see https://example.com/x."), vec!["https://example.com/x"]);
        assert_eq!(found("really? https://example.com!"), vec!["https://example.com"]);
        assert_eq!(found("a, https://example.com, b"), vec!["https://example.com"]);
    }

    // A closing paren is the URL's only with an opening one to match.
    #[test]
    fn a_closing_bracket_is_kept_only_when_it_balances() {
        assert_eq!(found("(https://example.com/a)"), vec!["https://example.com/a"]);
        assert_eq!(found("https://en.wikipedia.org/wiki/Foo_(bar)"), vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]);
        assert_eq!(found("[https://example.com/a]"), vec!["https://example.com/a"]);
    }

    #[test]
    fn quotes_and_angle_brackets_delimit_rather_than_belong() {
        assert_eq!(found("\"https://example.com/a\""), vec!["https://example.com/a"]);
        assert_eq!(found("<https://example.com/a>"), vec!["https://example.com/a"]);
        assert_eq!(found("`https://example.com/a`"), vec!["https://example.com/a"]);
    }

    #[test]
    fn finds_several_and_does_not_run_them_together() {
        assert_eq!(found("https://a.example https://b.example"), vec!["https://a.example", "https://b.example"]);
    }

    // A colon inside a URL doesn't start another.
    #[test]
    fn a_colon_inside_a_url_does_not_start_another() {
        assert_eq!(found("https://example.com:8080/a:b"), vec!["https://example.com:8080/a:b"]);
    }

    #[test]
    fn finds_mailto_but_not_a_bare_address() {
        assert_eq!(found("write to mailto:a@example.com please"), vec!["mailto:a@example.com"]);
        assert_eq!(found("write to a@example.com please"), Vec::<String>::new());
    }

    // Things in ordinary text that look like schemes and aren't.
    #[test]
    fn ordinary_text_with_colons_finds_nothing() {
        assert_eq!(found("note: this is a note"), Vec::<String>::new());
        assert_eq!(found("at 12:30 today"), Vec::<String>::new());
        assert_eq!(found("key: value"), Vec::<String>::new());
        assert_eq!(found("C:\\Users\\me"), Vec::<String>::new());
        assert_eq!(found("std::vec::Vec"), Vec::<String>::new());
    }

    #[test]
    fn finds_the_other_schemes_that_turn_up_in_files() {
        assert_eq!(found("git+ssh://git@example.com/r.git"), vec!["git+ssh://git@example.com/r.git"]);
        assert_eq!(found("file:///etc/hosts"), vec!["file:///etc/hosts"]);
        assert_eq!(found("ws://localhost:9222/x"), vec!["ws://localhost:9222/x"]);
    }

    #[test]
    fn spans_are_char_offsets() {
        let text = "\u{e4}\u{e4} https://example.com";
        assert_eq!(find(text), vec![3..22]);
    }

    #[test]
    fn nothing_typeable_panics() {
        for text in ["", ":", "://", "http:", "http://", "a:", "://a", "https://[", "https://[]:", "mailto:", "..:.."] {
            find(text);
            parse(text);
        }
    }

    #[test]
    fn origin_is_the_scheme_and_host() {
        assert_eq!(parse("https://example.com/a?b").unwrap().origin().as_deref(), Some("https://example.com"));
        assert_eq!(parse("https://example.com:8443/a").unwrap().origin().as_deref(), Some("https://example.com:8443"));
        assert_eq!(parse("mailto:a@b").unwrap().origin(), None);
    }

    #[test]
    fn a_url_with_a_control_character_is_not_safe_to_emit() {
        assert!(is_safe("https://example.com/a"));
        assert!(!is_safe(""));
        assert!(!is_safe("https://x\u{1b}]0;pwned\u{7}"));
        assert!(!is_safe("https://x\n"));
    }

    #[test]
    fn absolute_passes_a_url_through_and_resolves_a_path() {
        assert_eq!(absolute("https://example.com", None).as_deref(), Some("https://example.com"));
        let base = std::path::Path::new("/tmp/somewhere");
        assert_eq!(absolute("plan.md", Some(base)).as_deref(), Some("file:///tmp/somewhere/plan.md"));
        assert_eq!(absolute("plan.md", None), None, "nothing to be relative to");
    }
}
