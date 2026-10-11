// The hosts this machine knows, from `~/.ssh/config`,
// `~/.ssh/known_hosts` and `/etc/hosts`: what can follow `ssh`, as one
// deduplicated list. Each reader is a pure function of a string, so
// hashed entries, `[host]:port` and `Include` are tested without a
// filesystem.
//
// Patterns are never offered: `Host *.internal` or a wildcard
// `known_hosts` entry is a rule, not a host you can reach.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every host worth offering, deduplicated, in a stable order. Silent
/// about what it can't read: a missing `~/.ssh/config` is normal.
pub fn known() -> Vec<String> {
    let mut out = BTreeSet::new();
    if let Some(home) = home() {
        out.extend(from_ssh_config_file(&home.join(".ssh/config"), 0));
        out.extend(from_known_hosts(&read(&home.join(".ssh/known_hosts"))));
    }
    out.extend(from_ssh_config_file(Path::new("/etc/ssh/ssh_config"), 0));
    out.extend(from_known_hosts(&read(Path::new("/etc/ssh/ssh_known_hosts"))));
    out.extend(from_etc_hosts(&read(Path::new("/etc/hosts"))));
    out.into_iter().collect()
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// How deep `Include` chains are followed. ssh documents no limit; this
// stops a config that includes itself.
const MAX_INCLUDE_DEPTH: usize = 8;

fn from_ssh_config_file(path: &Path, depth: usize) -> Vec<String> {
    let text = read(path);
    let mut out = from_ssh_config(&text);
    if depth >= MAX_INCLUDE_DEPTH {
        return out;
    }
    // `Include` is followed: many configs are just `Include config.d/*`.
    for pattern in ssh_config_includes(&text) {
        for included in resolve_include(&pattern, path) {
            out.extend(from_ssh_config_file(&included, depth + 1));
        }
    }
    out
}

// An `Include` path is relative to `~/.ssh` for a user config and
// `/etc/ssh` for a system one, may start with `~`, and may glob.
fn resolve_include(pattern: &str, from: &Path) -> Vec<PathBuf> {
    let expanded = match pattern.strip_prefix("~/") {
        Some(rest) => match home() {
            Some(home) => home.join(rest).to_string_lossy().into_owned(),
            None => return Vec::new(),
        },
        None if pattern.starts_with('/') => pattern.to_string(),
        None => match from.parent() {
            Some(dir) => dir.join(pattern).to_string_lossy().into_owned(),
            None => return Vec::new(),
        },
    };
    match crate::glob::expand(&expanded, crate::glob::Options::default(), std::path::Path::new(".")) {
        Some(paths) => paths.into_iter().map(PathBuf::from).collect(),
        // No matches or no metacharacters: the literal path, which may not
        // exist.
        None => vec![PathBuf::from(expanded)],
    }
}

/// `Host` aliases and `HostName` values from an ssh config: the alias is
/// what the config gives you, and the real name is what you type on a
/// machine without that config.
pub fn from_ssh_config(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (keyword, value) in ssh_config_lines(text) {
        match keyword.as_str() {
            // `Host` takes a list of patterns; `HostName` one name, which
            // may hold `%h`-style tokens.
            "host" => out.extend(value.split_whitespace().filter(|w| is_plain_host(w)).map(str::to_string)),
            "hostname" => out.extend(value.split_whitespace().take(1).filter(|w| is_plain_host(w)).map(str::to_string)),
            _ => {}
        }
    }
    out
}

fn ssh_config_includes(text: &str) -> Vec<String> {
    ssh_config_lines(text)
        .into_iter()
        .filter(|(keyword, _)| keyword == "include")
        .flat_map(|(_, value)| value.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

// `Keyword value` or `Keyword=value`, maybe indented. Keywords are
// case-insensitive, so they come back folded.
fn ssh_config_lines(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = match line.find(['=', ' ', '\t']) {
            Some(at) => (&line[..at], line[at + 1..].trim_start_matches(['=', ' ', '\t'])),
            None => (line, ""),
        };
        out.push((keyword.to_ascii_lowercase(), value.trim().to_string()));
    }
    out
}

/// The host names in a `known_hosts` file, skipping hashed entries
/// (`|1|...`), whose names can't be recovered, and `@revoked` keys, whose
/// hosts shouldn't be one Tab away.
pub fn from_known_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut first = fields.next().unwrap_or_default();
        if first == "@revoked" {
            continue;
        }
        if first.starts_with('@') {
            // `@cert-authority` and other markers: the host list is the
            // next field.
            first = fields.next().unwrap_or_default();
        }
        if first.starts_with('|') {
            continue;
        }
        out.extend(first.split(',').filter_map(known_host_name));
    }
    out
}

