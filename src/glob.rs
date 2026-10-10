// Shell glob matching: `*`, `?`, `[abc]`, `[a-z]`, `[!abc]`/`[^abc]`, and
// the extglob groups `@(...)`, `!(...)`, `+(...)`, `*(...)`, `?(...)`.
// bash needs `shopt -s extglob` for those; bish always recognizes them,
// which differs only for a script using `!(foo)` as literal text. Shared
// by pathname expansion, `case` and `[[ ]]`'s `==`/`!=`.

pub fn matches(pattern: &str, text: &str) -> bool {
    match_here(pattern.as_bytes(), text.as_bytes())
}

/// `matches`, optionally folding case, for `shopt -s nocasematch` in
/// `case` and `[[ == ]]`. Both sides are folded, as `nocaseglob` does for
/// pathnames (see `read_names`).
pub fn matches_with_case(pattern: &str, text: &str, fold_case: bool) -> bool {
    match fold_case {
        true => matches(&pattern.to_lowercase(), &text.to_lowercase()),
        false => matches(pattern, text),
    }
}

/// The same, but `*` and `?` never cross a `/`: pathname semantics, C's
/// `FNM_PATHNAME`. Pathname expansion and `GLOBIGNORE` use it, so `*.o`
/// drops `a.o` but not `sub/x.o`, as in bash.
pub fn matches_path(pattern: &str, text: &str) -> bool {
    let (pat, txt): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), text.split('/').collect());
    pat.len() == txt.len() && pat.iter().zip(txt.iter()).all(|(p, t)| matches(p, t))
}

/// How many characters a pattern must match, or `None` with a `*` or an
/// extglob group. `${v//pat/repl}` otherwise tries every end position at
/// every start; a fixed width gives one end per start, the common shape
/// (`${s//a/b}`, `${path//\//_}`).
pub fn fixed_width(pattern: &str) -> Option<usize> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut width = 0usize;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => return None,
            // An extglob group repeats or alternates, and `?(...)`/`+(...)`
            // can match nothing.
            '@' | '!' | '+' | '?' if chars.get(i + 1) == Some(&'(') => return None,
            '\\' => {
                i += 1;
                width += 1;
            }
            '[' => {
                // To the closing bracket; `[]]` and `[^]]` put a literal
                // `]` first.
                let mut j = i + 1;
                if matches!(chars.get(j), Some('!') | Some('^')) {
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    // An escaped `]` is a member, not the end.
                    if chars[j] == '\\' && j + 1 < chars.len() {
                        j += 2;
                        continue;
                    }
                    // `[:space:]`, `[.coll.]`, `[=equiv=]` each hold a `]`
                    // that doesn't close the class: `[[:space:]]` is one
                    // character wide.
                    if chars[j] == '[' && matches!(chars.get(j + 1), Some(':') | Some('.') | Some('=')) {
                        let kind = chars[j + 1];
                        let mut k = j + 2;
                        while k + 1 < chars.len() && !(chars[k] == kind && chars[k + 1] == ']') {
                            k += 1;
                        }
                        // Unterminated: leave `j` for the scan below.
                        if k + 1 < chars.len() {
                            j = k + 1;
                        }
                    }
                    j += 1;
                }
                // Unterminated: the `[` is a literal, and so is the rest.
                if j >= chars.len() {
                    width += chars.len() - i;
                    return Some(width);
                }
                i = j;
                width += 1;
            }
            _ => width += 1,
        }
        i += 1;
    }
    Some(width)
}

fn match_here(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    // `\c` matches a literal `c`, even a metacharacter, so a pattern built
    // from a partly quoted word (`escape`, `expand_glob_pattern` in exec.rs)
    // keeps quoted chunks literal and unquoted ones as syntax, as for the
    // `=~` operand.
    if pat[0] == b'\\' && pat.len() > 1 {
        return !text.is_empty() && text[0] == pat[1] && match_here(&pat[2..], &text[1..]);
    }
    if matches!(pat[0], b'@' | b'!' | b'+' | b'*' | b'?') && pat.len() > 1 && pat[1] == b'(' {
        if let Some((alts, group, rest)) = find_group(&pat[1..]) {
            return match_extglob(pat[0], &alts, group, rest, text);
        }
    }
    match pat[0] {
        b'*' => {
            for i in 0..=text.len() {
                if match_here(&pat[1..], &text[i..]) {
                    return true;
                }
            }
            false
        }
        b'?' => !text.is_empty() && match_here(&pat[1..], &text[1..]),
        b'[' => match match_class(pat, text.first().copied()) {
            Some((true, rest)) => match_here(rest, &text[1..]),
            Some((false, _)) => false,
            None => !text.is_empty() && text[0] == b'[' && match_here(&pat[1..], &text[1..]),
        },
        c => !text.is_empty() && text[0] == c && match_here(&pat[1..], &text[1..]),
    }
}

