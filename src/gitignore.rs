// `.gitignore` pattern matching, per gitignore(5).
//
// Built on `glob::matches` rather than beside it: a gitignore pattern is
// a *path* pattern, and the one thing that separates it from the shell
// glob this repo already has is that `*` and `?` must not cross a `/`.
// That falls out for free by splitting both the pattern and the path on
// `/` and matching one segment at a time -- so bracket classes,
// backslash escapes and the rest keep exactly the meaning they already
// have everywhere else in bish, and there is no second glob engine to
// drift from the first. (One consequence worth naming: `glob::matches`
// also recognizes extglob `@(a|b)`, which real git does not. That is the
// same strict-superset stance glob.rs already documents for bash.)
//
// The `**` segment is the piece a shell glob has no equivalent for, and
// is handled here rather than in glob.rs, since it is only meaningful
// between slashes.
//
// What this module does *not* do is walk a directory tree or read a file
// off disk. That belongs to whoever is doing the walking -- and the walk
// isn't incidental: git's "it is not possible to re-include a file if a
// parent directory of that file is excluded" is a property of *not
// descending*, not of the patterns. `matched_path_or_any_parents` is
// here for callers that have a path but no walk, and implements that
// rule explicitly.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// What a set of patterns says about one path. `Whitelisted` is a `!`
/// pattern winning, which is not the same as no pattern matching at all
/// -- a caller stacking several ignore files needs to tell those apart,
/// since a whitelist in a deeper file has to stop a shallower file's
/// exclusion from applying.
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
    /// The line this came from, so a caller can say *which* rule
    /// excluded a file the way `git check-ignore -v` does.
    source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// A `**` segment: any run of directories. Zero or more in the
    /// middle of a pattern (`a/**/b` matches `a/b`), but one or more at
    /// the end, since `abc/**` means everything *inside* `abc`.
    AnyDirs,
    /// Anything else, matched against one path segment by `glob`.
    Glob(String),
}

impl Pattern {
    /// One line of a `.gitignore`. `None` for a line that is not a
    /// pattern at all: blank, a `#` comment, or a `/` with nothing left
    /// after the trailing-slash strip.
    pub fn parse(line: &str) -> Option<Pattern> {
        let source = line.to_string();
        // Trailing spaces are ignored unless backslash-escaped -- so
        // `foo\ ` really does mean a name ending in a space.
        let line = strip_trailing_spaces(line);
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (negated, line) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            // `\#` and `\!` are the escapes that let a pattern start
            // with either character literally. The backslash is dropped
            // here rather than left for `glob`, which would otherwise
            // treat it as an escape a second time and change nothing --
            // correct by luck, not by design.
            None => (false, line.strip_prefix('\\').filter(|r| r.starts_with(['#', '!'])).unwrap_or(line)),
        };
        let (dir_only, line) = match line.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        // A `/` anywhere but the (already removed) end anchors the
        // pattern; a leading one anchors it and is otherwise not part of
        // the pattern.
        let anchored = line.contains('/');
        let line = line.strip_prefix('/').unwrap_or(line);
        let segments: Vec<Segment> = line
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| if s == "**" { Segment::AnyDirs } else { Segment::Glob(s.to_string()) })
            .collect();
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

    /// Whether this pattern matches `path` -- a path relative to the
    /// directory the ignore file lives in, `/`-separated.
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
        // Unanchored: `foo` is a name, not a location, so it matches at
        // any depth. Trying every starting segment is what says that.
        (0..segments.len()).any(|i| match_segments(&self.segments, &segments[i..]))
    }
}

