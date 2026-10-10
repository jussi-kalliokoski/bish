// `.gitignore` pattern matching, per gitignore(5).
//
// Built on `glob::matches`: a gitignore pattern is a path pattern whose
// `*` and `?` don't cross `/`, which follows from matching segment by
// segment, so classes and escapes mean what they do elsewhere in bish.
// `glob::matches` also takes extglob `@(a|b)`, which git doesn't. `**`
// segments are handled here, being meaningful only between slashes.
//
// No directory walking or file reading, except `Stack::for_directory`.
// git's rule that a file under an excluded directory can't be re-included
// comes from not descending; `matched_path_or_any_parents` implements it
// for callers with a path but no walk.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// What patterns say about one path. `Whitelisted` (a `!` pattern won)
/// differs from no match: a whitelist in a deeper file stops a shallower
/// file's exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    None,
    Ignored,
    Whitelisted,
}

impl Match {
    pub fn is_ignored(self) -> bool {
        self == Match::Ignored
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    negated: bool,
    /// Set by a trailing `/`: the pattern matches directories only.
    dir_only: bool,
    /// Set by a `/` anywhere but the end: the pattern is relative to the
    /// ignore file's own directory instead of matching at any depth.
    anchored: bool,
    segments: Vec<Segment>,
    /// The line this came from, so a caller can say which rule excluded a
    /// file, as `git check-ignore -v` does.
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A `**` segment: any run of directories, zero or more in the middle
    /// (`a/**/b` matches `a/b`) but one or more at the end (`abc/**` is
    /// everything inside `abc`).
    AnyDirs,
    /// Anything else, matched against one path segment by `glob`.
    Glob(String),
}

impl Pattern {
    /// One `.gitignore` line, or `None` for a blank line, a `#` comment, or
    /// a lone `/`.
    pub fn parse(line: &str) -> Option<Pattern> {
        let source = line.to_string();
        // Trailing spaces are dropped unless escaped: `foo\ ` ends in a
        // space.
        let line = strip_trailing_spaces(line);
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (negated, line) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            // `\#` and `\!` start a pattern with that character. The
            // backslash is dropped here rather than left for `glob` to
            // interpret.
            None => (false, line.strip_prefix('\\').filter(|r| r.starts_with(['#', '!'])).unwrap_or(line)),
        };
        let (dir_only, line) = match line.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        // A `/` anywhere but the end anchors the pattern; a leading one isn't
        // part of it.
        let anchored = line.contains('/');
        let line = line.strip_prefix('/').unwrap_or(line);
        let segments: Vec<Segment> =
            line.split('/').filter(|s| !s.is_empty()).map(|s| if s == "**" { Segment::AnyDirs } else { Segment::Glob(s.to_string()) }).collect();
        if segments.is_empty() {
            return None;
        }
        Some(Pattern { negated, dir_only, anchored, segments, source })
    }

    pub fn negated(&self) -> bool {
        self.negated
    }

    pub fn dir_only(&self) -> bool {
        self.dir_only
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether this matches `path`, `/`-separated and relative to the ignore
    /// file's directory.
    pub fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            return false;
        }
        if self.anchored {
            return match_segments(&self.segments, &segments);
        }
        // Unanchored, `foo` is a name, matching at any depth: try every
        // starting segment.
        (0..segments.len()).any(|i| match_segments(&self.segments, &segments[i..]))
    }
}

fn match_segments(pattern: &[Segment], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(Segment::AnyDirs) => {
            let rest = &pattern[1..];
            // A trailing `**` must consume something: `abc/**` doesn't
            // include `abc`.
            let least = if rest.is_empty() { 1 } else { 0 };
            (least..=path.len()).any(|i| match_segments(rest, &path[i..]))
        }
        Some(Segment::Glob(g)) => !path.is_empty() && crate::glob::matches(g, path[0]) && match_segments(&pattern[1..], &path[1..]),
    }
}

/// One ignore file's worth of patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ignore {
    patterns: Vec<Pattern>,
}