// Splits a `(alt1|alt2|...)` group (pat[0] == '(') into its top-level
// alternatives, nested groups respected. Returns the alternatives, the
// group's text with its parens, and what follows; `None` if
// unterminated.
fn find_group(pat: &[u8]) -> Option<(Vec<&[u8]>, &[u8], &[u8])> {
    if pat.is_empty() || pat[0] != b'(' {
        return None;
    }
    let mut depth = 0i32;
    let mut alt_start = 1;
    let mut alts: Vec<&[u8]> = Vec::new();
    for i in 0..pat.len() {
        match pat[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    alts.push(&pat[alt_start..i]);
                    return Some((alts, &pat[0..=i], &pat[i + 1..]));
                }
            }
            b'|' if depth == 1 => {
                alts.push(&pat[alt_start..i]);
                alt_start = i + 1;
            }
            _ => {}
        }
    }
    None
}

fn match_extglob(prefix: u8, alts: &[&[u8]], group: &[u8], rest: &[u8], text: &[u8]) -> bool {
    match prefix {
        b'@' => alts.iter().any(|alt| {
            let mut combined = alt.to_vec();
            combined.extend_from_slice(rest);
            match_here(&combined, text)
        }),
        b'?' => {
            match_here(rest, text)
                || alts.iter().any(|alt| {
                    let mut combined = alt.to_vec();
                    combined.extend_from_slice(rest);
                    match_here(&combined, text)
                })
        }
        b'*' => {
            match_here(rest, text)
                || alts.iter().any(|alt| {
                    let mut combined = alt.to_vec();
                    combined.push(b'*');
                    combined.extend_from_slice(group);
                    combined.extend_from_slice(rest);
                    match_here(&combined, text)
                })
        }
        b'+' => alts.iter().any(|alt| {
            let mut combined = alt.to_vec();
            combined.push(b'*');
            combined.extend_from_slice(group);
            combined.extend_from_slice(rest);
            match_here(&combined, text)
        }),
        b'!' => (0..=text.len()).any(|i| {
            let excluded = alts.iter().any(|alt| match_here(alt, &text[..i]));
            !excluded && match_here(rest, &text[i..])
        }),
        _ => unreachable!(),
    }
}

/// The ASCII members of a POSIX character class, as byte ranges. A
/// `[:space:]` is a name inside a bracket expression, and its `]` doesn't
/// close the bracket. Ranges, so the regex engine can share the table.
pub(crate) fn posix_class_ranges(name: &[u8]) -> Option<&'static [(u8, u8)]> {
    Some(match name {
        b"alpha" => &[(b'A', b'Z'), (b'a', b'z')],
        b"digit" => &[(b'0', b'9')],
        b"alnum" => &[(b'0', b'9'), (b'A', b'Z'), (b'a', b'z')],
        b"upper" => &[(b'A', b'Z')],
        b"lower" => &[(b'a', b'z')],
        // 0x09..0x0d includes vertical tab, which `is_ascii_whitespace`
        // leaves out.
        b"space" => &[(0x09, 0x0d), (b' ', b' ')],
        b"blank" => &[(0x09, 0x09), (b' ', b' ')],
        b"punct" => &[(0x21, 0x2f), (0x3a, 0x40), (0x5b, 0x60), (0x7b, 0x7e)],
        b"print" => &[(0x20, 0x7e)],
        b"graph" => &[(0x21, 0x7e)],
        b"cntrl" => &[(0x00, 0x1f), (0x7f, 0x7f)],
        b"xdigit" => &[(b'0', b'9'), (b'A', b'F'), (b'a', b'f')],
        _ => return None,
    })
}

/// The length of a `[:name:]` at the start of `pat`, brackets included,
/// and the name.
pub(crate) fn posix_class_at(pat: &[u8]) -> Option<(&[u8], usize)> {
    if pat.len() < 4 || pat[0] != b'[' || pat[1] != b':' {
        return None;
    }
    let mut i = 2;
    while i + 1 < pat.len() && !(pat[i] == b':' && pat[i + 1] == b']') {
        i += 1;
    }
    if i + 1 >= pat.len() {
        return None;
    }
    Some((&pat[2..i], i + 2))
}

