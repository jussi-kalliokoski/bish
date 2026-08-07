// Command history: persisted to ~/.bish_history (plain text, one entry per
// file line -- a stored entry's own newlines/backslashes are escaped so a
// multi-line history entry, e.g. a whole recalled for-loop, round-trips
// through the file as a single line rather than fragmenting on reload).
// editor.rs is the only consumer of the search methods, driving fish-style
// up/down: prefix-filtered rather than plain chronological recall.

use std::io::Write;
use std::path::PathBuf;

pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
}

impl History {
    pub fn load() -> History {
        let path = history_path();
        let mut entries = Vec::new();
        if let Some(p) = &path {
            if let Ok(content) = std::fs::read_to_string(p) {
                entries.extend(content.lines().map(unescape));
            }
        }
        History { path, entries }
    }

    // Records a submitted line (regardless of whether it later succeeds or
    // fails -- bash and fish both do this). Skipped if blank or identical
    // to the immediately preceding entry; this is a lighter touch than
    // fish's full re-dedup-and-move-to-front, but keeps the common
    // "pressed enter on the same command twice" case from cluttering
    // history, which is what this is mainly for.
    pub fn record(&mut self, entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        if self.entries.last().map(|s| s.as_str()) == Some(entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if let Some(p) = &self.path {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{}", escape(entry));
            }
        }
    }

    // Nearest entry at or before index `from` (exclusive; None means
    // "start from the newest") whose text starts with `prefix`, searching
    // toward older entries. Used for Up.
    pub fn search_backward(&self, prefix: &str, from: Option<usize>) -> Option<(usize, &str)> {
        let end = from.unwrap_or(self.entries.len());
        self.entries[..end].iter().enumerate().rev().find(|(_, e)| e.starts_with(prefix)).map(|(i, e)| (i, e.as_str()))
    }

    // Nearest entry after index `from` whose text starts with `prefix`,
    // searching toward newer entries. Used for Down.
    pub fn search_forward(&self, prefix: &str, from: usize) -> Option<(usize, &str)> {
        self.entries
            .iter()
            .enumerate()
            .skip(from + 1)
            .find(|(_, e)| e.starts_with(prefix))
            .map(|(i, e)| (i, e.as_str()))
    }
}

fn history_path() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".bish_history"))
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}
