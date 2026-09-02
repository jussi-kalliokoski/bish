// git pathspecs: the `--` arguments that limit a git command to part of
// the tree, including the magic prefixes (`:(exclude)`, `:!`, `:/`,
// `:(glob)`, `:(icase)`, `:(literal)`).
//
// Two glob dialects live here, which is the thing to understand about
// pathspecs and the reason this reuses two different matchers rather
// than one:
//
//   - **By default a pathspec's `*` crosses `/`.** `git log '*.c'` finds
//     `src/deep/a.c`, because git matches with fnmatch *without*
//     `FNM_PATHNAME`. That is exactly what `glob::matches` already does,
//     so the default mode is the shell glob this repo has had all along,
//     handed the whole path.
//   - **`:(glob)` turns on pathname semantics**, where `*` stops at a
//     `/` and `**` is how you cross one. That is the gitignore dialect,
//     so it reuses `gitignore::Pattern` -- anchored, since a pathspec is
//     always relative to a known root rather than a name to look for at
//     any depth.
//
// On top of either, git matches a pathspec that names a directory
// against everything under it (`git log Documentation` covers
// `Documentation/git.txt`).
#![allow(dead_code)]

/// One pathspec: its magic, and the pattern the magic applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pathspec {
    /// `:(exclude)`, `:!` or `:^`. A list is filtered by these *after*
    /// the positive specs have chosen, never as a match of its own.
    pub exclude: bool,
    /// `:(top)` or `:/`: the pattern is relative to the repository root
    /// rather than the current directory. Carried, not acted on -- only
    /// the caller knows which root a path is already relative to.
    pub top: bool,
    /// `:(literal)`: no wildcards at all, the pattern is the name.
    pub literal: bool,
    /// `:(icase)`: match without regard to case.
    pub icase: bool,
    /// `:(glob)`: `*` stops at a `/`, and `**` crosses one.
    pub glob: bool,
    pub pattern: String,
}

impl Pathspec {
    pub fn parse(spec: &str) -> Result<Pathspec, String> {
        let mut out = Pathspec { exclude: false, top: false, literal: false, icase: false, glob: false, pattern: String::new() };
        let Some(rest) = spec.strip_prefix(':') else {
            // No leading colon: the whole thing is the pattern, magic and
            // all -- which is why a file really named `:foo` needs
            // `:(literal):foo`.
            out.pattern = spec.to_string();
            return Ok(out);
        };
        let rest = if let Some(long) = rest.strip_prefix('(') {
            let Some(close) = long.find(')') else {
                return Err(format!("Missing ')' at the end of pathspec magic in '{spec}'"));
            };
            for word in long[..close].split(',').filter(|w| !w.is_empty()) {
                match word {
                    "top" => out.top = true,
                    "literal" => out.literal = true,
                    "icase" => out.icase = true,
                    "glob" => out.glob = true,
                    "exclude" => out.exclude = true,
                    // Recognized, and refused rather than quietly
                    // dropped: silently matching more than was asked for
                    // is worse than saying no.
                    w if w.starts_with("attr:") => {
                        return Err(format!("Unimplemented pathspec magic 'attr' in '{spec}'"));
                    }
                    w => return Err(format!("Invalid pathspec magic '{w}' in '{spec}'")),
                }
            }
            &long[close + 1..]
        } else {
            // Short form: a run of magic characters, optionally closed by
            // a `:` so that the pattern can start with one of them.
            let mut chars = rest.char_indices();
            let mut end = rest.len();
            for (i, c) in chars.by_ref() {
                match c {
                    '/' => out.top = true,
                    '!' | '^' => out.exclude = true,
                    ':' => {
                        end = i + 1;
                        break;
                    }
                    _ => {
                        end = i;
                        break;
                    }
                }
                end = i + c.len_utf8();
            }
            &rest[end..]
        };
        // `:(literal)` and `:(glob)` say opposite things about the same
        // characters, so asking for both is a mistake worth reporting.
        if out.literal && out.glob {
            return Err(format!("'literal' and 'glob' are incompatible in '{spec}'"));
        }
        out.pattern = rest.to_string();
        Ok(out)
    }