// One bracket-expression member at `k`: `\c` is a literal `c`, so an
// escaped `-`, `]`, `^` or `\` is a member, not syntax. Returns the
// character and the next index.
fn class_member(class: &[u8], k: usize) -> (u8, usize) {
    match class[k] == b'\\' && k + 1 < class.len() {
        true => (class[k + 1], k + 2),
        false => (class[k], k + 1),
    }
}

// `pat[0]` is `[`. Returns whether `c` matched and the pattern after the
// closing `]`, or `None` with no closing `]`, when `[` is literal.
pub(crate) fn match_class(pat: &[u8], c: Option<u8>) -> Option<(bool, &[u8])> {
    let mut i = 1;
    let negate = i < pat.len() && (pat[i] == b'!' || pat[i] == b'^');
    if negate {
        i += 1;
    }
    let start = i;
    let mut j = i;
    if j < pat.len() && pat[j] == b']' {
        j += 1;
    }
    while j < pat.len() && pat[j] != b']' {
        // An escaped `]` is a member: `[\]]` holds a `]`.
        if pat[j] == b'\\' && j + 1 < pat.len() {
            j += 2;
            continue;
        }
        // A `[:name:]`'s `]` isn't this bracket's either.
        if let Some((_, len)) = posix_class_at(&pat[j..]) {
            j += len;
            continue;
        }
        j += 1;
    }
    if j >= pat.len() {
        return None;
    }
    let class = &pat[start..j];
    let c = c?;
    let mut matched = false;
    let mut k = 0;
    while k < class.len() {
        if let Some((name, len)) = posix_class_at(&class[k..]) {
            if posix_class_ranges(name).is_some_and(|rs| rs.iter().any(|(lo, hi)| c >= *lo && c <= *hi)) {
                matched = true;
            }
            k += len;
            continue;
        }
        let (lo, next) = class_member(class, k);
        // A range needs a bare `-` between two members; `[a\-z]` is a, `-`
        // and z.
        if next < class.len() && class[next] == b'-' && next + 1 < class.len() {
            let (hi, after) = class_member(class, next + 1);
            if c >= lo && c <= hi {
                matched = true;
            }
            k = after;
        } else {
            if lo == c {
                matched = true;
            }
            k = next;
        }
    }
    if negate {
        matched = !matched;
    }
    Some((matched, &pat[j + 1..]))
}

/// Whether `s` has an unescaped metacharacter. Quoted parts arrive
/// escaped, so `printf '[%s]'` comes as `\[%s\]` and isn't a pattern,
/// and doesn't vanish under `nullglob`.
pub fn has_meta(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if matches!(bytes[i], b'*' | b'?') {
            return true;
        }
        if bytes[i] == b'[' && closing_bracket(&bytes[i..]).is_some() {
            return true;
        }
        if matches!(bytes[i], b'@' | b'!' | b'+') && bytes.get(i + 1) == Some(&b'(') {
            return true;
        }
        i += 1;
    }
    false
}

/// Where the bracket expression at `pat[0]` closes, if it does,
/// agreeing with `match_class`: a `[` with no `]` is the character `[`.
/// Otherwise `[ $n -lt 300 ]` would scan the directory for each `[` and
/// `]` on every iteration.
fn closing_bracket(pat: &[u8]) -> Option<usize> {
    let mut i = 1;
    if i < pat.len() && (pat[i] == b'!' || pat[i] == b'^') {
        i += 1;
    }
    // A `]` first is literal: `[]]` matches `]`.
    if i < pat.len() && pat[i] == b']' {
        i += 1;
    }
    while i < pat.len() && pat[i] != b']' {
        if let Some((_, len)) = posix_class_at(&pat[i..]) {
            i += len;
            continue;
        }
        i += 1;
    }
    (i < pat.len()).then_some(i)
}

// Escapes everything `match_here` treats specially, so the result
// matches only the literal text. `expand_glob_pattern` (exec.rs) uses it
// for a word's quoted chunks inside a pattern, as `regex::escape` serves
// `=~`. `]`, `^` and `-` are included because a quoted chunk can land
// inside a bracket expression (`[\^]`); escaping them elsewhere is
// harmless.
pub fn escape(s: &str) -> std::borrow::Cow<'_, str> {
    // Borrowed when there's nothing to escape, the usual case.
    match s.bytes().any(is_pattern_syntax) {
        false => std::borrow::Cow::Borrowed(s),
        true => {
            let mut out = String::with_capacity(s.len() + 4);
            for c in s.chars() {
                if c.is_ascii() && is_pattern_syntax(c as u8) {
                    out.push('\\');
                }
                out.push(c);
            }
            std::borrow::Cow::Owned(out)
        }
    }
}

