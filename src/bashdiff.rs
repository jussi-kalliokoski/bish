// A corpus of shell snippets that bash and bish should agree on, run
// through both and diffed.
//
// This file is the survey that produced the second roadmap, made
// permanent. Its value is not the cases already passing -- it is that
// adding a case costs one line, so the next question of the form "does
// bish do X the way bash does?" is answered by writing it down rather
// than by a temp file that gets deleted.
//
// The two lists matter equally:
//
// - `CASES` is what should agree, and does.
// - `DIVERGENCES` is what does not agree, each with the reason. Every
//   entry is *asserted to still diverge*, so one that gets fixed fails
//   this test until its entry is removed. A known-issues list nobody
//   is forced to update is worth nothing.
//
// bash is run with `LC_ALL=C` -- otherwise this measures the machine's
// locale rather than the shell (a Finnish `printf %.2f` writes `3,14`)
// -- and both shells get an empty `HOME` and a scratch cwd so neither
// reads a config or trips over the other's files.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    struct Case {
        name: &'static str,
        script: &'static str,
    }

    const fn case(name: &'static str, script: &'static str) -> Case {
        Case { name, script }
    }

    // Deliberately no `$RANDOM`, no timestamps, no pids: a case that
    // cannot answer the same way twice cannot be compared at all.
    const CASES: &[Case] = &[
        // -- expansion ------------------------------------------------
        case("param-default", r#"unset u; echo "${u:-d}" "${u-d}" "${u:+s}""#),
        case("param-assign-default", r#"unset u; echo "${u:=set}" "$u""#),
        case("param-length", r#"x=abcd; echo "${#x}""#),
        case("param-case", r#"x=abc; echo "${x^}" "${x^^}" "${x,,}""#),
        case("param-substring", r#"x=abcdef; echo "${x:1:3}" "${x: -2}" "${x:2}""#),
        case("param-strip", r#"p=a/b/c.txt; echo "${p##*/}" "${p%.*}" "${p#*/}" "${p%%/*}""#),
        case("param-subst", r#"x=aXbXc; echo "${x/X/-}" "${x//X/-}" "${x/#a/A}" "${x/%c/C}""#),
        case("param-indirect", r#"v=hello; n=v; echo "${!n}""#),
        case("param-at-split", r#"set -- "a b" c; for w in "$@"; do echo "[$w]"; done"#),
        case("param-nested", r#"x=abc; echo "${x/$(printf b)/Z}""#),
        // -- arrays ---------------------------------------------------
        case("array-basic", r#"a=(1 2 3); echo "${a[0]}" "${a[@]}" "${#a[@]}" "${!a[@]}""#),
        case("array-append", r#"a=(1); a+=(2 3); echo "${a[@]}""#),
        case("array-negative", r#"a=(1 2 3); echo "${a[-1]}""#),
        case("array-unset-elem", r#"a=(1 2 3); unset 'a[1]'; echo "${a[@]}" "${#a[@]}""#),
        case("assoc-basic", r#"declare -A m=([k]=v [j]=w); echo "${m[k]}" "${#m[@]}""#),
        // -- arithmetic -----------------------------------------------
        case("arith-ops", r#"echo $((1+2*3)) $((7/2)) $((7%3)) $((2**10)) $((1<<4))"#),
        case("arith-base", r#"echo $((16#ff)) $((2#1011)) $((8#17))"#),
        case("arith-ternary-comma", r#"echo $((1?2:3)) $((1,2,3))"#),
        case("arith-incr", r#"i=1; echo $((i++)) $i $((++i)) $i"#),
        case("arith-compare", r#"echo $((3>2)) $((3<2)) $((3==3)) $((1&&0)) $((1||0))"#),
        case("arith-assign-ops", r#"i=10; ((i+=5)); ((i*=2)); echo $i"#),
        case("arith-let", r#"let 'a = 3 + 4'; echo $a"#),
        // -- conditionals ---------------------------------------------
        case("test-string", r#"[ a = a ] && echo eq; [ -n "" ] || echo empty; [ -z "" ] && echo z"#),
        case("test-numeric", r#"[ 3 -gt 2 ] && [ 2 -le 2 ] && echo num"#),
        case("dbracket-regex", r#"[[ ab12 =~ ([a-z]+)([0-9]+) ]] && echo "${BASH_REMATCH[1]}-${BASH_REMATCH[2]}""#),
        case("dbracket-logic", r#"[[ a == a && b == b ]] && echo both; [[ a == b || c == c ]] && echo either"#),
        case("dbracket-lt", r#"[[ a < b ]] && echo lt"#),
        // -- control flow ---------------------------------------------
        case("if-elif-else", r#"if false; then echo a; elif true; then echo b; else echo c; fi"#),
        case("for-in", r#"for i in 1 2 3; do printf '%s' "$i"; done; echo"#),
        case("for-cstyle", r#"for ((i=0;i<3;i++)); do printf '%s' "$i"; done; echo"#),
        case("while-until", r#"i=0; while [ $i -lt 2 ]; do i=$((i+1)); done; until [ $i -ge 4 ]; do i=$((i+1)); done; echo $i"#),
        case("case-patterns", r#"for x in ab cd zz; do case $x in a*) echo A;; c?) echo C;; *) echo other;; esac; done"#),
        case("case-fallthrough", r#"case ab in a*) echo one;;& *b) echo two;; esac"#),
        case("break-continue", r#"for i in 1 2 3 4; do [ $i = 2 ] && continue; [ $i = 4 ] && break; printf '%s' $i; done; echo"#),
        case("nested-break", r#"for i in 1 2; do for j in 1 2; do break 2; done; done; echo done"#),
        // -- functions ------------------------------------------------
        case("func-args", r#"f() { echo "$1-$2-$#"; }; f a b"#),
        case("func-return", r#"f() { return 3; }; f; echo $?"#),
        case("func-local", r#"x=out; f() { local x=in; g; }; g() { echo "$x"; }; f; echo "$x""#),
        case("func-recursion", r#"f() { [ "$1" = 0 ] && return; printf '%s' "$1"; f $(($1-1)); }; f 3; echo"#),
        case("func-in-subst", r#"f() { echo "$1"; }; echo "$(f inner)""#),
        // -- quoting --------------------------------------------------
        case("quote-forms", r#"echo 'a$b' "a$UNSET_ZZ b" a\ b"#),
        case("quote-mixed", r#"x=v; echo "$x"'lit'"$x""#),
        case("quote-ansi-c", r#"printf '%s' $'a\tb\101\x41\n'"#),
        case("quote-empty", r#"set -- "" a; echo "$#"; for w in "$@"; do echo "[$w]"; done"#),
        // -- redirection ----------------------------------------------
        case("redir-basic", r#"echo hi > f; cat f; echo more >> f; wc -l < f"#),
        case("redir-stderr", r#"{ echo out; echo err >&2; } 2>/dev/null"#),
        case("redir-both", r#"{ echo out; echo err >&2; } > all 2>&1; sort all"#),
        case("redir-heredoc", "cat <<EOF\nline $((1+1))\nEOF"),
        case("redir-heredoc-quoted", "cat <<'EOF'\nno $expansion\nEOF"),
        case("redir-heredoc-tabs", "cat <<-EOF\n\tstripped\nEOF"),
        case("redir-herestring", r#"cat <<< "here string""#),
        case("redir-fd", r#"exec 3> three; echo x >&3; exec 3>&-; cat three"#),
        // -- pipelines and subshells ----------------------------------
        case("pipeline", r#"printf 'b\na\n' | sort | head -1"#),
        // A half-line from a builtin has to reach fd 1 before the child
        // that writes to the same fd next.
        case("flush-order", r#"printf A; printf B | cat; printf C; echo"#),
        case("flush-order-stderr", r#"printf E >&2; printf F | cat; echo"#),
        case("pipeline-status", r#"false | true; echo $?; set -o pipefail; false | true; echo $?"#),
        case("subshell-scope", r#"x=1; (x=2); echo $x"#),
        case("subshell-exit", r#"(exit 4); echo $?"#),
        case("command-subst", r#"echo "$(printf a)$(printf b)""#),
        case("command-subst-backtick", "echo \"`printf a`\""),
        case("process-subst", r#"cat <(printf 'p\n')"#),
        case("group-command", r#"{ echo a; echo b; } | wc -l"#),
        // -- globbing -------------------------------------------------
        case("glob-star", r#": > ga; : > gb; printf '%s,' g*; echo"#),
        case("glob-class", r#": > g1; : > g2; printf '%s,' g[12]; echo"#),
        case("glob-question", r#": > gx; printf '%s,' g?; echo"#),
        case("glob-nomatch", r#"printf '%s,' zz_no_match*; echo"#),
        case("brace-list", r#"echo {a,b}{1,2}"#),
        case("brace-range", r#"echo {1..5} {a..e} {1..9..3}"#),
        case("brace-nested", r#"echo {a,b{1,2}}"#),
        case("tilde", r#"echo ~ | grep -c /"#),
        // -- builtins -------------------------------------------------
        case("echo-flags", r#"echo -n a; echo; echo -e 'a\tb'"#),
        case("printf-recycle", r#"printf '%s-%s\n' a b c d"#),
        case("printf-width", r#"printf '[%5s][%-5s][%05d]\n' a b 42"#),
        case("read-basic", r#"printf 'x y\n' | { read -r a b; echo "$a|$b"; }"#),
        case("read-ifs", r#"IFS=: read -r a b <<< 'x:y'; echo "$a|$b""#),
        case("read-array", r#"read -r -a arr <<< 'p q r'; echo "${arr[1]}" "${#arr[@]}""#),
        case("shift-set", r#"set -- a b c; shift; echo "$@"; set -- x; echo "$#""#),
        case("declare-i", r#"declare -i n; n=3+4; echo $n"#),
        case("declare-p", r#"x=1; declare -p x"#),
        case("export-env", r#"export EV=1; printenv EV"#),
        case("unset-var", r#"x=1; unset x; echo "[${x-gone}]""#),
        case("type-builtin", r#"type -t echo; type -t nosuchthing_zz; echo $?"#),
        case("getopts", r#"set -- -a -b val; while getopts "ab:" o; do echo "$o=${OPTARG-}"; done"#),
        case("trap-exit", r#"trap 'echo bye' EXIT; echo body"#),
        case("eval", r#"x=1; eval 'x=$((x+1))'; echo $x"#),
        case("source-file", "printf 'sourced=1\\n' > lib.sh; . ./lib.sh; echo $sourced"),
        // -- errors and status ----------------------------------------
        case("exit-status", r#"true; echo $?; false; echo $?"#),
        case("set-e", r#"set -e; (false; echo unreached); echo $?"#),
        case("set-u", r#"set -u; echo "${undefined_zz-ok}""#),
        case("and-or", r#"true && echo t; false || echo f; false && echo no; echo $?"#),
    ];

    // Cases bish does not match today, each with why. Asserted to
    // *still* diverge -- fixing one fails this test until its line is
    // removed, which is the only way a list like this stays true.
    const DIVERGENCES: &[(&str, &str)] = &[
        ("param-quoted-star", "roadmap 6: \"$*\" does not join on IFS"),
        ("array-star-join", "roadmap 6: \"${a[*]}\" does not join on IFS"),
        ("dbracket-pattern", "quoting the right of `[[ == ]]` should make it a literal, not a glob"),
        ("printf-octal-escape", "roadmap 8: \\nnn in a printf format is not decoded"),
        ("array-slice-length", "roadmap 3: ${a[@]:off:len} drops the length"),
        ("args-slice-length", "roadmap 3: ${@:off:len} drops the length"),
        ("shopt-nullglob", "roadmap 4: the globbing shopts are tracked and ignored"),
        ("shopt-dotglob", "roadmap 4: as above"),
        ("shopt-failglob", "roadmap 4: as above"),
        ("globstar", "roadmap 5: ** is not implemented"),
        ("time-keyword", "roadmap 7: `time` is not a reserved word here"),
        ("builtin-gaps", "roadmap 9: [[ -v ]], wait -n, kill -l SIG, exec -a, set -C"),
    ];

    // The cases the divergence list is about. Kept apart from `CASES`
    // so that list stays a description of what works.
    const PENDING: &[Case] = &[
        case("param-quoted-star", r#"set -- a b c; IFS=,; echo "$*"; echo "$@""#),
        case("array-star-join", r#"a=(x y); IFS=-; echo "${a[*]}""#),
        case("dbracket-pattern", r#"[[ abc == a* ]] && echo glob; [[ abc == "a*" ]] || echo literal"#),
        case("printf-octal-escape", r#"printf 'a\101b\n'"#),
        case("array-slice-length", r#"a=(1 2 3 4); echo "${a[@]:1:2}" "${a[@]:2}""#),
        case("args-slice-length", r#"set -- p q r s; echo "${@:2:2}""#),
        case("shopt-nullglob", r#"shopt -s nullglob; printf '%s,' zz_no_match*; echo"#),
        case("shopt-dotglob", r#": > .hidden; : > shown; shopt -s dotglob; printf '%s,' *; echo"#),
        case("shopt-failglob", r#"shopt -s failglob; printf '%s,' zz_no_match*; echo"#),
        case("globstar", r#"mkdir -p a/b; : > a/b/deep; shopt -s globstar; printf '%s,' **/deep; echo"#),
        case("time-keyword", r#"time true"#),
        case("builtin-gaps", r#"x=1; [[ -v x ]] && echo v; kill -l 9"#),
    ];

    // `target/<profile>/bish`, worked out from the test binary's own
    // path (`target/<profile>/deps/bish-<hash>`). `cargo test` builds
    // the binary too, so it is there and current -- but the test skips
    // rather than fails if it is not, the same as when there is no
    // bash.
    fn bish_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.parent()?.join("bish");
        path.exists().then_some(path)
    }

    fn have_bash() -> bool {
        Command::new("bash").arg("-c").arg(":").status().is_ok_and(|s| s.success())
    }

    // Both shells get the same empty HOME and the same scratch
    // directory, so neither reads a config file and neither sees the
    // other's leftovers.
    fn run(shell: &std::ffi::OsStr, script: &str, dir: &std::path::Path) -> String {
        let out = Command::new(shell)
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("LC_ALL", "C")
            .env("HOME", dir)
            .env("PS1", "$ ")
            .output();
        match out {
            Ok(out) => {
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                normalise(&text)
            }
            Err(e) => format!("<could not run: {e}>"),
        }
    }

    // Each shell names itself, and bash adds a line number, when it
    // reports a problem. That is not a difference worth failing over --
    // what a case about an error is asking is whether the error
    // happened and what it said about the input.
    fn normalise(text: &str) -> String {
        text.lines()
            .map(|line| {
                let rest = match line.strip_prefix("bash: ").or_else(|| line.strip_prefix("bish: ")) {
                    Some(rest) => rest,
                    None => return line.to_string(),
                };
                let rest = rest.strip_prefix("line ").and_then(|r| r.split_once(": ")).map(|(_, r)| r).unwrap_or(rest);
                format!("<shell>: {rest}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn compare(cases: &[Case], bish: &std::path::Path) -> Vec<(&'static str, String, String)> {
        let root = std::env::temp_dir().join(format!("bish-bashdiff-{}", std::process::id()));
        let mut differing = Vec::new();
        for (i, case) in cases.iter().enumerate() {
            // A directory per case: several of them create files, and
            // a glob is only predictable in a directory it owns.
            let dir = root.join(format!("{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let want = run(std::ffi::OsStr::new("bash"), case.script, &dir);
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let got = run(bish.as_os_str(), case.script, &dir);
            if want != got {
                differing.push((case.name, want, got));
            }
        }
        std::fs::remove_dir_all(&root).ok();
        differing
    }

    #[test]
    fn bish_agrees_with_bash() {
        let Some(bish) = bish_binary() else { return };
        if !have_bash() {
            return;
        }
        let differing = compare(CASES, &bish);
        assert!(
            differing.is_empty(),
            "{} of {} cases differ from bash:\n{}",
            differing.len(),
            CASES.len(),
            differing
                .iter()
                .map(|(name, want, got)| format!("  {name}\n    bash: {want:?}\n    bish: {got:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_known_divergences_are_still_divergences() {
        let Some(bish) = bish_binary() else { return };
        if !have_bash() {
            return;
        }
        let differing: Vec<&str> = compare(PENDING, &bish).into_iter().map(|(name, _, _)| name).collect();
        for (name, why) in DIVERGENCES {
            assert!(
                differing.contains(name),
                "`{name}` matches bash now -- remove its line from DIVERGENCES ({why})"
            );
        }
        let unlisted: Vec<&str> = differing.iter().filter(|n| !DIVERGENCES.iter().any(|(d, _)| d == *n)).copied().collect();
        assert!(unlisted.is_empty(), "differing but not listed: {unlisted:?}");
    }

    #[test]
    fn every_pending_case_has_a_reason_and_the_other_way_round() {
        let named: Vec<&str> = PENDING.iter().map(|c| c.name).collect();
        for (name, _) in DIVERGENCES {
            assert!(named.contains(name), "DIVERGENCES names `{name}`, which is not in PENDING");
        }
        for case in PENDING {
            assert!(DIVERGENCES.iter().any(|(n, _)| *n == case.name), "PENDING has `{}` with no reason", case.name);
        }
    }
}
