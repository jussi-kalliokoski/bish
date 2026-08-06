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
fn match_class(pat: &[u8], c: Option<u8>) -> Option<(bool, &[u8])> {
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

pub fn has_meta(s: &str) -> bool {
    s.chars().any(|c| c == '*' || c == '?' || c == '[')
        || ["@(", "!(", "+("].iter().any(|p| s.contains(p))
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
pub fn expand(pattern: &str) -> Option<Vec<String>> {
    if !has_meta(pattern) {
        return None;
    }
    let (dir, base_pattern, prefix) = match pattern.rfind('/') {
        Some(idx) => (pattern[..idx].to_string(), &pattern[idx + 1..], format!("{}/", &pattern[..idx])),
        None => (".".to_string(), pattern, String::new()),
    };
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut found: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| {
            if name.starts_with('.') && !base_pattern.starts_with('.') {
                return false;
            }
            matches(base_pattern, name)
        })
        .map(|name| format!("{}{}", prefix, name))
        .collect();
    if found.is_empty() {
        return None;
    }
    found.sort();
    Some(found)
}