// One entry from a known_hosts host list. `[host]:port` is a
// non-default port, dropped: callers want a name.
fn known_host_name(entry: &str) -> Option<String> {
    let name = match entry.strip_prefix('[') {
        Some(rest) => rest.split(']').next()?,
        None => entry,
    };
    is_plain_host(name).then(|| name.to_string())
}

/// The names in an `/etc/hosts` file: every field after the address.
pub fn from_etc_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        out.extend(line.split_whitespace().skip(1).filter(|w| is_plain_host(w)).map(str::to_string));
    }
    out
}

// Whether this is a name you could type, not a pattern or a negation.
fn is_plain_host(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('!')
        && !word.contains(['*', '?', '%'])
        && word.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_offers_aliases_and_the_names_behind_them() {
        let text = "\
Host web prod
    HostName web-1.example.com
    User deploy

Host db
    HostName 10.0.0.5
";
        assert_eq!(from_ssh_config(text), vec!["web", "prod", "web-1.example.com", "db", "10.0.0.5"]);
    }

    #[test]
    fn ssh_config_accepts_equals_and_any_casing() {
        assert_eq!(from_ssh_config("host=web\nHOSTNAME = web.example.com\n"), vec!["web", "web.example.com"]);
    }

    #[test]
    fn ssh_config_never_offers_a_pattern_or_a_negation() {
        let text = "Host *\n  ForwardAgent yes\nHost *.internal !secret real\n  HostName %h.example.com\n";
        assert_eq!(from_ssh_config(text), vec!["real"]);
    }

    #[test]
    fn ssh_config_comments_and_blank_lines_are_skipped() {
        assert_eq!(from_ssh_config("# Host commented\n\n   # indented\nHost real\n"), vec!["real"]);
    }

    #[test]
    fn known_hosts_takes_every_name_in_the_list() {
        let text = "web.example.com,10.0.0.4 ssh-rsa AAAAB3Nz comment\ndb.example.com ssh-ed25519 AAAAC3Nz\n";
        assert_eq!(from_known_hosts(text), vec!["web.example.com", "10.0.0.4", "db.example.com"]);
    }

    #[test]
    fn known_hosts_understands_bracketed_ports_and_markers() {
        assert_eq!(from_known_hosts("[web.example.com]:2222 ssh-rsa AAAA\n"), vec!["web.example.com"]);
        assert_eq!(from_known_hosts("@cert-authority *.example.com ssh-rsa AAAA\n"), Vec::<String>::new());
        assert_eq!(from_known_hosts("@cert-authority real.example.com ssh-rsa AAAA\n"), vec!["real.example.com"]);
    }

    #[test]
    fn known_hosts_skips_hashed_and_revoked_entries() {
        assert_eq!(from_known_hosts("|1|F1E1+abc=|9xyz= ssh-rsa AAAA\n"), Vec::<String>::new());
        assert_eq!(from_known_hosts("@revoked bad.example.com ssh-rsa AAAA\n"), Vec::<String>::new());
    }

    #[test]
    fn etc_hosts_takes_the_names_and_not_the_address() {
        let text = "127.0.0.1\tlocalhost\n192.168.1.10  nas nas.local  # the box\n::1 ip6-localhost\n";
        assert_eq!(from_etc_hosts(text), vec!["localhost", "nas", "nas.local", "ip6-localhost"]);
    }

    #[test]
    fn etc_hosts_ignores_comments_entirely() {
        assert_eq!(from_etc_hosts("# 1.2.3.4 commented\n"), Vec::<String>::new());
    }

    #[test]
    fn nothing_typeable_panics() {
        for text in ["", "\n", "#", "Host", "Host ", "@revoked", "|", "[", "[]:", ",,,", "=", " = ", "1.2.3.4"] {
            from_ssh_config(text);
            from_known_hosts(text);
            from_etc_hosts(text);
        }
    }

    // Against real files, since resolving an `Include` is about paths.
    #[test]
    fn an_included_config_is_read_too() {
        let dir = std::env::temp_dir().join(format!("bish-hosts-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(dir.join("config"), "Include config.d/*\nHost direct\n").unwrap();
        std::fs::write(dir.join("config.d/work"), "Host work\n  HostName work.example.com\n").unwrap();
        let hosts = from_ssh_config_file(&dir.join("config"), 0);
        assert!(hosts.contains(&"direct".to_string()));
        assert!(hosts.contains(&"work".to_string()), "got {hosts:?}");
        assert!(hosts.contains(&"work.example.com".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_self_including_config_terminates() {
        let dir = std::env::temp_dir().join(format!("bish-hosts-cycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config"), "Include config\nHost loop\n").unwrap();
        let hosts = from_ssh_config_file(&dir.join("config"), 0);
        assert!(hosts.contains(&"loop".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
