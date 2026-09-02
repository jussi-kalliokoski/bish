// Shell-style filename glob matching: * ? [abc] [a-z] [!abc]/[^abc], plus
// extglob patterns @(...) !(...) +(...) *(...) ?(...). Real bash gates
// extglob behind `shopt -s extglob`; bish doesn't track shopt state yet, so
// these are recognized unconditionally -- a strict superset of default
// bash behavior that only diverges for scripts that use a literal pattern
// like `!(foo)` as ordinary text without ever enabling extglob, which is
// vanishingly rare in practice.
// Shared by pathname expansion, `case` patterns, and `[[ ]]`'s `==`/`!=`.

pub fn matches(pattern: &str, text: &str) -> bool {
    match_here(pattern.as_bytes(), text.as_bytes())
}

/// `matches`, optionally folding case -- what `shopt -s nocasematch`
/// asks of `case` and of `[[ == ]]`. Folding both sides is the same
/// thing `nocaseglob` does for pathnames (see `read_names`).
pub fn matches_with_case(pattern: &str, text: &str, fold_case: bool) -> bool {
    match fold_case {
        true => matches(&pattern.to_lowercase(), &text.to_lowercase()),
        false => matches(pattern, text),
    }
}

/// The same, but `*` and `?` never cross a `/` -- pathname semantics,
/// what C's fnmatch spells `FNM_PATHNAME`.
///
/// This is the rule pathname expansion itself follows, and the one
/// `GLOBIGNORE` is matched under: `*.o` drops `a.o` and leaves
/// `sub/x.o` alone, because the pattern names one path component and
/// the candidate has two. Confirmed against real bash, which was the
/// opposite of what I first assumed.
pub fn matches_path(pattern: &str, text: &str) -> bool {
    let (pat, txt): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), text.split('/').collect());
    pat.len() == txt.len() && pat.iter().zip(txt.iter()).all(|(p, t)| matches(p, t))
}

fn match_here(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    // Backslash-escape: `\c` matches a literal `c`, whatever `c` is (even
    // one of the metacharacters below). Needed so a pattern built from a
    // partly-quoted word (see escape() and expand_glob_pattern in exec.rs)
    // can represent "this piece is literal text" for the quoted chunks
    // while unquoted chunks stay real glob syntax, mirroring how the same
    // word is already handled for the `=~` regex operand.
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
// pipe-separated alternatives, honoring nested groups. Returns
// (alternatives, the group text including its parens, the pattern text
// after the closing paren), or None if unterminated (no matching ')').
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

// pat[0] == b'['. Returns (did `c` match the class, remaining pattern after
// the closing ']'), or None if the bracket expression is malformed (no
// closing ']'), in which case '[' should be treated as a literal char.
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
        if k + 2 < class.len() && class[k + 1] == b'-' {
            if c >= class[k] && c <= class[k + 2] {
                matched = true;
            }
            k += 3;
        } else {
            if class[k] == c {
                matched = true;
            }
            k += 1;
        }
    }
    if negate {
        matched = !matched;
    }
    Some((matched, &pat[j + 1..]))
}

/// Whether `s` is a pattern at all -- i.e. whether it has a
/// metacharacter that is not escaped.
///
/// The escaping matters and used to be ignored. A word's quoted parts
/// reach here through `escape`, so `printf '[%s]'` arrives as
/// `\[%s\]`: it has a `[` in it, but not one that means anything, and
/// reading it as a pattern makes the word vanish under `nullglob`.
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

/// Where the bracket expression opened at `pat[0]` closes, if it closes
/// at all -- the same scan `match_class` does, and it has to agree with
/// it: a `[` with no `]` after it is not a pattern, it is the character
/// `[`.
///
/// That agreement is worth a measurement. `has_meta` used to answer
/// "yes, a pattern" for any `[`, and a word that *might* be a pattern
/// gets a filesystem scan. So `[ $n -lt 300 ]` -- two words, `[` and
/// `]` -- read the whole current directory twice per iteration:
///
///     cd /tmp                                  # ~8,000 entries
///     while [ $n -lt 300 ]; do n=$((n+1)); done
///     bash 3-13ms, bish 5.7-8.3s
///
/// The result was never wrong, since the scan matched nothing and the
/// word fell back to its own text. It just cost 19-27ms each time.
fn closing_bracket(pat: &[u8]) -> Option<usize> {
    let mut i = 1;
    if i < pat.len() && (pat[i] == b'!' || pat[i] == b'^') {
        i += 1;
    }
    // A `]` in the first position is the literal character, not the
    // terminator -- `[]]` matches a single `]`.
    if i < pat.len() && pat[i] == b']' {
        i += 1;
    }
    while i < pat.len() && pat[i] != b']' {
        i += 1;
    }
    (i < pat.len()).then_some(i)
}

// Escapes every character match_here treats specially, so the result --
// spliced into a larger pattern -- matches only the literal input text.
// Used by expand_glob_pattern (exec.rs) to render a word's quoted/expanded
// chunks as inert literal text within an otherwise-real glob pattern built
// from the word's unquoted chunks, the same per-chunk approach already
// used for `[[ =~ ]]`'s regex operand (see regex::escape).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "*?[\\@!+(".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// Expands a glob pattern against the filesystem. Only the final path
// component may contain metacharacters -- `dir/*.txt` works, but metachars
// in an intermediate directory segment don't. Returns None if there were no
// matches (caller should fall back to the literal pattern text, bash's
// default nullglob-off behavior).
/// The `shopt` settings that change what a pattern matches.
///
/// Passed in rather than read here because `glob` has no shell to ask,
/// and because `case` patterns and `[[ == ]]` deliberately do not take
/// them -- `nocaseglob` and `dotglob` are about *pathnames*.
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