/// One list, so the check and the loop agree.
fn is_pattern_syntax(b: u8) -> bool {
    matches!(b, b'*' | b'?' | b'[' | b']' | b'\\' | b'@' | b'!' | b'+' | b'(' | b'^' | b'-')
}

/// The `shopt` settings that change pathname matching. Passed in, since
/// `glob` has no shell; `case` and `[[ == ]]` don't take them.
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// `shopt -s dotglob`: a leading `.` stops being special, so `*`
    /// matches `.hidden` too.
    pub dotglob: bool,
    /// `shopt -s nocaseglob`: match without regard to case.
    pub nocaseglob: bool,
    /// `shopt -s globstar`: `**` as a whole component stands for any
    /// number of directories.
    pub globstar: bool,
}

/// Expands `pattern` against the filesystem, a relative one against
/// `root`, the shell's cwd (which differs from the process's for an
/// in-process pipeline stage). Matches stay relative, as written. `None`
/// means not a pattern; `Some(vec![])` is a pattern matching nothing,
/// which the caller handles (as is, `nullglob`, `failglob`). Every
/// component may be a pattern; with `globstar`, `**` is any number of
/// directories, none included.
pub fn expand(pattern: &str, options: Options, root: &std::path::Path) -> Option<Vec<String>> {
    if !has_meta(pattern) {
        return None;
    }
    let absolute = pattern.starts_with('/');
    let trailing_slash = pattern.len() > 1 && pattern.ends_with('/');
    let components: Vec<&str> = pattern.trim_matches('/').split('/').filter(|c| !c.is_empty()).collect();
    let mut candidates = vec![if absolute { "/".to_string() } else { String::new() }];

    for (i, component) in components.iter().enumerate() {
        let last = i + 1 == components.len();
        let mut next = Vec::new();
        for base in &candidates {
            match *component {
                // `**` is any number of directories, none included:
                // `a/**/x` finds `a/x` and `a/b/c/x`. Last, it also lists
                // the files, so `**` alone lists everything.
                "**" if options.globstar => {
                    for path in descend(base, !last, root) {
                        next.push(match (path.is_empty(), last) {
                            // No directories. At the end that's the base,
                            // written with the pattern's separator (`a/**`
                            // lists `a/`); in the middle the separator
                            // belongs to what follows, or `a/**/x` would
                            // give `a//x`.
                            (true, true) => join(base, ""),
                            (true, false) => base.clone(),
                            _ => join(base, &path),
                        });
                    }
                }
                _ => {
                    for name in read_names(base, component, options, root) {
                        next.push(join(base, &name));
                    }
                }
            }
        }
        candidates = next;
        crate::sort::sort(&mut candidates);
        candidates.dedup();
    }

    let mut found: Vec<String> = candidates
        .into_iter()
        .filter(|p| !p.is_empty())
        .filter(|p| !trailing_slash || std::fs::metadata(root.join(p)).is_ok_and(|m| m.is_dir()))
        .map(|p| match trailing_slash {
            true => format!("{p}/"),
            false => p,
        })
        .collect();
    crate::sort::sort(&mut found);
    found.dedup();
    Some(found)
}

// The names in `base` that `component` matches, or the component
// itself when it's not a pattern (then only its existence matters).
fn read_names(base: &str, component: &str, options: Options, root: &std::path::Path) -> Vec<String> {
    let dir = root.join(base);
    if !has_meta(component) {
        let literal = unescape(component);
        let path = join(base, &literal);
        let path = if path.is_empty() { ".".to_string() } else { path };
        return match std::fs::symlink_metadata(&path).is_ok() {
            true => vec![literal],
            false => Vec::new(),
        };
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| {
            // A leading `.` must be named, unless `dotglob`. `.` and `..`
            // are never produced, as `globskipdots` defaults and bash 5.2
            // made unconditional.
            if name.starts_with('.') && !options.dotglob && !component.starts_with('.') {
                return false;
            }
            if name == "." || name == ".." {
                return false;
            }
            match options.nocaseglob {
                true => matches(&component.to_lowercase(), &name.to_lowercase()),
                false => matches(component, name),
            }
        })
        .collect()
}