fn match_segments(pattern: &[Segment], path: &[&str]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(Segment::AnyDirs) => {
            let rest = &pattern[1..];
            // A trailing `**` has to consume something: `abc/**` is
            // "everything inside abc", which does not include `abc`.
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

    /// The last pattern to match wins -- which is the whole reason a `!`
    /// line has to come *after* the rule it is undoing.
    pub fn matched(&self, path: &str, is_dir: bool) -> Match {
        self.matched_pattern(path, is_dir).map_or(Match::None, |p| if p.negated { Match::Whitelisted } else { Match::Ignored })
    }

    /// The same decision, but handing back the rule that made it, for a
    /// caller that wants to explain itself the way `git check-ignore -v`
    /// does.
    pub fn matched_pattern(&self, path: &str, is_dir: bool) -> Option<&Pattern> {
        self.patterns.iter().rev().find(|p| p.matches(path, is_dir))
    }

    /// For a caller holding a path but no directory walk. Git excludes a
    /// file under an excluded directory by never descending into it,
    /// which is also why a `!` line inside cannot rescue it -- there is
    /// nothing to re-include. Checking the ancestors from the top down,
    /// and stopping at the first excluded one, is that rule written out.
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

/// Several ignore files at once, each rooted at the directory it was
/// found in. The deepest file that applies to a path decides it, which
/// is how a `.gitignore` in a subdirectory overrides the one at the top
/// of the repository.
#[derive(Debug, Clone, Default)]
pub struct Stack {
    files: Vec<(PathBuf, Ignore)>,
}

impl Stack {
    pub fn new() -> Stack {
        Stack::default()
    }

    /// `base` is the directory the file was found in; patterns in it are
    /// relative to that.
    pub fn push(&mut self, base: impl Into<PathBuf>, ignore: Ignore) {
        self.files.push((base.into(), ignore));
    }

    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|(_, i)| i.is_empty())
    }

    /// Every ignore file that applies inside `dir`, read off disk in
    /// the order git itself consults them.
    ///
    /// Outside a repository this is empty and everything is visible:
    /// `.gitignore` files only mean anything relative to a repository
    /// root, and honouring a stray one in some unrelated directory
    /// would be inventing a rule git doesn't have.
    ///
    /// Sources, shallowest first (so the deepest still wins, per
    /// `matched`): `core.excludesFile`, `.git/info/exclude`, then a
    /// `.gitignore` in every directory from the root down to `dir`.
    /// The first two are repository-wide, so both are based at the root.
    pub fn for_directory(dir: &Path) -> Stack {
        let mut stack = Stack::new();
        let Some(root) = repo_root(dir) else { return stack };
        if let Some(excludes) = excludes_file(&root) {
            stack.push_file(&root, &excludes);
        }
        stack.push_file(&root, &root.join(".git/info/exclude"));
        // Root first, then each directory on the way down -- `matched`
        // orders by depth itself, but reading in this order keeps the
        // stack legible to anyone printing it.
        let mut here = PathBuf::new();
        for component in dir.strip_prefix(&root).unwrap_or(Path::new("")).components() {
            stack.push_file(&root.join(&here), &root.join(&here).join(".gitignore"));
            here.push(component);
        }
        stack.push_file(dir, &dir.join(".gitignore"));
        stack
    }

    // Reads one ignore file if it is there, and says nothing if it
    // isn't -- a missing `.gitignore` is the normal case, not an error.
    fn push_file(&mut self, base: &Path, file: &Path) {
        if let Ok(text) = std::fs::read_to_string(file) {
            let ignore = Ignore::parse(&text);
            if !ignore.is_empty() {
                self.push(base, ignore);
            }
        }
    }

    /// Deepest base first, and the first file with anything to say
    /// decides -- including when what it says is `Whitelisted`, which is
    /// exactly how a nested `!` line overrides an exclusion from above.
    pub fn matched(&self, path: &Path, is_dir: bool) -> Match {
        let mut applicable: Vec<&(PathBuf, Ignore)> = self.files.iter().filter(|(base, _)| path.starts_with(base)).collect();
        applicable.sort_by_key(|(base, _)| std::cmp::Reverse(base.components().count()));
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

/// The nearest ancestor of `dir` holding a `.git` -- a directory
/// normally, but a *file* in a worktree or submodule, which is why this
/// only asks whether the name exists.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let mut here = dir;
    loop {
        if here.join(".git").exists() {
            return Some(here.to_path_buf());
        }
        here = here.parent()?;
    }
}

// git's `core.excludesFile`, from the repository config and then the
// user's, falling back to the location git uses when nobody set one.
fn excludes_file(root: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let configs = [
        Some(root.join(".git/config")),
        xdg.as_ref().map(|x| x.join("git/config")),
        home.as_ref().map(|h| h.join(".gitconfig")),
    ];
    for config in configs.into_iter().flatten() {
        if let Some(value) = git_config_value(&config, "core", "excludesfile") {
            return Some(expand_tilde(&value, home.as_deref()));
        }
    }
    // What git uses with no configuration at all.
    xdg.map(|x| x.join("git/ignore"))
}

// One value out of a git config file, which is INI -- so this is
// `ini::parse` and nothing more. Section and key names are compared
// case-insensitively, as git compares them; `section` and `key` are
// expected already lowercased.
fn git_config_value(path: &Path, section: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_section = false;
    for item in &crate::ini::parse(&text).items {
        match item {
            crate::ini::Item::Section { name, .. } => {
                in_section = slice(&text, name).to_lowercase() == section;
            }
            crate::ini::Item::Entry { key: k, value: Some(v), .. }
                if in_section && slice(&text, k).to_lowercase() == key =>
            {
                // A quoted value is the same string without its quotes;
                // nothing here needs git's escapes.
                return Some(slice(&text, &v.span).trim_matches('"').to_string());
            }
            _ => {}
        }
    }
    None
}

// ini's spans are char offsets, so this can't be a byte slice.
fn slice(text: &str, span: &std::ops::Range<usize>) -> String {
    text.chars().skip(span.start).take(span.end - span.start).collect()
}

fn expand_tilde(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

// Walks back over trailing spaces, stopping at one that is escaped: a
// space is escaped when an *odd* number of backslashes precede it, so
// `a\\ ` (an escaped backslash, then a space) still strips.
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

    // The rule that makes a pattern a location rather than a name.
    #[test]
    fn a_slash_anywhere_but_the_end_anchors_the_pattern() {
        assert!(ignored("/foo", "foo"));
        assert!(!ignored("/foo", "a/foo"));
        assert!(ignored("a/foo", "a/foo"));
        assert!(!ignored("a/foo", "x/a/foo"), "an internal slash anchors it just as a leading one does");
    }

    // The whole reason this isn't just `glob::matches` on the full path.
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

    // `abc/**` is everything *inside* abc, so it must not match abc.
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

    // wildmatch's rule: asterisks not surrounded by slashes are just a
    // star, which falls out of handing the segment to `glob` unchanged.
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
        // Order really is what decides it.
        let reversed = Ignore::parse("!keep.log\n*.log\n");
        assert_eq!(reversed.matched("keep.log", false), Match::Ignored);
    }

    // `Whitelisted` has to be distinct from `None`, or a stack couldn't
    // tell "a rule said keep this" from "no rule mentioned it".
    #[test]
    fn a_whitelist_is_not_the_same_answer_as_no_match() {
        let ignore = Ignore::parse("*.log\n!keep.log\n");
        assert_ne!(ignore.matched("keep.log", false), ignore.matched("keep.txt", false));
    }

    // git's own rule, and the one place where matching a path in
    // isolation differs from walking a tree.
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

    // The strongest check available: ask real git the same questions and
    // require the same answers. Skipped when git isn't installed, the
    // same way anything else here that shells out to it is -- a missing
    // git is not a failing test.
    //
    // Each case gets its own directory tree, because `check-ignore`
    // consults the working tree to decide what is a directory and one
    // tree can't hold both a file `foo` and a directory `foo/`. Within a
    // case, any path that is a prefix of another is created as a
    // directory and the rest as files, which is the only arrangement
    // that can exist at all.
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
        let run = |args: &[&str], dir: &std::path::Path| {
            std::process::Command::new("git").args(args).current_dir(dir).output().expect("git")
        };
        run(&["init", "-q"], &root);

        let mut disagreements = Vec::new();
        for (n, (pattern, paths)) in cases.iter().enumerate() {
            let case = root.join(format!("case{n}"));
            std::fs::create_dir_all(&case).unwrap();
            std::fs::write(case.join(".gitignore"), format!("{pattern}\n")).unwrap();
            // A path that another path continues through has to be a
            // directory; everything else is a file.
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
                // `check-ignore` answers as git's own walk would,
                // ancestors included -- which is what
                // matched_path_or_any_parents is for.
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
        // A `.gitignore` with no repository above it is not a rule -- it
        // is a file that happens to be named that.
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

    // The payoff from the INI parser: `core.excludesFile` is a git
    // config value, and a git config is an INI file.
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
        // A key of the same name in a different section is a different
        // key.
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
