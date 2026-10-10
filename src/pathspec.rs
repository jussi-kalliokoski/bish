// git pathspecs: the arguments after `--` that limit a git command to
// part of the tree, with the magic prefixes (`:(exclude)`, `:!`, `:/`,
// `:(glob)`, `:(icase)`, `:(literal)`).
//
// Two glob dialects, so two matchers:
//
//   - By default `*` crosses `/`: `git log '*.c'` finds `src/deep/a.c`, as
//     git uses fnmatch without `FNM_PATHNAME`. That's `glob::matches`,
//     given the whole path.
//   - `:(glob)` has pathname semantics: `*` stops at `/`, `**` crosses
//     it. That's the gitignore dialect, so `gitignore::Pattern`, anchored,
//     since a pathspec is relative to a known root.
//
// Either way, a pathspec naming a directory covers everything under it
// (`git log Documentation` covers `Documentation/git.txt`).
#![allow(dead_code)]

/// One pathspec: its magic, and the pattern the magic applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pathspec {
    /// `:(exclude)`, `:!` or `:^`: applied as a filter after the positive
    /// specs, never as a match.
    pub exclude: bool,
    /// `:(top)` or `:/`: relative to the repository root, not the current
    /// directory. Carried, not acted on: only the caller knows the root.
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
            // No leading colon: the whole thing is the pattern, so a file
            // named `:foo` needs `:(literal):foo`.
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
                    // Recognized and refused: silently matching more than
                    // asked is worse than saying no.
                    w if w.starts_with("attr:") => {
                        return Err(format!("Unimplemented pathspec magic 'attr' in '{spec}'"));
                    }
                    w => return Err(format!("Invalid pathspec magic '{w}' in '{spec}'")),
                }
            }
            &long[close + 1..]
        } else {
            // Short form: magic characters, optionally closed by `:` so the
            // pattern can start with one.
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
        // `:(literal)` and `:(glob)` contradict each other: an error.
        if out.literal && out.glob {
            return Err(format!("'literal' and 'glob' are incompatible in '{spec}'"));
        }
        out.pattern = rest.to_string();
        Ok(out)
    }

    /// Whether this spec covers `path`, `/`-separated and relative to the
    /// root `top` selected.
    pub fn matches(&self, path: &str) -> bool {
        // An empty pattern is the whole tree: `:/` alone, or a bare `:`.
        if self.pattern.is_empty() {
            return true;
        }
        let (pattern, path) = match self.icase {
            true => (self.pattern.to_lowercase(), path.to_lowercase()),
            false => (self.pattern.clone(), path.to_string()),
        };
        // Naming a directory covers everything under it, in either dialect.
        if path.starts_with(&format!("{}/", pattern.trim_end_matches('/'))) && !crate::glob::has_meta(&pattern) {
            return true;
        }
        if self.literal {
            return path == pattern;
        }
        if self.glob {
            // Anchored, as a pathspec is a location: `:(glob)a/b` is `a/b`
            // from the root, not a `b` in any `a`.
            let anchored = if pattern.starts_with('/') { pattern.clone() } else { format!("/{pattern}") };
            return crate::gitignore::Pattern::parse(&anchored).is_some_and(|p| p.matches(&path, false));
        }
        crate::glob::matches(&pattern, &path)
    }
}

/// A whole `--` argument list. Exclusions filter after the positive
/// specs, so a list of only `:!` specs matches everything else.
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
        // No positive spec means the whole tree: `git log -- ':!docs'` is
        // every path but docs.
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

    // The `:` terminator lets a pattern start with a magic character.
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
        // Unimplemented magic is refused, not widened to match everything.
        assert!(Pathspec::parse(":(attr:binary)x").unwrap_err().contains("attr"));
    }

    // By default, fnmatch without `FNM_PATHNAME`: the shell glob.
    #[test]
    fn by_default_a_star_crosses_a_slash() {
        assert!(parse("*.c").matches("a.c"));
        assert!(parse("*.c").matches("src/deep/a.c"));
    }

    // `:(glob)` is the other dialect.
    #[test]
    fn glob_magic_stops_a_star_at_a_slash() {
        assert!(!parse(":(glob)*.c").matches("src/a.c"));
        assert!(parse(":(glob)*.c").matches("a.c"));
        assert!(parse(":(glob)**/*.c").matches("src/deep/a.c"));
        assert!(parse(":(glob)src/**/a.c").matches("src/deep/a.c"));
    }

    // A pathspec is a location: `:(glob)b` doesn't find `a/b`.
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

    // Real git, asked which paths a pathspec selects, must agree, as for
    // the gitignore module. Skipped without git.
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
        let run = |args: &[&str]| crate::gittest::output(&root, args);
        crate::gittest::init(&root);
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