// What `**` stands for at `base`: the empty path and every path under
// it. Symlinks are named but not descended, as in bash, which avoids
// cycles. `dirs_only` when something follows `**`, since it's looked up
// inside.
fn descend(base: &str, dirs_only: bool, root: &std::path::Path) -> Vec<String> {
    let base = root.join(base);
    let mut out = vec![String::new()];
    let mut queue = vec![String::new()];
    while let Some(prefix) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(base.join(&prefix)) else { continue };
        let mut names: Vec<String> = entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
        crate::sort::sort(&mut names);
        for name in names {
            if name.starts_with('.') {
                continue;
            }
            let path = join(&prefix, &name);
            let real_dir = std::fs::symlink_metadata(base.join(&path)).is_ok_and(|m| m.is_dir());
            if real_dir {
                queue.push(path.clone());
            }
            if !dirs_only || real_dir {
                out.push(path);
            }
        }
    }
    out
}

fn join(base: &str, rest: &str) -> String {
    match (base, rest) {
        ("", _) => rest.to_string(),
        (_, "") => format!("{base}/"),
        ("/", _) => format!("/{rest}"),
        _ => format!("{base}/{rest}"),
    }
}

// A component without metacharacters may still carry `escape`'s
// backslashes; the name on disk is what they escaped.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A quoted chunk can land inside a bracket expression, so `]`, `^` and
    // `-` must be escapable there.
    #[test]
    fn a_bracket_expression_honours_backslash_escapes() {
        assert!(matches("[\\^]", "^"));
        assert!(!matches("[\\^]", "a"));
        assert!(matches("[\\]]", "]"));
        assert!(!matches("[\\]]", "a"));
        // `[a\-z]` is a, `-` and z, not a range.
        assert!(matches("[a\\-z]", "-"));
        assert!(matches("[a\\-z]", "a"));
        assert!(matches("[a\\-z]", "z"));
        assert!(!matches("[a\\-z]", "q"));
        // Unescaped, it's the range.
        assert!(matches("[a-z]", "q"));
        // A negated class still negates and escapes.
        assert!(!matches("[^\\^]", "^"));
        assert!(matches("[^\\^]", "a"));
    }

    // Everything `escape` emits is inert, the three bracket-only
    // characters included.
    #[test]
    fn escaped_text_matches_only_itself() {
        for s in ["a*b", "a?b", "a[b", "a]b", "a^b", "a-b", "a\\b", "a@(b", "[^a-z]"] {
            assert!(matches(&escape(s), s), "{s} should match its own escaping");
        }
        assert!(!matches(&escape("a*b"), "axb"));
    }

    // An escaped `]` isn't the class's end for the width either.
    #[test]
    fn escapes_do_not_confuse_the_fixed_width_scan() {
        assert_eq!(fixed_width("[\\]]"), Some(1));
        assert_eq!(fixed_width("[\\^]"), Some(1));
        assert_eq!(fixed_width("a[\\]]b"), Some(3));
        assert_eq!(fixed_width("\\*"), Some(1));
        assert_eq!(fixed_width("[[:space:]]"), Some(1));
        assert_eq!(fixed_width("*"), None);
    }

    // Each `true` costs a directory scan where it's called, so the `false`
    // cases matter.
    #[test]
    fn a_bracket_is_a_pattern_only_when_it_closes() {
        assert!(has_meta("[ab]"), "a real class");
        assert!(has_meta("[!a]"), "a negated class");
        assert!(has_meta("[^a]"), "the other negation");
        assert!(has_meta("[]]"), "a class whose only member is `]`");
        assert!(has_meta("x[]]y"), "the same, mid-word");
        assert!(has_meta("a[b]c"), "mid-word");

        // `[` and `]`, the words of `[ $x -lt 1 ]`, aren't patterns.
        assert!(!has_meta("["), "a lone open bracket");
        assert!(!has_meta("]"), "a lone close bracket");
        assert!(!has_meta("[abc"), "never closed");
        assert!(!has_meta("[!"), "a negation with nothing after it");
        assert!(!has_meta("[]"), "`]` right after `[` is the literal, so this never closes");
        assert!(!has_meta("plain"), "no metacharacters at all");
        assert!(!has_meta("\\[a]"), "escaped, so not a pattern");
    }

    // `has_meta` and `match_class` agree on where a class ends.
    #[test]
    fn has_meta_agrees_with_the_matcher_about_what_is_a_class() {
        for pat in ["[ab]", "[]]", "[!a]", "[", "]", "[abc", "[]"] {
            let scanned = has_meta(pat);
            let matched = match_class(pat.as_bytes(), Some(b'a')).is_some();
            assert_eq!(scanned, matched, "{pat:?}: has_meta says {scanned}, match_class says {matched}");
        }
    }

    #[test]
    fn an_unterminated_bracket_matches_itself() {
        assert!(matches("[", "["));
        assert!(matches("]", "]"));
        assert!(matches("a[b", "a[b"));
        assert!(!matches("[", "a"));
    }
}