impl Ignore {
    pub fn parse(text: &str) -> Ignore {
        Ignore { patterns: text.lines().filter_map(Pattern::parse).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// The last matching pattern wins, which is why a `!` line comes after
    /// the rule it undoes.
    pub fn matched(&self, path: &str, is_dir: bool) -> Match {
        self.matched_pattern(path, is_dir).map_or(Match::None, |p| if p.negated { Match::Whitelisted } else { Match::Ignored })
    }

    /// The same, returning the deciding rule, as for `git check-ignore -v`.
    pub fn matched_pattern(&self, path: &str, is_dir: bool) -> Option<&Pattern> {
        self.patterns.iter().rev().find(|p| p.matches(path, is_dir))
    }

    /// For a caller with a path but no walk. git never descends into an
    /// excluded directory, so a `!` line can't rescue anything under it:
    /// checking ancestors top down and stopping at the first excluded one is
    /// that rule.
    pub fn matched_path_or_any_parents(&self, path: &str, is_dir: bool) -> Match {
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        for i in 1..segments.len() {
            if self.matched(&segments[..i].join("/"), true) == Match::Ignored {
                return Match::Ignored;
            }
        }
        self.matched(path, is_dir)
    }
}

/// Several ignore files, each rooted where it was found. The deepest
/// applicable file decides, as a subdirectory's `.gitignore` overrides
/// the top one.
#[derive(Debug, Clone, Default)]
pub struct Stack {
    files: Vec<(PathBuf, Ignore)>,
}

impl Stack {
    pub fn new() -> Stack {
        Stack::default()
    }

    /// `base` is where the file was found; its patterns are relative to it.
    pub fn push(&mut self, base: impl Into<PathBuf>, ignore: Ignore) {
        self.files.push((base.into(), ignore));
    }

    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|(_, i)| i.is_empty())
    }

    /// Every ignore file that applies inside `dir`, in git's order. Empty
    /// outside a repository: a stray `.gitignore` there isn't a rule.
    /// Sources, shallowest first (the deepest wins): `core.excludesFile`
    /// and `.git/info/exclude`, both based at the root, then each
    /// `.gitignore` from the root down to `dir`.
    pub fn for_directory(dir: &Path) -> Stack {
        let mut stack = Stack::new();
        let Some(root) = repo_root(dir) else { return stack };
        if let Some(excludes) = excludes_file(&root) {
            stack.push_file(&root, &excludes);
        }
        stack.push_file(&root, &root.join(".git/info/exclude"));
        // Root first, then down. `matched` orders by depth itself; this order
        // just reads well.
        let mut here = PathBuf::new();
        for component in dir.strip_prefix(&root).unwrap_or(Path::new("")).components() {
            stack.push_file(&root.join(&here), &root.join(&here).join(".gitignore"));
            here.push(component);
        }
        stack.push_file(dir, &dir.join(".gitignore"));
        stack
    }

    // Reads one ignore file if present; a missing one is normal.
    fn push_file(&mut self, base: &Path, file: &Path) {
        if let Ok(text) = std::fs::read_to_string(file) {
            let ignore = Ignore::parse(&text);
            if !ignore.is_empty() {
                self.push(base, ignore);
            }
        }
    }

    /// Deepest base first, and the first file with anything to say decides,
    /// `Whitelisted` included, which is how a nested `!` overrides an
    /// exclusion from above.
    pub fn matched(&self, path: &Path, is_dir: bool) -> Match {
        let mut applicable: Vec<&(PathBuf, Ignore)> = self.files.iter().filter(|(base, _)| path.starts_with(base)).collect();
        crate::sort::sort_by_key(&mut applicable, |(base, _)| std::cmp::Reverse(base.components().count()));
        for (base, ignore) in applicable {
            let Ok(relative) = path.strip_prefix(base) else { continue };
            let relative = relative.to_string_lossy();
            match ignore.matched_path_or_any_parents(&relative, is_dir) {
                Match::None => continue,
                decided => return decided,
            }
        }
        Match::None
    }
}

/// The nearest ancestor of `dir` holding `.git`, a directory or, in a
/// worktree or submodule, a file; so only the name is checked.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let mut here = dir;
    loop {
        if here.join(".git").exists() {
            return Some(here.to_path_buf());
        }
        here = here.parent()?;
    }
}

// `core.excludesFile` from the repository config, then the user's, then
// git's default location.
fn excludes_file(root: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".config")));
    let configs = [Some(root.join(".git/config")), xdg.as_ref().map(|x| x.join("git/config")), home.as_ref().map(|h| h.join(".gitconfig"))];
    for config in configs.into_iter().flatten() {
        if let Some(value) = git_config_value(&config, "core", "excludesfile") {
            return Some(expand_tilde(&value, home.as_deref()));
        }
    }
    // git's default with no configuration.
    xdg.map(|x| x.join("git/ignore"))
}