    /// Whether this spec covers `path` -- `/`-separated and already
    /// relative to whichever root `top` selected.
    pub fn matches(&self, path: &str) -> bool {
        // An empty pattern is the whole tree: `:/` on its own, or a bare
        // `:`, means "everything from here".
        if self.pattern.is_empty() {
            return true;
        }
        let (pattern, path) = match self.icase {
            true => (self.pattern.to_lowercase(), path.to_lowercase()),
            false => (self.pattern.clone(), path.to_string()),
        };
        // Naming a directory covers everything under it, whichever
        // dialect is in play.
        if path.starts_with(&format!("{}/", pattern.trim_end_matches('/'))) && !crate::glob::has_meta(&pattern) {
            return true;
        }
        if self.literal {
            return path == pattern;
        }
        if self.glob {
            // Anchored, because a pathspec is a location: `:(glob)a/b`
            // is `a/b` from the root, not a `b` inside any `a`.
            let anchored = if pattern.starts_with('/') { pattern.clone() } else { format!("/{pattern}") };
            return crate::gitignore::Pattern::parse(&anchored).is_some_and(|p| p.matches(&path, false));
        }
        crate::glob::matches(&pattern, &path)
    }
}

/// A whole `--` argument list. Exclusions are not alternatives to the
/// positive specs -- they are a filter applied after them, which is why
/// a list of nothing but `:!` specs still matches everything else.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pathspecs {
    specs: Vec<Pathspec>,
}

