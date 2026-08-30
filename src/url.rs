// URLs: parsing one, and finding them in ordinary text.
//
// Two jobs that sound like one and are not. `parse` takes a string that
// is supposed to be a URL and says what its parts are. `find` takes
// arbitrary text -- a comment, a commit message, a `.env` value -- and
// says which stretches of it are URLs, which is a different and
// fiddlier question, because a URL in prose has no delimiters. The hard
// part there is entirely about where one *ends*:
//
//     see https://example.com.          <- the full stop is a sentence's
//     (https://en.wikipedia.org/x_(y))  <- but one of those parens isn't
//
// So `find` scans a candidate to the first character that cannot be in
// a URL, then walks back over trailing punctuation, keeping a closing
// bracket only when there is an opening one inside the URL to match it.
// A candidate that doesn't `parse` is then dropped, which is what keeps
// the two halves of this module honest about each other.
//
// Offsets are **char** offsets, matching `highlight::HighlightSpan`.
#![allow(dead_code)]

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub userinfo: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Always present, possibly empty. `https://example.com` has an
    /// empty path, which is not the same as `/`.
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl Url {
    /// `https://example.com:8080` -- what a caller would show as "where
    /// this points", without the path. `None` for a scheme with no
    /// authority at all (`mailto:`).
    pub fn origin(&self) -> Option<String> {
        let host = self.host.as_ref()?;
        Some(match self.port {
            Some(port) => format!("{}://{}:{}", self.scheme, host, port),
            None => format!("{}://{}", self.scheme, host),
        })
    }
}

/// RFC 3986's shape, leniently: this reports what the parts *are*, and
/// does not judge whether each one holds only characters its grammar
/// allows. A URL that came out of a real file is far more likely to be
/// slightly out of spec than to be something else entirely, and a
/// consumer that wants to open it cares about the parts, not the
/// verdict. `None` is reserved for text that isn't a URL at all: no
/// scheme, or nothing after it.
pub fn parse(text: &str) -> Option<Url> {
    let (scheme, rest) = split_scheme(text)?;
    // `//` introduces an authority; without it everything after the
    // colon is the path (`mailto:a@b`, `file:relative`).
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
    Some(Url {
        scheme,
        userinfo,
        host,
        port,
        path: path.to_string(),
        query: query.map(str::to_string),
        fragment: fragment.map(str::to_string),
    })
}

// `scheme:` -- a letter followed by letters, digits, `+`, `-` or `.`.
// Split out rather than inlined because the `://` in `find` has to agree
// with it exactly, and two spellings of "what a scheme looks like" is
// how a detector starts finding things the parser then rejects.
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

// The host and port, with an IPv6 literal's own brackets kept: the
// colons inside `[::1]` are part of the address, not a port separator.
// Only ever called when an authority was actually present, so an empty
// one is `Some("")` rather than `None`: `file:///x` has an authority
// and it is empty, which is a different thing from `mailto:`, which has
// none at all.
fn split_host_port(hostport: &str) -> (Option<String>, Option<u16>) {
    if let Some(close) = hostport.strip_prefix('[').and_then(|rest| rest.find(']')) {
        let host = &hostport[..close + 2];
        let port = hostport[close + 2..].strip_prefix(':').and_then(|p| p.parse().ok());
        return (Some(host.to_string()), port);
    }
    match hostport.rsplit_once(':') {
        // A trailing `:` with no digits, or something that isn't a port,
        // stays part of the host rather than being silently dropped.
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

/// Every URL in `text`, as char-offset ranges. Only schemes with an
/// authority (`https://`, `file://`, `ssh://`, ...) plus the few
/// authority-less ones worth recognizing in prose -- `mailto:` above
/// all. A bare `www.example.com` is deliberately not found: guessing at
/// a missing scheme is how a detector starts underlining ordinary
/// sentences that happen to contain a dot.
pub fn find(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Range<usize>> = Vec::new();
    for (i, c) in chars.iter().enumerate() {
        if *c != ':' {
            continue;
        }
        // Inside a URL already found, so not the start of another.
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

// Walks back from a `:` over the scheme before it, returning where that
// scheme starts. `None` when what precedes isn't one -- which includes
// the very common `a:b` inside ordinary text.
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

// Whether the colon at `colon` really opens a URL: either `//` follows,
// or the scheme is one of the few that legitimately has no authority
// and is still worth finding in prose.
fn opens_a_url(chars: &[char], colon: usize) -> bool {
    if chars.get(colon + 1) == Some(&'/') && chars.get(colon + 2) == Some(&'/') {
        return true;
    }
    let scheme: String = chars[scheme_start(chars, colon).unwrap_or(colon)..colon].iter().collect();
    matches!(scheme.to_ascii_lowercase().as_str(), "mailto" | "tel")
}

// The first character that cannot be inside a URL. Whitespace ends one,
// as do the characters that habitually *surround* one in text: quotes,
// backticks, angle brackets.
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

// Walks back over punctuation that belongs to the sentence rather than
// to the URL. A closing bracket is kept only when there is an opening
// one inside the URL for it to match -- which is what tells
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

    // The colons inside an IPv6 literal are the address, not a port.
    #[test]
    fn an_ipv6_host_keeps_its_brackets_and_finds_its_port() {
        let u = parse("http://[::1]:8080/x").unwrap();
        assert_eq!(u.host.as_deref(), Some("[::1]"));
        assert_eq!(u.port, Some(8080));
        assert_eq!(parse("http://[::1]/x").unwrap().port, None);
    }

    // Something that isn't a port stays part of the host rather than
    // being quietly dropped.
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

    // The sentence's full stop is not the URL's.
    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_url() {
        assert_eq!(found("see https://example.com/x."), vec!["https://example.com/x"]);
        assert_eq!(found("really? https://example.com!"), vec!["https://example.com"]);
        assert_eq!(found("a, https://example.com, b"), vec!["https://example.com"]);
    }

    // The classic one: a closing paren is the URL's only when it has an
    // opening one to match.
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

    // A colon inside a URL must not start a second one.
    #[test]
    fn a_colon_inside_a_url_does_not_start_another() {
        assert_eq!(found("https://example.com:8080/a:b"), vec!["https://example.com:8080/a:b"]);
    }

    #[test]
    fn finds_mailto_but_not_a_bare_address() {
        assert_eq!(found("write to mailto:a@example.com please"), vec!["mailto:a@example.com"]);
        assert_eq!(found("write to a@example.com please"), Vec::<String>::new());
    }

    // The things that look like schemes in ordinary text and are not.
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
}
