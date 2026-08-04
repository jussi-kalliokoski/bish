// Shell-style filename glob matching: * ? [abc] [a-z] [!abc]/[^abc].
// Shared by pathname expansion, `case` patterns, and `[[ ]]`'s `==`/`!=`.

pub fn matches(pattern: &str, text: &str) -> bool {
    match_here(pattern.as_bytes(), text.as_bytes())
}

fn match_here(pat: &[u8], text: &[u8]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
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