impl Pathspecs {
    pub fn parse<'a>(specs: impl IntoIterator<Item = &'a str>) -> Result<Pathspecs, String> {
        Ok(Pathspecs { specs: specs.into_iter().map(Pathspec::parse).collect::<Result<_, _>>()? })
    }

    pub fn is_empty(&self) -> bool {
        self.specs.is_empty()
    }

    pub fn specs(&self) -> &[Pathspec] {
        &self.specs
    }

    pub fn matches(&self, path: &str) -> bool {
        let (excludes, includes): (Vec<&Pathspec>, Vec<&Pathspec>) = self.specs.iter().partition(|s| s.exclude);
        // No positive spec at all means the whole tree was asked for --
        // `git log -- ':!docs'` is every path but docs, not no paths.
        let included = includes.is_empty() || includes.iter().any(|s| s.matches(path));
        included && !excludes.iter().any(|s| s.matches(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(spec: &str) -> Pathspec {
        Pathspec::parse(spec).unwrap()
    }

    #[test]
    fn a_plain_pathspec_has_no_magic() {
        let p = parse("src/main.rs");
        assert_eq!(p.pattern, "src/main.rs");
        assert!(!p.exclude && !p.top && !p.literal && !p.icase && !p.glob);
    }

    #[test]
    fn short_magic_is_recognized() {
        assert!(parse(":!docs").exclude);
        assert!(parse(":^docs").exclude);
        assert!(parse(":/docs").top);
        let both = parse(":!/docs");
        assert!(both.exclude && both.top);
        assert_eq!(both.pattern, "docs");
    }

    // The `:` terminator is what lets a pattern start with a magic
    // character.
    #[test]
    fn a_colon_ends_short_magic() {
        assert_eq!(parse(":!:!literal").pattern, "!literal");
        assert_eq!(parse("::foo").pattern, "foo");
    }

    #[test]
    fn long_magic_is_recognized() {
        let p = parse(":(exclude,icase,glob)*.C");
        assert!(p.exclude && p.icase && p.glob);
        assert_eq!(p.pattern, "*.C");
        assert!(parse(":(top)x").top);
        assert!(parse(":(literal)x").literal);
    }

    #[test]
    fn bad_magic_is_reported_rather_than_ignored() {
        assert!(Pathspec::parse(":(nonsense)x").is_err());
        assert!(Pathspec::parse(":(exclude").is_err());
        assert!(Pathspec::parse(":(literal,glob)x").is_err());
        // Real magic this doesn't implement: refused, not silently
        // widened into "match everything".
        assert!(Pathspec::parse(":(attr:binary)x").unwrap_err().contains("attr"));
    }

    // The default dialect is fnmatch without FNM_PATHNAME, which is the
    // shell glob this repo already had.
    #[test]
    fn by_default_a_star_crosses_a_slash() {
        assert!(parse("*.c").matches("a.c"));
        assert!(parse("*.c").matches("src/deep/a.c"));
    }

    // ...and `:(glob)` is the other dialect entirely.
    #[test]
    fn glob_magic_stops_a_star_at_a_slash() {
        assert!(!parse(":(glob)*.c").matches("src/a.c"));
        assert!(parse(":(glob)*.c").matches("a.c"));
        assert!(parse(":(glob)**/*.c").matches("src/deep/a.c"));
        assert!(parse(":(glob)src/**/a.c").matches("src/deep/a.c"));
    }

    // A pathspec is a location, so `:(glob)b` must not find `a/b`.
    #[test]
    fn glob_magic_is_anchored_unlike_a_gitignore_pattern() {
        assert!(parse(":(glob)b").matches("b"));
        assert!(!parse(":(glob)b").matches("a/b"));
    }

    #[test]
    fn naming_a_directory_covers_everything_under_it() {
        assert!(parse("Documentation").matches("Documentation/git.txt"));
        assert!(parse("Documentation").matches("Documentation/a/b.txt"));
        assert!(!parse("Documentation").matches("Documentationx/a"));
        assert!(parse(":(glob)src").matches("src/main.rs"));
    }

    #[test]
    fn literal_magic_turns_off_the_wildcards() {
        assert!(parse(":(literal)a*b").matches("a*b"));
        assert!(!parse(":(literal)a*b").matches("axb"));
    }

    #[test]
    fn icase_magic_matches_either_spelling() {
        assert!(parse(":(icase)readme.md").matches("README.md"));
        assert!(parse(":(icase)*.C").matches("a.c"));
        assert!(!parse("readme.md").matches("README.md"));
    }

    #[test]
    fn an_empty_pattern_is_the_whole_tree() {
        assert!(parse(":/").matches("anything/at/all"));
        assert!(parse(":").matches("anything/at/all"));
    }

    // Excludes filter, they don't select.
    #[test]
    fn a_list_of_only_exclusions_still_matches_everything_else() {
        let specs = Pathspecs::parse([":!docs"]).unwrap();
        assert!(specs.matches("src/main.rs"));
        assert!(!specs.matches("docs/a.md"));
    }

    #[test]
    fn a_positive_spec_narrows_and_an_exclusion_then_filters() {
        let specs = Pathspecs::parse(["src", ":!src/vendor"]).unwrap();
        assert!(specs.matches("src/main.rs"));
        assert!(!specs.matches("src/vendor/x.rs"));
        assert!(!specs.matches("docs/a.md"));
    }

    #[test]
    fn an_empty_list_matches_everything() {
        assert!(Pathspecs::default().matches("anything"));
    }

    // The same ground truth the gitignore module is held to: ask real
    // git which paths a pathspec selects, and require the same set.
    // Skipped when git isn't installed.
    #[test]
    fn the_answers_agree_with_real_git() {
        if !crate::git::available() {
            return;
        }
        let files =
            ["a.c", "README.md", "src/main.rs", "src/a.c", "src/deep/a.c", "src/vendor/x.rs", "docs/a.md", "docs/deep/b.md", "Documentation/git.txt"];
        let root = std::env::temp_dir().join(format!("bish-pathspec-vs-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&root).output().expect("git");
        run(&["init", "-q"]);
        for f in files {
            let full = root.join(f);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "").unwrap();
        }
        run(&["add", "-A"]);

        let cases: &[&[&str]] = &[
            &["*.c"],
            &[":(glob)*.c"],
            &[":(glob)**/*.c"],
            &["src"],
            &["src", ":!src/vendor"],
            &[":!docs"],
            &[":(icase)readme.md"],
            &[":(literal)a.c"],
            &[":(glob)src/**/a.c"],
            &["docs/*"],
            &[":/"],
        ];
        let mut disagreements = Vec::new();
        for case in cases {
            let mut args = vec!["ls-files", "--"];
            args.extend(case.iter().copied());
            let out = run(&args);
            let mut git_says: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(|l| l.to_string()).collect();
            git_says.sort();
            let specs = Pathspecs::parse(case.iter().copied()).unwrap();
            let mut we_say: Vec<String> = files.iter().filter(|f| specs.matches(f)).map(|f| f.to_string()).collect();
            we_say.sort();
            if git_says != we_say {
                disagreements.push(format!("{case:?}\n      git:  {git_says:?}\n      bish: {we_say:?}"));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(disagreements.is_empty(), "disagreed with real git:\n    {}", disagreements.join("\n    "));
    }

    #[test]
    fn nothing_typeable_panics() {
        for spec in [":", "::", ":(", ":()", ":()x", ":!", ":/", ":^", "", "*", "**", ":(glob)", ":(literal)", ":!!"] {
            if let Ok(p) = Pathspec::parse(spec) {
                for path in ["", "a", "a/b", "a/b/c"] {
                    p.matches(path);
                }
            }
        }
    }
}