// One value from a git config file, which is INI, via `ini::parse`.
// Section and key names compare case-insensitively, as in git; pass
// them lowercased.
fn git_config_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    for item in &crate::ini::parse(&text).items {
        match item {
            crate::ini::Item::Section { name, .. } => {
                in_section = slice(&text, name).to_lowercase() == section;
            }
            crate::ini::Item::Entry { key: k, value: Some(v), .. } if in_section && slice(&text, k).to_lowercase() == key => {
                // A quoted value without its quotes; git's escapes aren't
                // needed.
                return Some(slice(&text, &v.span).trim_matches('"').to_string());
            }
            _ => {}
        }
    }
    None
}

// ini's spans are char offsets, not bytes.
fn slice(text: &str, span: &std::ops::Range<usize>) -> String {
    text.chars().skip(span.start).take(span.end - span.start).collect()
}

fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

// Strips trailing spaces up to an escaped one: a space is escaped after
// an odd number of backslashes, so `a\\ ` still strips.
fn strip_trailing_spaces(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b' ' {
        let mut backslashes = 0;
        let mut i = end - 1;
        while i > 0 && bytes[i - 1] == b'\\' {
            backslashes += 1;
            i -= 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(pattern: &str, path: &str) -> bool {
        Pattern::parse(pattern).unwrap().matches(path, false)
    }

    #[test]
    fn a_bare_name_matches_at_any_depth() {
        assert!(ignored("foo", "foo"));
        assert!(ignored("foo", "a/foo"));
        assert!(ignored("foo", "a/b/foo"));
        assert!(!ignored("foo", "foo/bar"), "the name is the last segment, not a prefix");
        assert!(!ignored("foo", "foobar"));
    }

    // A `/` makes a pattern a location rather than a name.
    #[test]
    fn a_slash_anywhere_but_the_end_anchors_the_pattern() {
        assert!(ignored("/foo", "foo"));
        assert!(!ignored("/foo", "a/foo"));
        assert!(ignored("a/foo", "a/foo"));
        assert!(!ignored("a/foo", "x/a/foo"), "an internal slash anchors it just as a leading one does");
    }

    // Why this isn't `glob::matches` on the whole path.
    #[test]
    fn a_star_does_not_cross_a_slash() {
        assert!(ignored("*.o", "a.o"));
        assert!(ignored("*.o", "src/a.o"), "unanchored, so it matches the name at any depth");
        assert!(!ignored("src/*.o", "src/deep/a.o"), "but the star itself never spans a directory");
        assert!(!ignored("a?c", "a/c"));
    }

    #[test]
    fn bracket_classes_and_escapes_come_from_the_shared_glob() {
        assert!(ignored("[abc].txt", "b.txt"));
        assert!(ignored("[!abc].txt", "z.txt"));
        assert!(ignored("a\\*b", "a*b"));
        assert!(!ignored("a\\*b", "axb"), "the star was escaped, so it is a literal");
    }

    #[test]
    fn a_trailing_slash_matches_directories_only() {
        let p = Pattern::parse("build/").unwrap();
        assert!(p.matches("build", true));
        assert!(!p.matches("build", false));
        assert!(p.dir_only());
    }

    #[test]
    fn leading_double_star_matches_in_every_directory() {
        assert!(ignored("**/foo", "foo"));
        assert!(ignored("**/foo", "a/b/foo"));
        assert!(ignored("**/foo/bar", "x/foo/bar"));
    }

    #[test]
    fn trailing_double_star_matches_what_is_inside_but_not_the_directory() {
        assert!(ignored("abc/**", "abc/x"));
        assert!(ignored("abc/**", "abc/x/y"));
        assert!(!ignored("abc/**", "abc"));
    }

    #[test]
    fn an_interior_double_star_matches_zero_or_more_directories() {
        assert!(ignored("a/**/b", "a/b"));
        assert!(ignored("a/**/b", "a/x/b"));
        assert!(ignored("a/**/b", "a/x/y/b"));
        assert!(!ignored("a/**/b", "b"));
    }

    // wildmatch: a `*` not between slashes is a plain star.
    #[test]
    fn a_double_star_inside_a_segment_is_an_ordinary_star() {
        assert!(ignored("a**b", "axxb"));
        assert!(!ignored("a**b", "a/b"));
    }

    #[test]
    fn blank_lines_and_comments_are_not_patterns() {
        assert_eq!(Pattern::parse(""), None);
        assert_eq!(Pattern::parse("   "), None);
        assert_eq!(Pattern::parse("# a comment"), None);
        assert_eq!(Pattern::parse("/"), None);
        assert!(Pattern::parse("\\#literal").is_some(), "an escaped hash is a pattern");
        assert!(ignored("\\#notes", "#notes"));
    }

    #[test]
    fn trailing_spaces_are_dropped_unless_escaped() {
        assert!(ignored("foo   ", "foo"));
        assert!(ignored("foo\\ ", "foo "));
        assert!(!ignored("foo   ", "foo "));
    }

    #[test]
    fn a_bang_negates_and_is_escapable() {
        assert!(Pattern::parse("!foo").unwrap().negated());
        assert!(!Pattern::parse("\\!foo").unwrap().negated());
        assert!(ignored("\\!foo", "!foo"));
    }

    #[test]
    fn the_last_matching_pattern_wins() {
        let ignore = Ignore::parse("*.log\n!keep.log\n");
        assert_eq!(ignore.matched("debug.log", false), Match::Ignored);
        assert_eq!(ignore.matched("keep.log", false), Match::Whitelisted);
        assert_eq!(ignore.matched("notes.txt", false), Match::None);
        // Order decides.
        let reversed = Ignore::parse("!keep.log\n*.log\n");
        assert_eq!(reversed.matched("keep.log", false), Match::Ignored);
    }

    #[test]
    fn a_whitelist_is_not_the_same_answer_as_no_match() {
        let ignore = Ignore::parse("*.log\n!keep.log\n");
        assert_ne!(ignore.matched("keep.log", false), ignore.matched("keep.txt", false));
    }

    // git's rule, where matching a path alone differs from walking a tree.
    #[test]
    fn a_file_under_an_excluded_directory_cannot_be_re_included() {
        let ignore = Ignore::parse("build/\n!build/keep.txt\n");
        assert_eq!(ignore.matched("build/keep.txt", false), Match::Whitelisted, "the path alone matches the negation");
        assert_eq!(
            ignore.matched_path_or_any_parents("build/keep.txt", false),
            Match::Ignored,
            "but git never descends into build/, so nothing inside can be rescued"
        );
    }

    #[test]
    fn a_deeper_ignore_file_overrides_a_shallower_one() {
        let mut stack = Stack::new();
        stack.push("/repo", Ignore::parse("*.log\n"));
        stack.push("/repo/keep", Ignore::parse("!*.log\n"));
        assert_eq!(stack.matched(Path::new("/repo/a.log"), false), Match::Ignored);
        assert_eq!(stack.matched(Path::new("/repo/keep/a.log"), false), Match::Whitelisted);
    }

    #[test]
    fn a_stack_ignores_files_that_do_not_cover_the_path() {
        let mut stack = Stack::new();
        stack.push("/repo/sub", Ignore::parse("*.log\n"));
        assert_eq!(stack.matched(Path::new("/repo/a.log"), false), Match::None);
        assert_eq!(stack.matched(Path::new("/repo/sub/a.log"), false), Match::Ignored);
    }

    #[test]
    fn the_matching_pattern_can_be_reported_back() {
        let ignore = Ignore::parse("*.log\n!keep.log\n");
        assert_eq!(ignore.matched_pattern("keep.log", false).unwrap().source(), "!keep.log");
    }

    // Asks real git the same questions, skipping without git. Each case
    // gets its own tree, since `check-ignore` looks at the working tree and
    // one tree can't hold a file `foo` and a directory `foo/`; a path that's
    // a prefix of another is a directory, the rest files.
    #[test]
    fn the_answers_agree_with_real_git() {
        if !crate::git::available() {
            return;
        }
        let cases: &[(&str, &[&str])] = &[
            ("foo", &["foo/bar", "a/foo", "a/b/foo", "foobar"]),
            ("foo", &["foo", "a/foo", "foobar"]),
            ("/foo", &["foo", "a/foo"]),
            ("a/foo", &["a/foo", "x/a/foo"]),
            ("*.o", &["a.o", "src/a.o", "a.oo"]),
            ("src/*.o", &["src/a.o", "src/deep/a.o"]),
            ("build/", &["build/x", "a/build/x", "notbuild"]),
            ("build/", &["build", "keep"]),
            ("**/foo", &["foo", "a/b/foo"]),
            ("abc/**", &["abc/x", "abc/y/z"]),
            ("abc/**", &["abc", "other"]),
            ("a/**/b", &["a/b", "a/x/b", "a/x/y/b", "b"]),
            ("a**b", &["axxb", "a/b"]),
            ("[abc].txt", &["b.txt", "z.txt"]),
            ("[!abc].txt", &["z.txt", "b.txt"]),
            ("a?c", &["abc", "a/c"]),
            ("doc/frotz/", &["doc/frotz/x", "a/doc/frotz/x"]),
            ("*.log", &["a.log", "sub/a.log"]),
        ];

        let root = std::env::temp_dir().join(format!("bish-gitignore-vs-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str], dir: &std::path::Path| crate::gittest::output(dir, args);
        crate::gittest::init(&root);

        let mut disagreements = Vec::new();
        for (n, (pattern, paths)) in cases.iter().enumerate() {
            let case = root.join(format!("case{n}"));
            std::fs::create_dir_all(&case).unwrap();
            std::fs::write(case.join(".gitignore"), format!("{pattern}\n")).unwrap();
            let is_dir = |p: &str| paths.iter().any(|other| other.starts_with(&format!("{p}/")));
            for path in *paths {
                let full = case.join(path);
                if is_dir(path) {
                    std::fs::create_dir_all(&full).unwrap();
                } else {
                    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                    std::fs::write(&full, "").unwrap();
                }
            }

            let ignore = Ignore::parse(pattern);
            for path in *paths {
                // `check-ignore` answers as git's walk would, ancestors
                // included, which is `matched_path_or_any_parents`.
                let git_says = run(&["check-ignore", "-q", "--no-index", path], &case).status.success();
                let we_say = ignore.matched_path_or_any_parents(path, is_dir(path)).is_ignored();
                if git_says != we_say {
                    disagreements.push(format!("{pattern:?} vs {path:?} (dir={}): git={git_says} bish={we_say}", is_dir(path)));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(disagreements.is_empty(), "disagreed with real git:\n  {}", disagreements.join("\n  "));
    }

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!("bish-gitignore-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".git/info")).unwrap();
            Tmp(dir)
        }
        fn write(&self, name: &str, contents: &str) {
            let p = self.0.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn for_directory_finds_nothing_outside_a_repository() {
        let dir = std::env::temp_dir().join(format!("bish-gitignore-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        // A `.gitignore` with no repository above it isn't a rule.
        assert!(Stack::for_directory(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn for_directory_collects_every_gitignore_from_the_root_down() {
        let t = Tmp::new("stack");
        t.write(".gitignore", "*.log\n");
        t.write("a/b/.gitignore", "!*.log\n");
        let stack = Stack::for_directory(&t.0.join("a/b"));
        assert_eq!(stack.matched(&t.0.join("x.log"), false), Match::Ignored);
        assert_eq!(stack.matched(&t.0.join("a/x.log"), false), Match::Ignored);
        assert_eq!(stack.matched(&t.0.join("a/b/x.log"), false), Match::Whitelisted, "the deepest file wins");
    }

    #[test]
    fn for_directory_reads_git_info_exclude() {
        let t = Tmp::new("exclude");
        t.write(".git/info/exclude", "secret*\n");
        assert_eq!(Stack::for_directory(&t.0).matched(&t.0.join("secrets.txt"), false), Match::Ignored);
    }

    // `core.excludesFile` read from a git config, which is INI.
    #[test]
    fn core_excludes_file_is_read_out_of_the_git_config() {
        let t = Tmp::new("excludesfile");
        t.write("myignores", "*.bak\n");
        t.write(".git/config", format!("[core]\n\texcludesFile = {}\n", t.0.join("myignores").display()).as_str());
        assert_eq!(Stack::for_directory(&t.0).matched(&t.0.join("a.bak"), false), Match::Ignored);
    }

    #[test]
    fn git_config_values_are_found_case_insensitively_and_unquoted() {
        let t = Tmp::new("config");
        t.write(".git/config", "[CORE]\n\tExcludesFile = \"/tmp/x y\"\n");
        assert_eq!(git_config_value(&t.0.join(".git/config"), "core", "excludesfile").as_deref(), Some("/tmp/x y"));
        assert_eq!(git_config_value(&t.0.join(".git/config"), "core", "missing"), None);
        // The same key in another section is a different key.
        t.write(".git/config", "[other]\n\texcludesfile = /tmp/x\n");
        assert_eq!(git_config_value(&t.0.join(".git/config"), "core", "excludesfile"), None);
    }

    #[test]
    fn nothing_typeable_panics() {
        for line in ["", "!", "/", "//", "**", "**/", "/**", "\\", "[", "[]", "!/", "a//b", "   ", "!!x", "***"] {
            if let Some(p) = Pattern::parse(line) {
                for path in ["", "a", "a/b", "a/b/c"] {
                    p.matches(path, false);
                    p.matches(path, true);
                }
            }
        }
    }
}