/// Expands a glob pattern against the filesystem.
///
/// `None` means "this is not a pattern at all" -- no unescaped
/// metacharacters -- and is a different answer from `Some(vec![])`,
/// which means it is a pattern that matched nothing. Only the caller
/// can decide what the second one means: left alone it is bash's
/// default, `nullglob` drops the word, and `failglob` makes it an
/// error.
///
/// Every path component may be a pattern, and `**` (with `globstar`)
/// stands for any number of directories, none included.
pub fn expand(pattern: &str, options: Options) -> Option<Vec<String>> {
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
                // `**` is any number of directories, including none --
                // so `a/**/x` finds `a/x` as well as `a/b/c/x`. As the
                // last component it also names the files it passed, so
                // `**` on its own lists everything.
                "**" if options.globstar => {
                    for path in descend(base, !last) {
                        next.push(match (path.is_empty(), last) {
                            // No directories at all. On the end that is
                            // the base itself, which bash writes with
                            // the separator the pattern had -- `a/**`
                            // lists `a/`. In the middle the separator
                            // belongs to whatever comes next, or
                            // `a/**/x` would be `a//x`.
                            (true, true) => join(base, ""),
                            (true, false) => base.clone(),
                            _ => join(base, &path),
                        });
                    }
                }
                _ => {
                    for name in read_names(base, component, options) {
                        next.push(join(base, &name));
                    }
                }
            }
        }
        candidates = next;
        candidates.sort();
        candidates.dedup();
    }

    let mut found: Vec<String> = candidates
        .into_iter()
        .filter(|p| !p.is_empty())
        .filter(|p| !trailing_slash || std::fs::metadata(p).is_ok_and(|m| m.is_dir()))
        .map(|p| match trailing_slash {
            true => format!("{p}/"),
            false => p,
        })
        .collect();
    found.sort();
    found.dedup();
    Some(found)
}

// One component's worth of matching: the names in `base` that
// `component` matches, or the component itself when it is not a
// pattern (in which case only its existence matters).
fn read_names(base: &str, component: &str, options: Options) -> Vec<String> {
    let dir = if base.is_empty() { "." } else { base };
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
            // A leading `.` has to be asked for by name -- unless
            // `dotglob` says otherwise. `.` and `..` are never produced
            // either way, which is what `globskipdots` defaults to and
            // bash 5.2 made unconditional.
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

// What `**` stands for at `base`: the empty path (no directories at
// all), and every path under it. Symlinks are named but never
// descended into, which is bash's rule and the only thing between this
// and a cycle -- `*/x` still goes through one, because that is the
// component's own single step rather than this walk.
//
// `dirs_only` for a `**` that has something after it: whatever comes
// next has to be looked up *inside*, and a file is not somewhere to
// look.
fn descend(base: &str, dirs_only: bool) -> Vec<String> {
    let root = if base.is_empty() { "." } else { base };
    let mut out = vec![String::new()];
    let mut queue = vec![String::new()];
    while let Some(prefix) = queue.pop() {
        let dir = join(root, &prefix);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = entries.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect();
        names.sort();
        for name in names {
            if name.starts_with('.') {
                continue;
            }
            let path = join(&prefix, &name);
            let real_dir = std::fs::symlink_metadata(join(root, &path)).is_ok_and(|m| m.is_dir());
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

// A component with no metacharacters may still carry backslashes from
// `escape`; the name on disk is what they were escaping.
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

    // The check that keeps `[ ... ]` off the filesystem. Every `true`
    // here costs a directory scan at the call site, so the interesting
    // assertions are the `false` ones.
    #[test]
    fn a_bracket_is_a_pattern_only_when_it_closes() {
        assert!(has_meta("[ab]"), "a real class");
        assert!(has_meta("[!a]"), "a negated class");
        assert!(has_meta("[^a]"), "the other negation");
        assert!(has_meta("[]]"), "a class whose only member is `]`");
        assert!(has_meta("x[]]y"), "the same, mid-word");
        assert!(has_meta("a[b]c"), "mid-word");

        // `[` and `]` are the two words of `[ $x -lt 1 ]`, and reading
        // either as a pattern made every test in a loop read the whole
        // directory. Neither is one.
        assert!(!has_meta("["), "a lone open bracket");
        assert!(!has_meta("]"), "a lone close bracket");
        assert!(!has_meta("[abc"), "never closed");
        assert!(!has_meta("[!"), "a negation with nothing after it");
        assert!(!has_meta("[]"), "`]` right after `[` is the literal, so this never closes");
        assert!(!has_meta("plain"), "no metacharacters at all");
        assert!(!has_meta("\\[a]"), "escaped, so not a pattern");
    }

    // has_meta and match_class have to agree about where a class ends,
    // or a word gets scanned for as a pattern and then matched as a
    // literal (or the reverse).
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
