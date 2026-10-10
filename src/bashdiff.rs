// Snippets that bash and bish should agree on, run through both and diffed.
//
// `CASES` agree. `DIVERGENCES` don't, each with its reason, and every entry
// is asserted to still diverge, so fixing one fails this test until its
// entry is removed.
//
// bash runs with `LC_ALL=C` (a Finnish `printf %.2f` writes `3,14`), and
// both shells get an empty `HOME` and a scratch cwd.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    #[derive(Clone, Copy)]
    struct Case {
        name: &'static str,
        script: &'static str,
    }

    const fn case(name: &'static str, script: &'static str) -> Case {
        Case { name, script }
    }

    // No `$RANDOM`, timestamps or pids: a case has to answer the same way
    // twice.
    const CASES: &[Case] = &[
        // Expansion
        case("param-default", r#"unset u; echo "${u:-d}" "${u-d}" "${u:+s}""#),
        case("param-assign-default", r#"unset u; echo "${u:=set}" "$u""#),
        case("param-length", r#"x=abcd; echo "${#x}""#),
        case("param-case", r#"x=abc; echo "${x^}" "${x^^}" "${x,,}""#),
        case("param-substring", r#"x=abcdef; echo "${x:1:3}" "${x: -2}" "${x:2}""#),
        case("param-strip", r#"p=a/b/c.txt; echo "${p##*/}" "${p%.*}" "${p#*/}" "${p%%/*}""#),
        case("param-subst", r#"x=aXbXc; echo "${x/X/-}" "${x//X/-}" "${x/#a/A}" "${x/%c/C}""#),
        case("param-indirect", r#"v=hello; n=v; echo "${!n}""#),
        case("param-at-split", r#"set -- "a b" c; for w in "$@"; do echo "[$w]"; done"#),
        // A list joins on IFS's first character; unquoted, the join is
        // split again.
        case("ifs-join-star", r#"set -- a b c; IFS=-; printf '[%s]' "$*"; echo"#),
        case("ifs-join-array", r#"a=(x y z); IFS=-; printf '[%s]' "${a[*]}"; echo"#),
        case("ifs-join-unquoted", r#"set -- a b c; IFS=-; printf '[%s]' $*; printf '[%s]' $@; echo"#),
        case("ifs-join-empty", r#"set -- a b c; IFS=; printf '[%s]' "$*"; echo"#),
        case("ifs-join-multichar", r#"set -- a b c; IFS=xy; printf '[%s]' "$*"; echo"#),
        case("ifs-join-keys", r#"aa_1=; aa_2=; set -- ${!aa_*}; printf '%s' "$#""#),
        case("param-nested", r#"x=abc; echo "${x/$(printf b)/Z}""#),
        // Arrays
        case("array-basic", r#"a=(1 2 3); echo "${a[0]}" "${a[@]}" "${#a[@]}" "${!a[@]}""#),
        case("array-append", r#"a=(1); a+=(2 3); echo "${a[@]}""#),
        case("array-negative", r#"a=(1 2 3); echo "${a[-1]}""#),
        // `:off:len` counts elements for a list and characters for a string.
        case("array-slice", r#"a=(z1 z2 z3 z4 z5); echo "${a[@]:1:2}" "${a[@]:2}" "${a[@]:0:2}" "${a[@]: -2}" "${a[@]:9}""#),
        case("array-slice-unquoted", r#"a=(z1 z2 z3); printf '[%s]' ${a[@]:1:2}; echo"#),
        case("array-slice-sparse", r#"b=(x y); b[5]=z; printf '[%s]' "${b[@]:1:2}"; echo"#),
        case("array-slice-star", r#"a=(z1 z2 z3); IFS=-; echo "${a[*]:1:2}""#),
        case("args-slice", r#"set -- p1 p2 p3 p4; printf '[%s]' "${@:2:2}" "${@:2}" "${@: -1}"; echo"#),
        case("args-slice-zero", r#"set -- p1 p2; printf '[%s]' "${@:1:1}"; echo"#),
        case("string-slice-negative-length", r#"x=abcdef; echo "${x:1:-1}""#),
        case("array-unset-elem", r#"a=(1 2 3); unset 'a[1]'; echo "${a[@]}" "${#a[@]}""#),
        case("assoc-basic", r#"declare -A m=([k]=v [j]=w); echo "${m[k]}" "${#m[@]}""#),
        // Arithmetic
        case("arith-ops", r#"echo $((1+2*3)) $((7/2)) $((7%3)) $((2**10)) $((1<<4))"#),
        case("arith-base", r#"echo $((16#ff)) $((2#1011)) $((8#17))"#),
        case("arith-ternary-comma", r#"echo $((1?2:3)) $((1,2,3))"#),
        case("arith-incr", r#"i=1; echo $((i++)) $i $((++i)) $i"#),
        case("arith-compare", r#"echo $((3>2)) $((3<2)) $((3==3)) $((1&&0)) $((1||0))"#),
        case("arith-assign-ops", r#"i=10; ((i+=5)); ((i*=2)); echo $i"#),
        case("arith-let", r#"let 'a = 3 + 4'; echo $a"#),
        // Conditionals
        case("test-string", r#"[ a = a ] && echo eq; [ -n "" ] || echo empty; [ -z "" ] && echo z"#),
        case("test-numeric", r#"[ 3 -gt 2 ] && [ 2 -le 2 ] && echo num"#),
        case("dbracket-regex", r#"[[ ab12 =~ ([a-z]+)([0-9]+) ]] && echo "${BASH_REMATCH[1]}-${BASH_REMATCH[2]}""#),
        case("dbracket-logic", r#"[[ a == a && b == b ]] && echo both; [[ a == b || c == c ]] && echo either"#),
        case("dbracket-lt", r#"[[ a < b ]] && echo lt"#),
        // Control flow
        case("if-elif-else", r#"if false; then echo a; elif true; then echo b; else echo c; fi"#),
        case("for-in", r#"for i in 1 2 3; do printf '%s' "$i"; done; echo"#),
        case("for-cstyle", r#"for ((i=0;i<3;i++)); do printf '%s' "$i"; done; echo"#),
        case("while-until", r#"i=0; while [ $i -lt 2 ]; do i=$((i+1)); done; until [ $i -ge 4 ]; do i=$((i+1)); done; echo $i"#),
        case("case-patterns", r#"for x in ab cd zz; do case $x in a*) echo A;; c?) echo C;; *) echo other;; esac; done"#),
        case("case-fallthrough", r#"case ab in a*) echo one;;& *b) echo two;; esac"#),
        case("break-continue", r#"for i in 1 2 3 4; do [ $i = 2 ] && continue; [ $i = 4 ] && break; printf '%s' $i; done; echo"#),
        case("nested-break", r#"for i in 1 2; do for j in 1 2; do break 2; done; done; echo done"#),
        // Functions
        case("func-args", r#"f() { echo "$1-$2-$#"; }; f a b"#),
        case("func-return", r#"f() { return 3; }; f; echo $?"#),
        case("func-local", r#"x=out; f() { local x=in; g; }; g() { echo "$x"; }; f; echo "$x""#),
        case("func-recursion", r#"f() { [ "$1" = 0 ] && return; printf '%s' "$1"; f $(($1-1)); }; f 3; echo"#),
        case("func-in-subst", r#"f() { echo "$1"; }; echo "$(f inner)""#),
        // Quoting
        case("quote-forms", r#"echo 'a$b' "a$UNSET_ZZ b" a\ b"#),
        case("quote-mixed", r#"x=v; echo "$x"'lit'"$x""#),
        case("quote-ansi-c", r#"printf '%s' $'a\tb\101\x41\n'"#),
        // `$'...'` is quoting, so its result isn't globbed.
        case("quote-ansi-c-does-not-glob", r#"touch afile bfile; echo $'*'; echo $'a*'"#),
        case("quote-ansi-c-keeps-its-spaces-as-one-word", r#"printf '[%s]' $'a b'; echo"#),
        case("quote-empty", r#"set -- "" a; echo "$#"; for w in "$@"; do echo "[$w]"; done"#),
        // Redirection
        case("redir-basic", r#"echo hi > f; cat f; echo more >> f; wc -l < f"#),
        case("redir-stderr", r#"{ echo out; echo err >&2; } 2>/dev/null"#),
        case("redir-both", r#"{ echo out; echo err >&2; } > all 2>&1; sort all"#),
        case("redir-heredoc", "cat <<EOF\nline $((1+1))\nEOF"),
        case("redir-heredoc-quoted", "cat <<'EOF'\nno $expansion\nEOF"),
        case("redir-heredoc-tabs", "cat <<-EOF\n\tstripped\nEOF"),
        // `-ab` is two options in one word. OPTIND names the word, so the
        // position within it is state the script can't see.
        case(
            "getopts-clustered-options",
            r#"set -- -ab v x; while getopts ab: o; do echo "[$o:$OPTARG] OPTIND=$OPTIND"; done; echo "end=$OPTIND rest=${!OPTIND}""#,
        ),
        // `$_` is the last word of the previous command, or its name when
        // it had no arguments. Its initial value isn't tested: bash sets it
        // from argv[0], and the two shells are invoked by different names.
        case("underscore-is-the-last-argument", r#"true xyz; echo "[$_]""#),
        case("underscore-is-the-name-when-there-are-no-arguments", r#"true; echo "[$_]"; :; echo "[$_]""#),
        case("underscore-after-an-assignment-only-command", r#"true zz; x=1; echo "[$_]""#),
        case("underscore-is-the-expanded-word", r#"v=q; true "$v"; touch zz1; true zz*; echo "[$_]""#),
        // Set after the command, so a function body sees its caller's `$_`.
        case("underscore-inside-a-function-is-the-callers", r#"f(){ echo "[$_]"; }; true zz; f arg"#),
        case("underscore-after-a-function-is-the-calls-own", r#"f(){ true inner; }; true zz; f; echo "[$_]""#),
        case("underscore-through-nested-functions", r#"g(){ true deep; }; f(){ g inner; echo "[$_]"; }; true zz; f arg"#),
        case("underscore-after-a-compound", r#"if true zz; then :; fi; echo "[$_]"; for i in 1; do true loop; done; echo "[$_]""#),
        case("underscore-is-untouched-by-a-pipeline", r#"true zz; echo a | cat > /dev/null; true ww; echo "[$_]""#),
        case("underscore-is-untouched-by-a-subshell", r#"true zz; ( true inner ); true ww; echo "[$_]""#),
        case("underscore-inside-a-command-substitution", r#"x=$(true qq; echo "[$_]"); echo "$x""#),
        case("underscore-survives-being-unset", r#"unset _; true zz; echo "[$_]""#),
        case("arith-rejects-a-trailing-token", r#"echo $((1 2)); echo rc=$?"#),
        case("arith-rejects-a-name-then-a-number", r#"echo $((sqrt 3)); echo rc=$?"#),
        case("arith-in-a-for-header-rejects-it-too", r#"for ((i=1 2; i<3; i++)); do :; done"#),
        // Diagnostics, in bash's wording.
        case("arith-division-by-zero", r#"x=$((1/0)); echo rc=$?"#),
        case("arith-division-by-zero-in-a-command", r#"((1/0)); echo rc=$?"#),
        case("arith-modulo-by-zero", r#"echo $((1 % 0))"#),
        case("arith-assigning-division-by-zero", r#"echo $((a/=0))"#),
        case("arith-negative-exponent", r#"echo $((1**-1))"#),
        case("arith-ternary-without-a-colon", r#"echo $((1?2))"#),
        case("arith-a-character-that-is-not-arithmetic", r#"echo $((@))"#),
        case("arith-operand-expected", r#"echo $((1&&)); echo $((1+))"#),
        case("arith-increment-on-a-literal-is-two-signs", r#"echo $((++5)) $((--5))"#),
        // A subscript that won't evaluate fails like an expansion that won't.
        case("arith-a-subscript-that-does-not-evaluate", r#"a=(1); echo ${a[1/0]}; echo rc=$?"#),
        case("arith-a-subscript-with-a-trailing-token", r#"a=(1); echo ${a[1 2]}; echo rc=$?"#),
        // POSIX `test` groups with `\( ... \)`. An unknown operator is an
        // error, not false.
        case("test-groups-with-parentheses", r#"[ \( 1 -eq 1 \) -a \( 2 -eq 2 \) ] && echo y"#),
        case("test-groups-with-or", r#"[ \( 1 -eq 2 \) -o \( 2 -eq 2 \) ] && echo y"#),
        case("test-a-negated-group", r#"[ ! \( 1 -eq 1 -o 2 -eq 2 \) ]; echo rc=$?"#),
        case("test-nested-groups", r#"[ \( \( 1 -eq 1 \) -a 1 \) -o 0 ] && echo y"#),
        case("test-a-group-of-one-word", r#"[ \( x \) ] && echo y; [ ! \( x \) ]; echo rc=$?"#),
        // Three words with an operator in the middle are a comparison, not
        // a group.
        case("test-a-parenthesis-is-still-a-string", r#"[ "(" = "(" ] && echo y; [ "(" = ")" ]; echo rc=$?"#),
        case("test-an-unknown-operator-is-an-error", r#"[ x y ]; echo rc=$?"#),
        case("test-an-unknown-dash-operator-is-an-error", r#"[ -q x ]; echo rc=$?"#),
        case("test-unary-o-is-not-a-connective", r#"[ -o errexit -o -o xtrace ]; echo rc=$?"#),
        case("test-file-tests-in-a-group", r#"touch g; [ \( -e g -a -f g \) -o -d g ] && echo y"#),
        // `return` ends a sourced file, and `.` returns its status.
        case("return-from-a-sourced-file", "printf 'echo in\\nreturn 4\\necho no\\n' > s.sh; . ./s.sh; echo rc=$?"),
        case("return-from-a-sourced-file-with-no-argument", "printf 'false\\nreturn\\n' > s.sh; . ./s.sh; echo rc=$?"),
        case(
            "return-from-a-function-inside-a-sourced-file",
            "printf 'g(){ return 7; }\\ng\\necho \"in=$?\"\\nreturn 9\\n' > s.sh; . ./s.sh; echo rc=$?",
        ),
        case("return-through-a-nested-source", "printf 'return 6\\n' > b.sh; printf '. ./b.sh\\necho mid=$?\\n' > a.sh; . ./a.sh; echo rc=$?"),
        case("return-at-the-top-level-is-a-usage-error", r#"return 3; echo rc=$?"#),
        // A `#` inside a word is a character, not a comment.
        case("a-hash-in-an-expansion-word", r#"echo "[${x:-# hi}]" "[${x:-#}]""#),
        case("a-hash-in-ps4", r#"PS4='# '; set -x; echo a"#),
        case("a-hash-still-starts-a-comment", r#"echo a # comment"#),
        case("a-hash-in-the-middle-of-a-word", r#"echo a#b"#),
        // Sorted: each stage's child prints its own trace line, so the
        // order is up to the scheduler in both shells.
        case("xtrace-traces-every-stage", r#"{ set -x; /bin/echo a | /bin/cat; set +x; } 2>t; sort t"#),
        case("xtrace-traces-three-stages", r#"{ set -x; /bin/echo a | /bin/cat | /bin/cat; set +x; } 2>t; sort t"#),
        case("xtrace-traces-a-stage-running-in-this-shell", r#"{ set -x; echo a | cat; set +x; } 2>t; sort t"#),
        case("xtrace-traces-a-stages-prefix-assignment", r#"{ set -x; x=1 /bin/cat /dev/null | /bin/cat; set +x; } 2>t; sort t"#),
        case("xtrace-traces-an-appended-prefix-assignment", r#"p=x; { set -x; p+=y /bin/cat /dev/null | /bin/cat; set +x; } 2>t; sort t"#),
        case("getopts-cluster-with-a-glued-argument", r#"set -- -abv y; while getopts ab: o; do echo "[$o:$OPTARG]"; done; echo end=$OPTIND"#),
        case(
            "getopts-unknown-inside-a-cluster",
            r#"set -- -aZb; while getopts ab: o; do echo "[$o:$OPTARG] OPTIND=$OPTIND"; done; echo end=$OPTIND"#,
        ),
        case("getopts-silent-mode-inside-a-cluster", r#"set -- -aZ; while getopts :ab: o; do echo "[$o:$OPTARG]"; done"#),
        // Assigning OPTIND restarts the word, even when the value
        // assigned is the one already there.
        case(
            "getopts-assigning-optind-restarts-the-word",
            r#"set -- -ab v; getopts ab: o; echo "1:[$o] $OPTIND"; OPTIND=1; getopts ab: o; echo "2:[$o] $OPTIND""#,
        ),
        case("getopts-starts-at-one", r#"echo "[$OPTIND][$OPTERR]""#),
        case("getopts-a-lone-dash-is-not-an-option", r#"set -- - x; while getopts ab: o; do echo "[$o]"; done; echo $OPTIND"#),
        case(
            "getopts-in-a-function-with-a-local-optind",
            r#"f(){ local OPTIND=1; while getopts ab: o; do echo "[$o:$OPTARG]"; done; }; f -ab v; f -a"#,
        ),
        // A stage that won't start exits 127 without ending the pipeline.
        // Only its status and the last stage's are asserted: whether the
        // stage upstream of it finishes first is a race in both shells.
        case("a-missing-command-does-not-end-the-pipeline", r#"nosuchcmd | head -1; echo "ps=(${PIPESTATUS[@]}) q=$?""#),
        case("a-missing-command-later-in-a-pipeline", r#"echo x | nosuchcmd; echo "q=$? it=${PIPESTATUS[1]}""#),
        case("a-directory-as-a-pipeline-stage", r#"/etc | cat; echo "q=${PIPESTATUS[0]}""#),
        // The DEBUG trap fires for every stage before any of them runs. A
        // compound stage is a subshell, which doesn't inherit the trap, so
        // nothing inside it traces.
        case("debug-trap-fires-for-every-pipeline-stage", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; /bin/echo a | /bin/cat | /bin/cat"),
        case("debug-trap-skips-a-compound-stage", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; { echo g; } | cat"),
        case("debug-trap-does-not-feed-the-pipe", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; echo a | { read v; echo \"got $v\"; }"),
        case("debug-trap-with-functrace-enters-a-subshell", "set -T; trap 'echo \"[$BASH_COMMAND]\"' DEBUG; echo a | { read v; echo \"got $v\"; }"),
        // A loop or `case` header is a command too, for `for`, `select` and
        // `case` but not `if` or `while`, whose conditions fire on their
        // own. The `for` header fires once per iteration, and not at all
        // for an empty list.
        case("debug-trap-fires-for-a-for-header", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; for i in a b; do :; done"),
        case("debug-trap-skips-an-empty-for", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; for i in; do :; done; echo end"),
        case("debug-trap-names-the-positional-for", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; set -- z; for i; do echo $i; done"),
        case("debug-trap-fires-for-a-case-header", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; case x in x) echo m;; esac"),
        case("debug-trap-fires-for-a-select-header", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; select x in a b; do break; done < /dev/null"),
        // Each section of an arithmetic `for` is its own command; an
        // omitted one is named `((1))`.
        case("debug-trap-fires-for-each-arithmetic-section", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; for ((i=0;i<2;i++)); do :; done"),
        case("debug-trap-names-an-omitted-section-as-one", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; for ((;;)); do break; done"),
        // Interval expressions: `a{2}`.
        case("regex-interval-exact", r#"re="^a{2}$"; for v in a aa aaa; do [[ $v =~ $re ]] && echo "$v y" || echo "$v n"; done"#),
        case("regex-interval-range", r#"re="^a{2,3}$"; for v in a aa aaa aaaa; do [[ $v =~ $re ]] && echo "$v y" || echo "$v n"; done"#),
        case("regex-interval-open-ended", r#"re="^a{2,}$"; for v in a aa aaaa; do [[ $v =~ $re ]] && echo "$v y" || echo "$v n"; done"#),
        case("regex-interval-on-a-group", r#"re="^(ab){2}$"; for v in ab abab ababab; do [[ $v =~ $re ]] && echo "$v y" || echo "$v n"; done"#),
        case("regex-interval-on-a-class", r#"re="^[0-9]{3}$"; for v in 12 123 1234; do [[ $v =~ $re ]] && echo "$v y" || echo "$v n"; done"#),
        case("regex-interval-captures", r#"re="^(a{2})(b{1,2})$"; [[ aabb =~ $re ]] && echo "${BASH_REMATCH[1]}-${BASH_REMATCH[2]}""#),
        // `exit` in a trap handler ends the script there.
        case("trap-exit-ends-the-script", r#"trap 'echo TRAP; exit 7' TERM; kill -TERM $$; echo NOTREACHED"#),
        case("trap-exit-skips-the-statements-after-it", r#"trap 'exit 3' TERM; kill -TERM $$; for i in 1 2 3; do echo $i; done; echo NOTREACHED"#),
        case("trap-without-exit-carries-on", r#"trap 'echo TRAP' TERM; kill -TERM $$; echo AFTER"#),
        // `$$` is the shell that started, even in a subshell; `BASHPID` is
        // the current process. bish re-execs where bash forks, so a
        // background subshell has to be handed the value.
        case("dollar-dollar-is-the-same-in-a-background-subshell", r#"main=$$; ( [ "$$" = "$main" ] && echo same || echo differs ) & wait"#),
        case("dollar-dollar-is-the-same-in-a-plain-subshell", r#"main=$$; ( [ "$$" = "$main" ] && echo same || echo differs )"#),
        // A negative target is a process group; only the first argument may
        // be a signal.
        case("kill-takes-a-process-group-as-a-target", r#"kill -TERM -999999 2>&1 | sed 's/.*kill: //'; kill -9 -999999 2>&1 | sed 's/.*kill: //'"#),
        case("kill-still-reads-a-leading-number-as-a-signal", r#"kill -999999 2>&1 | sed 's/.*kill: //'"#),
        case("kill-after-a-target-a-negative-is-another-target", r#"kill 999999 -888888 2>&1 | sed 's/.*kill: //'"#),
        // POSIX lets a `case` arm open with `(`.
        case("case-arm-may-open-with-a-paren", r#"case abc in (a*) echo one;; (*) echo other;; esac"#),
        case("case-arm-paren-with-alternation", r#"for v in ab cd ef; do case $v in (a*|c*) echo "$v yes";; (*) echo "$v no";; esac; done"#),
        case("case-arm-paren-around-a-quoted-pattern", r#"case 'x|y' in ("x|y") echo quoted;; (*) echo other;; esac"#),
        // KILL and STOP can't be trapped, but they can be sent.
        case(
            "kill-accepts-the-uncatchable-signals-by-name",
            r#"for s in KILL SIGKILL STOP TERM; do kill -$s 2>&1 >/dev/null | grep -q 'invalid signal' && echo "$s rejected" || echo "$s ok"; done"#,
        ),
        // A brace that doesn't open a well-formed interval is an ordinary
        // character, which `awk '{print}'` patterns depend on.
        case("regex-a-lone-brace-is-a-character", r#"[[ "a{" =~ ^a\{$ ]] && echo y || echo n; [[ "a{,}" =~ ^a{,}$ ]] && echo y || echo n"#),
        // Which match is found: POSIX takes the longest at the leftmost
        // position, where a backtracker takes the first its branch order
        // reaches. `(a|ab)` against `abc` shows the difference.
        case(
            "regex-takes-the-longest-match-not-the-first-branch",
            r#"re='(a|ab)'; [[ abc =~ $re ]]; echo "[${BASH_REMATCH[0]}]"; re='x|xy'; [[ xy =~ $re ]]; echo "[${BASH_REMATCH[0]}]"; re='ab|abc'; [[ abc =~ $re ]]; echo "[${BASH_REMATCH[0]}]""#,
        ),
        case(
            "regex-an-empty-branch-does-not-win-over-a-longer-one",
            r#"re='(|a)'; [[ abc =~ $re ]]; echo "[${BASH_REMATCH[0]}][${BASH_REMATCH[1]}]"; re='(a|)x'; [[ ax =~ $re ]]; echo "[${BASH_REMATCH[0]}][${BASH_REMATCH[1]}]""#,
        ),
        // Leftmost still outranks longest: a shorter match that starts
        // earlier beats a longer one that starts later.
        case(
            "regex-leftmost-outranks-longest",
            r#"re='b|abc'; [[ xabc =~ $re ]]; echo "[${BASH_REMATCH[0]}]"; re='a|bcd'; [[ abcd =~ $re ]]; echo "[${BASH_REMATCH[0]}]""#,
        ),
        // Escapes that mean something other than the character after the
        // backslash.
        case(
            "regex-word-boundaries",
            r#"for re in '\bbar' 'foo\b' '\Bar' '\<bar' 'bar\>'; do [[ "foo bar" =~ $re ]] && echo "$re [${BASH_REMATCH[0]}]" || echo "$re no"; done"#,
        ),
        case(
            "regex-shorthand-classes",
            r#"re='\w+'; [[ 'a_1 %' =~ $re ]]; echo "[${BASH_REMATCH[0]}]"; re='\S+'; [[ '  xy' =~ $re ]]; echo "[${BASH_REMATCH[0]}]"; re='\s'; [[ 'ab cd' =~ $re ]]; echo "[${BASH_REMATCH[0]}]""#,
        ),
        // `\d` is Perl's, and glibc doesn't have it, so bash reads a
        // literal `d`.
        case(
            "regex-a-perl-shorthand-is-a-literal",
            r#"re='\d+'; [[ ab123 =~ $re ]] && echo "digits [${BASH_REMATCH[0]}]" || echo none; [[ add =~ $re ]] && echo "letters [${BASH_REMATCH[0]}]""#,
        ),
        case(
            "regex-the-ends-of-the-text",
            r#"re='\`abc'; [[ abc =~ $re ]] && echo start; re="abc\\'"; [[ abc =~ $re ]] && echo end; re='\`bc'; [[ abc =~ $re ]] && echo bad || echo anchored"#,
        ),
        case(
            "regex-a-nested-quantifier-does-not-hang",
            r#"t=$(printf 'a%.0s' {1..400}); re='^(a+)+$'; [[ $t =~ $re ]] && echo "y ${#BASH_REMATCH[0]}"; [[ ${t}b =~ $re ]] && echo bad || echo "n"; re='(a|aa)*b'; [[ $t =~ $re ]] && echo bad || echo n"#,
        ),
        // A redirect is performed even for a builtin that never reads it,
        // in order, and the first that won't open fails the command before
        // it runs.
        case("a-builtin-fails-on-an-input-it-never-reads", r#"echo z <nosuch; echo rc=$?; : <nosuch; echo rc=$?"#),
        case("a-numbered-input-redirect-on-a-builtin", r#"echo z 3<nosuch; echo rc=$?"#),
        case("redirects-are-performed-in-order", r#"echo z <nosuch >out; echo rc=$?; ls out 2>&1 | head -1"#),
        case("a-builtin-still-reads-what-it-can-open", r#"printf x > have; echo z <have; echo rc=$?; printf "L\n" > f2; read v <f2; echo "[$v]""#),
        // Opening the target to check it must not consume it: a process
        // substitution is a pipe with a producer already writing.
        case(
            "checking-an-input-does-not-consume-a-process-substitution",
            r#"read v < <(echo hi); echo "[$v]"; mapfile -t a < <(printf "1\n2\n"); echo "${#a[@]}""#,
        ),
        // Job-control flags: `jobs -r` and `-s`, `disown -h`.
        case("jobs-r-lists-only-the-running", r#"sleep 0.3 & jobs -r | wc -l; jobs -s | wc -l; jobs | wc -l; wait"#),
        case("jobs-narrowed-after-a-job-finished", r#"true & wait; jobs -s | wc -l; jobs -r | wc -l"#),
        case("disown-h-keeps-the-job", r#"sleep 0.2 & disown -h %1; jobs | wc -l; wait 2>/dev/null; echo done"#),
        case("disown-h-with-no-spec", r#"sleep 0.2 & disown -h; jobs | wc -l; wait 2>/dev/null; echo done"#),
        case("kill-l-lists-the-real-time-signals", r#"kill -l | wc -w; kill -l | tr -s " \t\n" "\n" | tail -4 | tr "\n" " ""#),
        case("kill-l-names-the-real-time-range", r#"kill -l RTMIN; kill -l RTMAX; kill -l SIGRTMIN+1; kill -l RTMAX-14"#),
        case("kill-l-numbers-the-real-time-range", r#"kill -l 34; kill -l 49; kill -l 50; kill -l 64"#),
        case("a-real-time-signal-can-be-trapped", r#"trap "echo caught" RTMIN; kill -RTMIN $$; sleep 0.1; echo done"#),
        case("a-real-time-signal-by-offset", r#"trap "echo caught" SIGRTMIN+3; kill -s RTMIN+3 $$; sleep 0.1; echo done"#),
        case("a-signal-above-thirty-two-by-number", r#"trap "echo n" 40; kill -40 $$; sleep 0.1; echo done"#),
        case("sigstkflt-is-a-signal", r#"kill -l 16; trap "echo s" 16; kill -16 $$; sleep 0.1; echo done"#),
        // `$BASH_COMMAND` is the command being run, as written, so a
        // command that reads it sees itself.
        case("bash-command-names-the-command-being-run", r#"true; echo "[$BASH_COMMAND]""#),
        case("bash-command-is-the-unexpanded-text", "v=q; trap 'echo \"[$BASH_COMMAND]\"' DEBUG; echo \"$v\" a*"),
        case("bash-command-in-an-err-trap", "trap 'echo \"err=[$BASH_COMMAND]\"' ERR; false"),
        case("bash-command-names-a-redirect", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; echo z > /dev/null; echo z 2>&1 >>f; echo z >&2"),
        case("bash-command-keeps-backticks", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; echo `true`"),
        // A trap's own body doesn't change the command the trap is about.
        case("bash-command-survives-the-traps-own-body", "trap 'echo \"[$BASH_COMMAND]\"' DEBUG; true one; true two"),
        // The DEBUG trap fires before the command's expansions.
        case("debug-trap-fires-before-expansion", "trap \"echo TRAP >&2\" DEBUG; echo $(echo SUB >&2)"),
        // A backslash-newline is a line continuation: both characters go.
        case("a-backslash-newline-is-removed", "echo a\\\nb"),
        case("a-backslash-newline-inside-double-quotes", "echo \"a\\\nb\""),
        case("a-backslash-newline-in-an-assignment", "x=a\\\nb; echo \"[$x]\""),
        case("a-backslash-newline-between-words", "echo one \\\ntwo"),
        case("a-backslash-newline-before-a-command", "if true; then \\\necho t; fi"),
        case("a-backslash-newline-is-literal-in-single-quotes", "echo 'a\\\nb'"),
        // The usual form in config files: the continued line is indented. The
        // indent must not become an empty argument.
        case("a-backslash-newline-before-an-indented-word", "printf '[%s]\\n' a \\\n    b \\\n    c"),
        case("a-backslash-newline-then-a-tab", "printf '[%s]\\n' a \\\n\tb"),
        case("a-trailing-backslash-newline-adds-no-word", "printf '[%s]\\n' a \\\n"),
        // A status is the low byte: `exit -1` is 255 and `exit 300` is 44.
        case("exit-status-is-one-byte", r#"(exit -1); echo $?; (exit 300); echo $?; (exit 256); echo $?"#),
        case("return-status-is-one-byte", r#"f(){ return -1; }; f; echo $?; g(){ return 300; }; g; echo $?"#),
        // `exit` with a non-numeric word reports it and carries on with
        // status 2 (bash 5.3, outside posix mode). Two numbers is an error
        // that still exits, with 1. `--` ends the options.
        case("exit-with-a-word-carries-on", r#"exit x 2>/dev/null; echo "rc=$?""#),
        case("exit-with-two-numbers-leaves-with-one", r#"( exit 5 2 ) 2>/dev/null; echo "rc=$?""#),
        case("exit-after-a-double-dash", r#"( exit -- 5 ); echo "rc=$?""#),
        // The EXIT trap sees the status being exited with, not the previous
        // command's.
        case("the-exit-trap-sees-the-status-being-left-with", r#"trap 'echo "rc=$?"' EXIT; true; exit 4"#),
        // A numbered-descriptor redirect on a construct keeps what the
        // construct does in this shell: assignments, `cd`, `exit`.
        case(
            "a-construct-with-a-numbered-fd-redirect-runs-in-this-shell",
            r#"{ x=1; } 3>f; while :; do y=2; break; done 3>g; f(){ z=3; }; f 4>h; mkdir -p d; { cd d; } 3>f2; echo "$x$y$z $(basename "$PWD")"; { exit 3; } 3>f; echo not-reached"#,
        ),
        // ...and what the construct moved is put back as it was, open or
        // closed.
        case(
            "a-numbered-fd-redirect-is-put-back-afterwards",
            r#"exec 4>g; { echo a >&4; } 4>f; echo b >&4; cat g f; exec 3>h; { echo in >&3; } 3>&- 2>/dev/null; echo out >&3; cat h"#,
        ),
        // `. /dev/stdin` reads this command's stdin, wherever that is.
        case("source-reads-a-here-string-through-dev-stdin", r#". /dev/stdin <<< 'echo sourced'; { . /dev/stdin; } <<< 'echo grouped'"#),
        // The shell, a command substitution and a process substitution all
        // restore what a group, loop, `if` or builtin redirected before the
        // EXIT trap runs. `"$BASH" -c` is whichever shell is under test.
        case(
            "the-exit-trap-runs-once-the-shells-redirects-are-undone",
            r#""$BASH" -c 'trap "echo bye rc=\$?" EXIT; { echo in; exit 4; } > f'; "$BASH" -c 'trap "echo bye" EXIT; eval exit > g'; x=$(trap 'echo sub' EXIT; { exit; } > h); cat <(trap 'echo psub' EXIT; { exit; } > p); echo "f=[$(cat f)] g=[$(cat g)] h=[$(cat h)] p=[$(cat p)] x=[$x]""#,
        ),
        // ...but a subshell bash would fork puts nothing back, and neither
        // does an `exit` from inside a function, wherever the redirect is.
        case(
            "a-forked-subshell-or-a-function-leaves-every-redirect-in-place",
            r#"( trap 'echo bye1' EXIT; { exit; } > g1 ); { trap 'echo bye2' EXIT; { exit; } > g2; } | cat; "$BASH" -c 'trap "echo bye3" EXIT; f(){ { exit; } > g3; }; f'; "$BASH" -c 'trap "echo bye4" EXIT; h(){ exit; }; { h; } > g4'; echo "g1=[$(cat g1)] g2=[$(cat g2)] g3=[$(cat g3)] g4=[$(cat g4)]""#,
        ),
        // A here-string or a heredoc on a descriptor of its own.
        case(
            "a-here-string-and-a-heredoc-on-numbered-descriptors",
            "{ read a <&3; read b <&4; echo \"$a/$b\"; } 3<<<one 4<<EOF\ntwo\nEOF\ncat 0<<<zero",
        ),
        // `-c` alone is not a login shell, so `logout` refuses and the
        // script carries on. The login side is LOGIN_CASES.
        case("logout-outside-a-login-shell", r#"logout 2>/dev/null; echo "rc=$?"; logout 5 2>/dev/null; echo "rc=$?"; shopt login_shell"#),
        // A prefix assignment is expanded once, for every kind of command.
        case("prefix-assignment-is-expanded-once", r#"x=$(echo A >&2; echo a) /bin/echo hi"#),
        // The words first, then the assignments: `B` before `A`.
        case("prefix-assignment-is-expanded-after-the-words", r#"x=$(echo A >&2; echo a) /bin/echo $(echo B >&2; echo b)"#),
        case("prefix-assignment-reaches-a-function", r#"f(){ echo "in=[$p]"; }; p=1 f; echo "after=[$p]""#),
        case("prefix-assignment-is-restored-after-a-function", r#"p=old; f(){ echo "in=[$p]"; }; p=new f; echo "after=[$p]""#),
        case("prefix-assignment-reaches-a-redirected-function", r#"f(){ echo "[$p]"; }; p=1 f > /dev/null; f"#),
        case("prefix-assignment-reaches-a-builtin", r#"IFS=: read a b <<< "x:y"; echo "[$a][$b]""#),
        // Traced before the command, in `=` form with the value the command
        // will see, so an appending prefix shows the result.
        case("xtrace-traces-a-prefix-assignment", r#"set -x; p=1 /bin/true; set +x"#),
        case("xtrace-traces-a-prefix-on-a-builtin", r#"set -x; p=1 read q < /dev/null; set +x"#),
        case("xtrace-traces-a-prefix-on-a-function", r#"f(){ :; }; set -x; p=1 f; set +x"#),
        case("xtrace-traces-an-appending-prefix", r#"p=x; set -x; p+=y /bin/true; set +x"#),
        // `declare -f` output is re-sourced and read by scripts, so its
        // layout follows bash's. A word is written back the way it was
        // spelled: `trap "echo t" EXIT` keeps its double quotes.
        case("declare-f-keeps-each-kind-of-quote", "f() { echo \"a\" 'b' c\\ d; }; declare -f f"),
        case("declare-f-keeps-one-pair-of-double-quotes", r#"f() { echo "x${y}z" "a$x b"; }; declare -f f"#),
        case("declare-f-escapes-inside-double-quotes", r#"f() { echo "a\"b" "a\\b" "\$x"; }; declare -f f"#),
        // An empty pair of quotes is a word, and `a""b` one word with a
        // pair in the middle; but `"${a[@]:9}"`, holding an expansion that
        // vanishes, vanishes with it.
        case("declare-f-keeps-an-empty-quoted-word", r#"f() { echo "" '' a""b ""a; }; declare -f f"#),
        case("an-empty-expansion-in-quotes-is-not-a-word", r#"a=(1); c(){ echo $#; }; c "x" "${a[@]:9}" "y"; c "" x"#),
        // `$'...'` is the exception: bash prints the resolved text as an
        // ordinary literal.
        case("declare-f-resolves-dollar-single-quotes", "f() { echo $'a\\tb'; }; declare -f f"),
        case("declare-f-keeps-backticks", r#"f() { echo `pwd` "`pwd`" $(pwd); }; declare -f f"#),
        // A heredoc comes back as a heredoc: body below the line, then a
        // blank one. `<<-` and a quoted delimiter change what the body
        // means, so both survive.
        case("declare-f-keeps-a-heredoc", "f() { cat <<EOF\nhi\nEOF\n}; declare -f f"),
        case("declare-f-keeps-a-quoted-heredoc-delimiter", "f() { cat <<'E'\n$x\nE\n}; declare -f f"),
        case("declare-f-keeps-a-tab-stripping-heredoc", "f() { cat <<-T\n\tx\nT\n}; declare -f f"),
        case("declare-f-a-heredoc-with-a-command-after-it", "f() { cat <<EOF\nhi\nEOF\necho after; }; declare -f f"),
        case("declare-f-a-heredoc-in-a-pipeline", "f() { cat <<EOF | wc -l\na\nEOF\n}; declare -f f"),
        // A dup keeps its operator (`<&` stays `<&`). The descriptor is
        // printed before a number always, and before a variable only when
        // it isn't the operator's default. A close is `>&-` however it was
        // written.
        case(
            "declare-f-writes-a-dup-back-the-way-it-was-written",
            r#"f() { read a <&3; cat 3<&0; echo x >&2; echo y 4>&1; read b <&-; echo z 3>&-; fd=3; read c <&$fd; echo w >&$fd; cat 3<&$fd; echo v 4>&$fd; read d 0<&$fd; }; declare -f f"#,
        ),
        // A heredoc on a group, or on the function itself, keeps its body
        // too.
        case(
            "declare-f-keeps-a-heredoc-on-a-group-and-on-a-function",
            "f() { { cat; } <<EOF\nhi\nEOF\n}\nh() { cat; } <<EOF\nx\nEOF\nf; h; declare -f f h",
        ),
        // A command inside a redirected group or function reads that
        // redirect, in a pane too.
        case("a-group-hands-its-stdin-redirect-to-the-command-inside", "echo data > f; { cat; } < f; k() { cat; } < f; k"),
        case("declare-f-two-heredocs-on-one-command", "f() { cat <<A <<B\n1\nA\n2\nB\n}; declare -f f"),
        case("declare-f-a-heredoc-inside-a-branch", "f() { if :; then cat <<E\nb\nE\nfi; }; declare -f f"),
        // The idiom this output exists for.
        case("declare-f-output-can-be-re-sourced", "f() { cat <<E\nz\nE\n}; sh -c \"$(declare -f f); f\""),
        case("declare-f-empty-body", r#"f() { :; }; declare -f f"#),
        case("declare-f-two-statements", r#"f() { echo a; echo b; }; declare -f f"#),
        case("declare-f-a-bare-variable-stays-bare", r#"f() { echo $x ${y}; }; declare -f f"#),
        case("declare-f-if-then-else", r#"f() { if :; then echo a; else echo b; fi; echo c; }; declare -f f"#),
        // bash doesn't print `elif`; it nests the second `if` in the first
        // one's `else`.
        case("declare-f-elif-becomes-a-nested-else", r#"f() { if :; then echo a; elif :; then echo b; fi; }; declare -f f"#),
        case("declare-f-a-condition-of-two-statements", r#"f() { if a; b; then echo t; fi; }; declare -f f"#),
        case("declare-f-while-keeps-do-on-its-line", r#"f() { while :; do break; done; }; declare -f f"#),
        case("declare-f-until", r#"f() { until :; do echo u; done; }; declare -f f"#),
        // ...where `for` doesn't, and a `for` with no list prints the one it
        // stands for.
        case("declare-f-for-puts-do-on-its-own-line", r#"f() { for i in 1 2; do echo $i; done; }; declare -f f"#),
        case("declare-f-for-without-a-list", r#"f() { for x; do echo $x; done; }; declare -f f"#),
        case("declare-f-arithmetic-for", r#"f() { for ((i=0;i<2;i++)); do echo $i; done; }; declare -f f"#),
        case("declare-f-select", r#"f() { select x in a; do break; done; }; declare -f f"#),
        case("declare-f-case-arms", r#"f() { case $x in a) echo a;; b|c) echo p; echo q;; esac; }; declare -f f"#),
        case("declare-f-case-fallthrough-terminators", r#"f() { case $x in a) echo p;;& b) echo q;& esac; }; declare -f f"#),
        case("declare-f-a-group-and-its-redirect", r#"f() { { echo g; } > /dev/null; }; declare -f f"#),
        case("declare-f-a-group-inside-a-branch", r#"f() { if :; then { echo a; }; echo b; fi; }; declare -f f"#),
        case("declare-f-subshell-body", r#"f() { ( echo a; echo b ); }; declare -f f"#),
        case("declare-f-a-subshell-function-body", r#"f() ( echo x ); declare -f f"#),
        case("declare-f-a-nested-function-says-function", r#"f() { g() { echo n; }; }; declare -f f"#),
        case("declare-f-redirect-spacing", r#"f() { echo a >f 2>&1; echo b >&2; echo c &>d; }; declare -f f"#),
        case("declare-f-arithmetic-and-test-commands", r#"f() { [[ -n $x ]] && (( y++ )); ((n++)); }; declare -f f"#),
        case("declare-f-pipelines-and-connections", r#"f() { echo a | cat; echo b && echo c || echo d; }; declare -f f"#),
        // `&` does not end the line the way `;` does.
        case("declare-f-background-continues-the-line", r#"f() { echo a & ! true; echo b & echo c & }; declare -f f"#),
        case("declare-f-a-function-level-redirect", r#"f() { echo a; } 2>/dev/null; declare -f f"#),
        case("redir-heredoc-with-no-body", "wc -c <<EOF\nEOF"),
        case("redir-heredoc-with-one-empty-line", "wc -c <<EOF\n\nEOF"),
        case("redir-herestring", r#"cat <<< "here string""#),
        case("redir-fd", r#"exec 3> three; echo x >&3; exec 3>&-; cat three"#),
        // A bare `exec` moves this process's descriptors, and the shell's
        // own output has to follow, including inside `$( )`:
        // `v=$(exec > f; echo hi)` puts `hi` in the file. A builtin and a
        // spawned command share the write position, or the second
        // overwrites the first.
        case("a-bare-exec-redirect-catches-what-the-shell-prints-next", r#"exec > out; echo builtin; /bin/echo external; exec 1>&2; cat out"#),
        case("a-bare-exec-redirect-inside-a-substitution", r#"v=$(exec > out; echo builtin; /bin/echo external); echo "[$v]"; cat out"#),
        case("a-bare-exec-redirect-inside-a-subshell", r#"( exec > out; echo builtin; /bin/echo external ); echo after; cat out"#),
        case("a-bare-exec-redirect-of-stderr", r#"( exec 2> err; echo e >&2; /bin/ls /nosuch ); echo after; wc -l < err"#),
        // The last command of a subshell, which in a pane could be handed off
        // to a pty: its output and status still arrive.
        case(
            "a-subshells-last-command-is-waited-for",
            r#"( /bin/echo one ); ( /bin/echo a; /bin/echo b ); ( /bin/false ); echo "rc=$?"; ( /bin/sh -c 'exit 7' ); echo "rc=$?""#,
        ),
        case(
            "a-subshells-last-command-decides-a-condition",
            r#"if ( /bin/false ); then echo t; else echo f; fi; if ( /bin/true ); then echo t; else echo f; fi"#,
        ),
        // Pipelines and subshells
        case("pipeline", r#"printf 'b\na\n' | sort | head -1"#),
        // A half-line from a builtin has to reach fd 1 before the child
        // that writes to the same fd next.
        case("flush-order", r#"printf A; printf B | cat; printf C; echo"#),
        case("flush-order-stderr", r#"printf E >&2; printf F | cat; echo"#),
        case("pipeline-status", r#"false | true; echo $?; set -o pipefail; false | true; echo $?"#),
        // A lone stage that needs a shell runs in this process (`run_multi`'s
        // `inproc_stage`). It must still behave as a subshell, report its own
        // status, and die when its reader goes away.
        case("pipeline-stage-is-a-subshell", r#"x=1; { x=2; echo "in=$x"; } | cat; echo "out=$x""#),
        // What a re-exec'd stage couldn't carry across.
        case("dirs-in-a-pipeline", r#"cd /; pushd /usr >/dev/null; dirs | cat"#),
        case("alias-in-a-pipeline", r#"alias q=x; alias | cat"#),
        case("trap-in-a-pipeline", r#"trap 'echo t' INT; trap -p | cat"#),
        case("complete-in-a-pipeline", r#"complete -W 'a b' foo; complete -p foo | cat"#),
        case("jobs-in-a-pipeline", r#"sleep 0.2 & jobs -p | grep -cE '^[0-9]+$'"#),
        case("pipeline-stage-cd-does-not-escape", r#"cd /; { cd /usr; echo "in=$PWD"; } | cat; echo "out=$PWD""#),
        // Paths resolve against the shell's directory, not the process's. An
        // in-process stage shares the process with the shell.
        case("redirect-is-relative-to-the-shells-cwd", r#"mkdir -p d/e; cd d/e; echo hi > rel.txt; cat rel.txt; cd ../..; cat d/e/rel.txt"#),
        case("source-is-relative-to-the-shells-cwd", r#"mkdir -p d; printf 'echo sourced\n' > d/s.sh; cd d; . ./s.sh"#),
        case("glob-is-relative-to-the-shells-cwd", r#"mkdir -p d/e; : > d/e/a.rs; : > d/e/b.txt; : > top.rs; cd d/e; echo *.rs"#),
        case("glob-with-a-directory-prefix", r#"mkdir -p d/e; : > d/e/a.rs; cd d; echo e/*.rs"#),
        case("read-redirect-is-relative-to-the-shells-cwd", r#"mkdir -p d; printf 'x\n' > d/in.txt; cd d; read v < in.txt; echo "[$v]""#),
        case("pipeline-stage-export-does-not-escape", r#"{ export E=inner; echo hi; } | cat; echo "[${E-unset}]""#),
        case("pipeline-builtin-stage-status", r#"false | cat; echo "${PIPESTATUS[*]}""#),
        case("pipeline-stage-exit-status", r#"{ exit 3; } | cat; echo "${PIPESTATUS[*]} rc=$?""#),
        case("pipeline-stage-pipefail", r#"set -o pipefail; { exit 3; } | cat; echo "rc=$?""#),
        case("pipeline-stage-in-the-middle", r#"/bin/echo a b | { read x y; echo "$y $x"; } | cat"#),
        // Every case above ends in `cat`, which can't tell a stage that
        // wrote into the pipe from one that wrote past it to fd 1. A last
        // stage that transforms its input, inside a capture, catches that:
        // `x=$(echo hello | wc -c)` is 6. Either half alone passes with the
        // bug in place.
        case("capture-a-pipeline-from-a-builtin", r#"x=$(echo hello | wc -c); echo "[$x]""#),
        case("capture-a-pipeline-through-tr", r#"echo "[$(echo a b c | tr ' ' -)]""#),
        case(
            "capture-a-pipeline-from-printf",
            r#"x=$(printf '%s
' one two | grep two); echo "[$x]""#,
        ),
        case(
            "capture-a-pipeline-of-three",
            r#"x=$(echo a b c | tr ' ' '
' | wc -l); echo "[$x]""#,
        ),
        case(
            "capture-a-pipeline-of-two-builtins",
            r#"x=$(printf 'a
b
' | { read v; echo "got=$v"; }); echo "[$x]""#,
        ),
        case("capture-a-pipeline-from-a-group", r#"x=$({ echo one; echo two; } | wc -l); echo "[$x]""#),
        case("capture-a-pipeline-in-a-subshell", r#"x=$( (echo hi | wc -c) ); echo "[$x]""#),
        // The same one level further in, where the sink to avoid is the inner
        // capture.
        case("capture-inside-a-captured-pipeline", r#"x=$(echo "$(echo hi | wc -c)" | tr -d ' '); echo "[$x]""#),
        // The same pipelines without a capture, so a fix that breaks the
        // plain case shows up here.
        case(
            "pipeline-into-a-transforming-stage",
            r#"echo hello | wc -c; printf 'a
b
' | wc -l; echo hi | tr a-z A-Z"#,
        ),
        case("pipeline-stage-through-three", r#"echo a b c | tr ' ' '\n' | wc -l"#),
        // More than a pipe buffer, so the stage has to run while its output
        // is drained.
        case("pipeline-stage-outlasts-the-pipe-buffer", r#"for i in $(seq 1 5000); do echo "line $i"; done | wc -l"#),
        // The reader leaves first. A stage that is its own process dies of
        // SIGPIPE; one running in the shell has to stop too, with the same
        // status.
        case("pipeline-stage-reader-leaves-early", r#"for i in $(seq 1 100000); do echo "l$i"; done | head -2; echo "ps=${PIPESTATUS[*]}""#),
        case("pipeline-stage-unbounded-reader-leaves", r#"while true; do echo x; done | head -2; echo "ps=${PIPESTATUS[*]}""#),
        // Two or more stages that need a shell run as coroutines on one
        // thread (`run_multi_scheduled`): each would otherwise wait on a
        // pipe the other holds.
        case("two-shell-stages", r#"echo x | while read l; do echo "[$l]"; done"#),
        case("two-shell-stages-both-compound", r#"{ echo one; echo two; } | { read a; read b; echo "$b $a"; }"#),
        case("three-shell-stages", r#"echo hi | { read v; echo "$v $v"; } | { read a b; echo "$b-$a"; }"#),
        case("shell-stage-between-two-externals", r#"/bin/echo a b | { read x y; echo "$y $x"; } | cat"#),
        // More than a pipe buffer, so neither stage finishes without the
        // other running.
        case(
            "two-shell-stages-outlast-the-pipe-buffer",
            r#"for i in $(seq 1 5000); do echo "l$i"; done | { c=0; while read l; do c=$((c+1)); done; echo "$c"; }"#,
        ),
        // A stage's pipes have to be its real fds 0 and 1: an external
        // inheriting the rest of the input, and `exec {fd}<&0`, both use
        // them.
        case("shell-stage-hands-the-rest-to-an-external", r#"{ echo a; echo b; } | { read -r x; echo "x=$x"; cat; }"#),
        case("shell-stage-can-dup-its-own-stdin", r#"echo x | { exec {fd}<&0; read -r -u "$fd" l; echo "[$l]"; }"#),
        case("two-shell-stages-status", r#"{ exit 3; } | { read v; }; echo "ps=${PIPESTATUS[*]}""#),
        case("two-shell-stages-pipefail", r#"set -o pipefail; { exit 4; } | { read v; }; echo "rc=$?""#),
        case("two-shell-stages-are-subshells", r#"x=1; { x=2; echo "$x"; } | { read v; echo "in=$v"; }; echo "out=$x""#),
        case("two-shell-stages-cd-does-not-escape", r#"{ echo a; } | { read v; cd /tmp; }; echo "$PWD""#),
        case("two-shell-stages-reader-leaves-early", r#"while true; do echo x; done | { read v; echo "got=$v"; }"#),
        // Every shape where one side stops first. These hang rather than
        // answer wrongly, which is why the harness reports a timeout
        // separately.
        case("unbounded-shell-producer-into-an-external-head", r#"while true; do echo x; done | head -1"#),
        case("unbounded-external-producer-into-a-shell-reader", r#"yes | { read v; echo "got=$v"; }"#),
        case("unbounded-producer-in-the-middle", r#"echo start | { while read l; do echo "$l"; done; } | head -1"#),
        case("reader-leaves-between-two-shell-stages", r#"while true; do echo x; done | { read a; echo "$a"; } | head -1"#),
        case("both-sides-bounded-but-uneven", r#"seq 1 10000 | { read v; echo "got=$v"; }"#),
        case("subshell-scope", r#"x=1; (x=2); echo $x"#),
        case("subshell-exit", r#"(exit 4); echo $?"#),
        // In-process constructs share the real environment, so what a
        // foreground subshell exports has to be undone by hand (the env
        // journal in exec.rs).
        case("subshell-export-does-not-escape", r#"(export E=inner); echo "[${E-unset}]""#),
        case("subshell-export-restores-the-old-value", r#"export E=outer; (export E=inner); echo "$E""#),
        case("subshell-unset-does-not-escape", r#"export E=outer; (unset E); echo "$E""#),
        case("substitution-export-does-not-escape", r#"x=$(export E=inner; echo hi); echo "$x [${E-unset}]""#),
        case("nested-subshell-exports-unwind-in-order", r#"export E=a; (export E=b; (export E=c); echo "$E"); echo "$E""#),
        case("subshell-export-is-visible-to-a-child-of-that-subshell", r#"(export E=inner; env | grep '^E='); echo "[${E-unset}]""#),
        case("exported-var-still-reaches-an-external-command", r#"export E=v; env | grep '^E='"#),
        // Ways the shell's view and the process environment can disagree, in
        // what a child gets (see `Shell::command`).
        case("exported-local-shadows-the-global-for-a-child", r#"export E=outer; f() { local -x E=inner; env | grep '^E='; }; f; env | grep '^E='"#),
        case("exported-but-unset-exports-nothing", r#"export E; env | grep -c '^E=' || true"#),
        case("exported-then-unset-is-gone-from-a-child", r#"export E=v; unset E; env | grep -c '^E=' || true"#),
        case("export-n-removes-it-from-a-child", r#"export E=v; export -n E; env | grep -c '^E=' || true"#),
        case("assignment-after-export-is-seen-by-a-child", r#"export E=first; E=second; env | grep '^E='"#),
        case("a-child-sees-a-var-exported-inside-a-function", r#"f() { export E=fromfn; }; f; env | grep '^E='"#),
        // `export` is global even inside a function; `declare` and `local -x`
        // are not.
        case("export-in-a-function-is-global", r#"f() { export E=fromfn; }; f; echo "[$E]""#),
        case("declare-in-a-function-is-not", r#"f() { declare Z=5; }; f; echo "[$Z]""#),
        case("local-x-in-a-function-is-not", r#"export E=outer; f() { local -x E=inner; echo "in=[$E]"; }; f; echo "out=[$E]""#),
        // A name can carry attributes without carrying a value.
        case("declare-p-of-an-exported-but-unset-name", r#"export E; declare -p E"#),
        case("declare-p-of-an-integer-but-unset-name", r#"declare -i N; declare -p N"#),
        case("declare-p-of-a-name-with-nothing-at-all", r#"declare -p NOPE 2>/dev/null; echo "rc=$?""#),
        case("export-name-on-an-already-set-var", r#"E=v; export E; env | grep '^E='"#),
        case("export-name-on-a-local", r#"f() { local Z=inner; export Z; env | grep '^Z='; }; f"#),
        // The capture path: output far larger than a pipe buffer, and
        // output with no trailing newline, both through `$( )`.
        case("substitution-captures-a-lot", r#"x=$(for i in $(seq 1 4000); do echo "line $i"; done); echo "${#x}""#),
        case("substitution-captures-without-a-trailing-newline", r#"x=$(printf 'no-newline'); echo "[$x]""#),
        case("command-subst", r#"echo "$(printf a)$(printf b)""#),
        case("command-subst-backtick", "echo \"`printf a`\""),
        case("process-subst", r#"cat <(printf 'p\n')"#),
        case("proc-sub-out-to-a-builtins-redirect", r#"echo hi > >(cat)"#),
        case("proc-sub-out-to-an-externals-redirect", r#"/bin/echo hi > >(cat)"#),
        case("proc-sub-out-that-answers-at-eof", r#"printf 'a\nb\n' > >(wc -l)"#),
        case("proc-sub-out-as-an-argument", r#"tee >(wc -l) < /etc/hostname > /dev/null"#),
        // `>( )` runs concurrently with the command writing to it, so a
        // body that answers only at end of input still answers.
        //
        // Piped into `sort` because where the body's output lands relative
        // to the next command is a race in both shells: the substitution
        // isn't waited for. What's promised is that everything it writes
        // arrives.
        case("proc-sub-out-body-output-all-arrives", r#"{ echo hi > >(cat); echo done; } | sort"#),
        case("proc-sub-out-with-a-shell-body", r#"printf 'a\nb\n' > >(while read l; do echo "<$l>"; done)"#),
        case("proc-sub-out-two-at-once", r#"{ printf 'x\n' > >(cat) 2>/dev/null; printf 'y\n' > >(cat); } | sort"#),
        case("proc-sub-out-larger-than-a-pipe-buffer", r#"seq 1 20000 > >(wc -l)"#),
        // A redirect target is expanded once, even for an external command.
        case("redirect-target-is-expanded-once", r#"/bin/echo x > $(echo side >&2; echo out.txt); cat out.txt"#),
        case("redirect-target-expanded-once-for-a-builtin", r#"echo x > $(echo side >&2; echo out.txt); cat out.txt"#),
        // `<( )` streams: the producer runs as a coroutine while the shell
        // waits on its consumer, so an unbounded producer works.
        case("process-subst-streams-from-an-unbounded-producer", r#"head -2 <(yes)"#),
        case("process-subst-streams-from-an-unbounded-shell-producer", r#"head -3 <(while true; do echo x; done)"#),
        case("process-subst-two-at-once", r#"cat <(echo a) <(echo b)"#),
        case("process-subst-as-a-redirect", r#"wc -l < <(printf '1\n2\n3\n')"#),
        case("process-subst-read-by-a-builtin", r#"read v < <(echo hello); echo "[$v]""#),
        case("process-subst-read-by-a-loop", r#"while read l; do echo "<$l>"; done < <(printf 'a\nb\n')"#),
        case("process-subst-more-than-a-pipe-buffer", r#"wc -l < <(seq 1 20000)"#),
        case("process-subst-producer-sees-the-shells-variables", r#"x=v; cat <(echo "$x")"#),
        case("process-subst-does-not-leak-descriptors", r#"for i in 1 2 3 4 5; do cat <(echo $i) >/dev/null; done; ls /proc/self/fd | wc -l"#),
        case("group-command", r#"{ echo a; echo b; } | wc -l"#),
        // `time` is a reserved word, so it takes a whole pipeline and a
        // group, and `echo time` is still a word. The numbers vary, so only
        // output a format makes stable is compared; `format_times`' unit
        // test covers the arithmetic.
        case("time-format-literal", r#"TIMEFORMAT='timed'; time true"#),
        case("time-format-percent", r#"TIMEFORMAT='%%'; time true"#),
        case("time-status", r#"TIMEFORMAT='t'; time false; echo "status=$?""#),
        case("time-pipeline", r#"TIMEFORMAT='t'; time printf 'b\na\n' | sort | head -1"#),
        case("time-group", r#"TIMEFORMAT='t'; time { echo g; }"#),
        case("time-negated", r#"TIMEFORMAT='t'; ! time false; echo "status=$?""#),
        case("time-not-a-word", r#"echo time; x=time; echo "$x"; echo "$(echo time)""#),
        // Globbing
        case("glob-star", r#": > ga; : > gb; printf '%s,' g*; echo"#),
        case("glob-class", r#": > g1; : > g2; printf '%s,' g[12]; echo"#),
        case("glob-question", r#": > gx; printf '%s,' g?; echo"#),
        case("glob-nomatch", r#"printf '%s,' zz_no_match*; echo"#),
        case("glob-dot-literal", r#": > .hidden; printf '%s,' .h*; echo"#),
        // The shopts that change what a pattern matches.
        case("shopt-nullglob", r#": > kept; shopt -s nullglob; printf '[%s]' zz_no_match*; printf '[%s]' kept; echo"#),
        case("shopt-dotglob", r#": > .hidden; : > shown; shopt -s dotglob; printf '[%s]' *; echo"#),
        case("shopt-nocaseglob", r#": > ABC; shopt -s nocaseglob; printf '[%s]' abc*; echo"#),
        // A quoted word is never a pattern.
        case("shopt-nullglob-quoted", r#"shopt -s nullglob; printf '[%s]' 'a*b'; echo"#),
        // Every component may be a pattern, not only the last.
        case("glob-multi-component", r#"mkdir -p m/one m/two; : > m/one/f.c; : > m/two/f.c; printf '[%s]' m/*/f.c */*.c; echo"#),
        case("glob-question-component", r#"mkdir -p m/one; : > m/one/f; printf '[%s]' m/?ne/f; echo"#),
        case("glob-missing-dir", r#"printf '[%s]' nodir_zz/*; echo"#),
        // `**` is any number of directories, including none, and doesn't
        // descend through a symlink, where `*` does.
        case("globstar-all", r#"mkdir -p a/b/c; : > x; : > a/x; : > a/b/x; shopt -s globstar; printf '[%s]' **; echo"#),
        case("globstar-suffix", r#"mkdir -p a/b/c; : > x; : > a/x; : > a/b/c/x; shopt -s globstar; printf '[%s]' **/x; echo"#),
        case("globstar-middle", r#"mkdir -p a/b/c; : > a/x; : > a/b/c/x; shopt -s globstar; printf '[%s]' a/**/x; echo"#),
        case("globstar-prefix", r#"mkdir -p a/b; : > a/b/x; shopt -s globstar; printf '[%s]' a/**; echo"#),
        case("globstar-dirs-only", r#"mkdir -p a/b; : > a/x; shopt -s globstar; printf '[%s]' **/; echo"#),
        case("globstar-symlink", r#"mkdir -p a/b; : > a/x; ln -s a link; shopt -s globstar; printf '[%s]' **/x; printf '[%s]' */x; echo"#),
        case("globstar-off", r#"mkdir -p a/b; : > x; : > a/x; shopt -u globstar; printf '[%s]' ** **/x; echo"#),
        case("brace-list", r#"echo {a,b}{1,2}"#),
        case("brace-range", r#"echo {1..5} {a..e} {1..9..3}"#),
        case("brace-nested", r#"echo {a,b{1,2}}"#),
        case("tilde", r#"echo ~ | grep -c /"#),
        // A tilde that is the whole last word before an operator still
        // expands: `cd ~; pwd`. Compared against `$HOME` so the case
        // doesn't depend on where the corpus put the directory.
        case("tilde-before-semicolon", r#"[ "$(echo ~;)" = "$HOME" ] && echo expanded || echo literal"#),
        case("tilde-before-and", r#"[ "$(echo ~&&true)" = "$HOME" ] && echo expanded || echo literal"#),
        case("tilde-before-pipe", r#"[ "$(echo ~|cat)" = "$HOME" ] && echo expanded || echo literal"#),
        case("tilde-then-cd-semicolon", r#"[ "$(cd ~; pwd)" = "$HOME" ] && echo expanded || echo literal"#),
        case("tilde-in-a-for-list", r#"for i in ~; do [ "$i" = "$HOME" ] && echo expanded || echo literal; done"#),
        // ...and `#` still does not end one, mid-word.
        case("tilde-then-hash-is-literal", r#"echo ~#"#),
        // In an assignment a tilde prefix can start after the `=` and after
        // every unquoted `:`, which makes `PATH=~/bin:~/sbin` work.
        case("tilde-in-an-assignment", r#"x=~/y; [ "$x" = "$HOME/y" ] && echo expanded || echo literal"#),
        case("tilde-after-a-colon-in-an-assignment", r#"p=~/a:~/b; [ "$p" = "$HOME/a:$HOME/b" ] && echo expanded || echo literal"#),
        case("tilde-in-an-appending-assignment", r#"x=1; x+=:~/y; [ "$x" = "1:$HOME/y" ] && echo expanded || echo literal"#),
        case("tilde-in-a-subscripted-assignment", r#"declare -A m; m[k]=~/y; [ "${m[k]}" = "$HOME/y" ] && echo expanded || echo literal"#),
        case("tilde-in-an-exported-assignment", r#"export e=~/z; [ "$e" = "$HOME/z" ] && echo expanded || echo literal"#),
        case("tilde-in-a-local-assignment", r#"f() { local v=~/l; [ "$v" = "$HOME/l" ] && echo expanded || echo literal; }; f"#),
        case("tilde-in-an-assignment-shaped-argument", r#"[ "$(echo x=~/y)" = "x=$HOME/y" ] && echo expanded || echo literal"#),
        // The four shapes bash leaves alone: a flag isn't a name, a name
        // doesn't start with a digit, a quoted name isn't one, and a `:`
        // outside an assignment is a colon.
        case("tilde-after-a-flag-equals-is-literal", r#"echo --opt=~/y"#),
        case("tilde-after-a-digit-name-is-literal", r#"echo 9x=~/y"#),
        case("tilde-after-a-quoted-name-is-literal", r#"echo "a"=~/b"#),
        case("tilde-after-a-colon-outside-an-assignment-is-literal", r#"echo a:~/b"#),
        // Builtins
        case("echo-flags", r#"echo -n a; echo; echo -e 'a\tb'"#),
        case("printf-recycle", r#"printf '%s-%s\n' a b c d"#),
        case("printf-width", r#"printf '[%5s][%-5s][%05d]\n' a b 42"#),
        // A numeric escape in a format names a byte, so a pair of them
        // is one character.
        case("printf-escapes", r#"printf 'a\101b|\x41|\x7|\u0041|\U00000041|\0|\q|' | cat -v; echo"#),
        case("printf-escape-bytes", r#"printf '\303\244' | od -An -c"#),
        // `%b` takes a bare `\nnn` where `echo -e` reads it as text.
        case("printf-b-escapes", r#"printf '[%b][%b][%b]' 'a\101b' 'a\0101b' 'a\x41b' | cat -v; echo"#),
        case("echo-e-escapes", r#"echo -ne 'a\101b|a\0101b|a\x41b|' | cat -v; echo"#),
        case("printf-b-stop", r#"printf 'x%bz' 'a\cb'; echo done"#),
        case("echo-e-stop", r#"echo -ne 'a\cb'; echo done"#),
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
        // `trap -p` prints `trap -- 'code' SIG`, and `trap` has to read
        // that back.
        case("trap-reads-its-own-output", r#"trap -- 'echo t' USR1; trap -p | head -1"#),
        case("pushd-n-adds-without-moving", r#"cd /; pushd -n /usr >/dev/null; pushd -n /tmp >/dev/null; dirs; echo "cwd=$PWD""#),
        // What a construct that still re-execs can see of its parent shell.
        // A co-process starts once, so carrying this state costs nothing
        // per use.
        case("coproc-sees-the-directory-stack", r#"pushd /usr >/dev/null; coproc CP { dirs; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-sees-traps", r#"trap 'echo t' USR1; coproc CP { trap -p | head -1; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-sees-completions", r#"complete -W 'a b' foo; coproc CP { complete -p foo; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-round-trip", r#"coproc CP { cat; }; echo hi >&"${CP[1]}"; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("eval", r#"x=1; eval 'x=$((x+1))'; echo $x"#),
        case("source-file", "printf 'sourced=1\\n' > lib.sh; . ./lib.sh; echo $sourced"),
        // Errors and status
        case("exit-status", r#"true; echo $?; false; echo $?"#),
        case("set-e", r#"set -e; (false; echo unreached); echo $?"#),
        case("set-u", r#"set -u; echo "${undefined_zz-ok}""#),
        case("and-or", r#"true && echo t; false || echo f; false && echo no; echo $?"#),
        // Builtin flags
        case(
            "test-v",
            r#"x=1; a=(q); declare -A m; m[k]=1; [[ -v x ]] && echo x; [[ -v a[0] ]] && echo a; [[ -v m[k] ]] && echo m; [[ -v nope_zz ]] || echo no"#,
        ),
        case("kill-l", r#"kill -l 9; kill -l TERM; kill -l SIGTERM; kill -l 137"#),
        case("exec-a", r#"exec -a zzname /bin/sh -c 'echo $0'"#),
        case("noclobber", "set -C; echo a > nc; echo b > nc; echo $?; echo c >| nc; cat nc; set +C"),
        case("wait-n", r#"(exit 7) & wait -n; echo $?; wait -n; echo $?"#),
        case("export-f", r#"f() { :; }; export -f f; echo $?; export -f nosuch_zz; echo $?"#),
        case("read-t0", r#"read -t 0 < /dev/null; echo $?"#),
        case("set-o", r#"set -C -o pipefail; set -o > oo; grep -E '^(noclobber|pipefail|xtrace) ' oo"#),
        // `set -r` has to survive a re-exec. Restriction has no `-o` name, so
        // replaying the `-o` options doesn't carry it.
        case("restricted-keeps-a-backgrounded-subshell", r#"set -r; ( /bin/echo X ) & wait"#),
        case("restricted-keeps-a-backgrounded-group", r#"set -r; { /bin/echo X; } & wait"#),
        case("restricted-keeps-a-redirected-compound", r#"set -r; ( /bin/echo X ) <> /dev/null"#),
        // `<>` writes, so a restricted shell may not have it either.
        case("restricted-refuses-an-in-out-redirect", r#"set -r; exec 3<>/dev/null; echo "status=$?""#),
        // ...and what it may still do: a bare name, and any builtin.
        case("restricted-still-runs-a-bare-name", r#"set -r; ( env true && echo ran ) & wait"#),
        // `set -n`: what follows doesn't run. The first case names what
        // should appear.
        case("noexec-stops-what-follows", r#"echo before; set -n; echo after"#),
        case("noexec-cannot-be-undone", r#"echo before; set -n; set +n; echo after"#),
        case("noexec-by-its-long-name", r#"echo before; set -o noexec; echo after"#),
        case("noexec-skips-a-whole-construct", r#"echo before; set -n; for i in 1 2 3; do echo "$i"; done; echo after"#),
        case("noexec-skips-a-trap", r#"echo before; trap 'echo trapped' EXIT; set -n; echo after"#),
        case("noexec-leaves-the-status-alone", r#"true; set -n; false; echo "never=$?""#),
        case("ulimit-p", r#"ulimit -p"#),
        // Things bash never accepted
        case("readonly-array", r#"readonly a=(1); echo "[${a[*]}]"; a+=(2); echo "rc=$?""#),
        case("readonly-scalar-is-fatal", r#"readonly x=1; x=2; echo unreached"#),
        case("readonly-unset", r#"readonly x=1; unset x; echo "rc=$?""#),
        case("bad-option", r#"unset -z x"#),
        case("set-bad-option-name", r#"set -o nosuchopt"#),
        case("cd-too-many-arguments", r#"cd / /tmp"#),
        case("shift-past-end", r#"set -- a; shift 99; echo "rc=$?"; shift abc"#),
        case("printf-missing-conversion", r#"printf '%5'"#),
        case("printf-invalid-number", r#"printf '%d' 1x 2>/dev/null; echo " rc=$?""#),
        case("test-integer-expected", r#"[ a -eq b ]"#),
        case("test-too-many-arguments", r#"[ a = b = c ]"#),
        case("declare-bad-identifier", r#"export 1bad=1"#),
        case("bad-array-subscript", r#"declare -A m; m[]=1; echo unreached"#),
        case("bad-substitution", r#"echo ${x!y}; echo unreached"#),
        // stderr is discarded because the two shells word arithmetic errors
        // differently. What matters is that a `$(( ))` failure stops the
        // script and the `(( ))` command below doesn't.
        case("arith-expansion-is-fatal", r#"{ echo $((1+)); } 2>/dev/null; echo unreached"#),
        case("arith-command-is-not", r#"((1+)) 2>/dev/null; echo after"#),
        case("arith-parameter-default", r#"unset u; echo "$(( ${u:-5} + 1 ))" "$(( ${u-5} + 1 ))""#),
        case("arith-parameter-length", r#"x=abcde; echo "$(( ${#x} ))""#),
        case("arith-array-length", r#"a=(1 2 3); echo "$(( ${#a[@]} + 1 ))""#),
        case("arith-parameter-strip", r#"p=12x; echo "$(( ${p%x} + 1 ))""#),
        case("arith-positional-with-default", r#"f() { echo "$(( ${1:-0} + 1 ))"; }; f; f 5"#),
        case("arith-unset-plain-is-zero", r#"unset u; echo "$(( ${u} ))" "$(( u ))""#),
        case("arith-empty-is-zero", r#"echo "[$(( ))]"; (( )); echo "rc=$?""#),
        case("error-if-unset", r#": ${x:?}; echo unreached"#),
        // A NUL can't survive into a word, since `execve` would stop at it,
        // so both shells drop it and say so once per substitution.
        case("command-substitution-drops-a-nul", r#"x=$(printf 'a\0b'); printf '%s\n' "$x""#),
        case("command-substitution-warns-once-per-nul-run", r#"x=$(printf 'a\0b\0c'); printf '%s\n' "$x""#),
        case("command-substitution-without-a-nul-is-quiet", r#"x=$(printf 'ab'); printf '%s\n' "$x""#),
        case("funcnest-limits-recursion", r#"FUNCNEST=5; f() { f; }; f; echo unreached"#),
        case("funcnest-ignores-a-non-number", r#"FUNCNEST=abc; f() { echo in; }; f"#),
        case("funcnest-zero-is-no-limit", r#"FUNCNEST=0; f() { echo in; }; f"#),
        // Sourced with stderr discarded: the two shells report a syntax
        // error differently (bash echoes the line), and `.` turns the
        // rejection into a status the script can see.
        case("array-assignment-needs-attached-parens", "printf 'arr= (a b)\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("function-body-must-be-compound", "printf 'f() echo hi\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("empty-if-body", "printf 'if true; then fi\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("empty-loop-condition", "printf 'while; do :; done\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("declare-subscript", r#"declare 'a[0]=5'; echo "${a[0]}""#),
        // Variables are not the environment
        case("plain-assignment-is-not-exported", r#"x=1; env | grep -c '^x=1$'"#),
        case("export-is", r#"export y=2; env | grep -c '^y=2$'"#),
        case("export-n-keeps-the-value", r#"export z=1; export -n z; env | grep -c '^z='; echo "[$z]""#),
        case("a-child-sees-only-exports", r#"a=1; export b=2; env | grep -cE '^(a|b)='"#),
        case("read-nul-delimited", "printf 'a\\0b\\0' | while read -r -d '' v; do printf '[%s]' \"$v\"; done; echo"),
        // An array is many words
        case("array-copy", r#"a=(1 2 3); b=("${a[@]}"); c=(x "${a[@]}" y); echo "${#b[@]} ${#c[@]}""#),
        case("array-literal-splits", r#"s="p q"; d=($s); e=("$s"); echo "${#d[@]} ${#e[@]}""#),
        case("array-transform-per-element", r#"a=("x y" z); printf '[%s]' "${a[@]@Q}"; echo"#),
        case("arith-subscript", r#"a=(1 2 3); i=2; declare -A m=([k]=7); echo $((a[1])) $((a[i])) $((m[k]))"#),
        case("arith-subscript-assign", r#"a=(0 0); ((a[1]=5)); ((a[1]++)); echo "${a[*]}""#),
        case("assoc-key-with-spaces", r#"declare -A m; m[x y]=1; k="a b"; m[$k]=2; echo "${m[x y]}${m[a b]}""#),
        case("nested-subscript-assign", r#"a=(1 2); b=(0); a[b[0]]=9; echo "${a[0]}""#),
        case("index-append", r#"declare -A m; m[k]+=v; m[k]+=w; a=(x); a[0]+=y; echo "${m[k]} ${a[0]}""#),
        case("nameref-to-an-array", r#"a=(1 2); declare -n r=a; echo "${r[1]} ${#r[@]} ${r[*]}"; r[0]=9; echo "${a[0]}""#),
        // A pipeline stage is this shell's child
        case("stage-sees-globals", r#"x=1; f() { echo "[$x]"; }; { echo "[$x]"; } | cat; f | cat; (echo "[$x]") | cat"#),
        case("stage-sees-shell-options", r#"set -u; { echo "${nosuch-ok}"; } | cat; shopt -s nullglob; { shopt nullglob; } | cat"#),
        case("read-leaves-the-rest-of-the-pipe", r#"{ echo a; echo b; } | { read -r x; echo "x=$x"; cat; }"#),
        case("lastpipe", r#"shopt -s lastpipe; set +m; n=0; seq 1 3 | while read -r l; do n=$((n+1)); done; echo "$n""#),
        case("lastpipe-status", r#"shopt -s lastpipe; set +m; false | true; echo "${PIPESTATUS[*]}"; true | false; echo "rc=$?""#),
        case("shopt-status", r#"shopt nullglob; echo "rc=$?"; shopt -s nullglob; shopt nullglob; echo "rc=$?""#),
        // The descriptors the shell owns
        case(
            "builtin-reads-a-shell-fd",
            r#"exec 3</etc/hostname; read -r a <&3; exec 3<&-; exec 4</etc/hostname; read -r -u 4 b; exec 4<&-; [ "$a" = "$b" ] && echo same"#,
        ),
        case("read-u-bad-fd", r#"read -r -u 9 l 2>/dev/null; echo "rc=$?""#),
        case("varfd-open-and-close", r#"exec {fd}</etc/hostname; echo "$((fd>=10))"; read -r -u "$fd" l; exec {fd}<&-; [ -n "$l" ] && echo read"#),
        case("varfd-dup", r#"echo x | { exec {fd}<&0; read -r -u "$fd" l; echo "[$l]"; }"#),
        case("mapfile-counted-options", "printf '1\\n2\\n3\\n4\\n' | { mapfile -t -s 1 -n 2 a; echo \"${#a[@]} ${a[*]}\"; }"),
        case("mapfile-origin", "printf '1\\n2\\n' | { mapfile -t -O 5 a; echo \"${!a[*]}\"; }"),
        // errexit's rules
        case("errexit-exempts-a-chain-member", r#"set -e; f() { false; }; f && echo ok; false && echo no; echo after"#),
        case("errexit-still-fires-on-the-last", r#"set -e; true && false; echo unreached"#),
        case("errexit-and-pipefail", r#"set -euo pipefail; false | true; echo unreached"#),
        case("assignment-takes-its-substitution-status", r#"x=$(exit 7); echo "rc=$?"; y=$(true); echo "rc=$?""#),
        case("errexit-on-a-failed-capture", r#"set -e; x=$(false); echo unreached"#),
        case("inherit-errexit", r#"set -e; x=$(false; echo reached); echo "[$x]""#),
        case("set-cluster-ending-in-o", r#"set -euo pipefail; set -o | grep -cE '^(errexit|nounset|pipefail) +on'"#),
        // Nested expansions and diagnostics
        case(
            "dollar-lt-file",
            r#"printf 'x
y
' > f; echo "[$(<f)]"; v=$(<f); echo "[$v]""#,
        ),
        case("dollar-lt-missing", r#"echo "[$(<nosuch_zz)]"; echo "rc=$?""#),
        case("arith-with-a-substitution", r#"echo $(( $(echo 2) + 3 )); let "v=$(echo 4)+1"; echo "$v""#),
        case("arith-name-still-resolves", r#"x=y; y=2; echo $((x))"#),
        case("command-not-found", r#"nosuchcmd_zz; echo "rc=$?""#),
        case("not-executable-is-126", r#"/etc/hosts; echo "rc=$?"; /etc; echo "rc=$?""#),
        case("missing-path-is-127", r#"./nosuchpath_zz; echo "rc=$?""#),
        case("source-a-directory", r#". /etc; echo "rc=$?"; source /nosuch_zz; echo "rc=$?""#),
        // local, FUNCNAME and the call stack
        case("local-shadows-as-unset", r#"x=out; f() { local x; echo "[${x-unset}]"; }; f; echo "[$x]""#),
        case("declare-with-no-value-is-unset", r#"declare z; echo "[${z-unset}]"; g() { declare y; echo "[${y-unset}]"; }; y=out; g"#),
        case("nounset-status", r#"set -u; echo "$nosuch_zz""#),
        case("bare-array-is-element-zero", r#"a=(1 2); echo "[$a]"; declare -A m=([k]=v); echo "[$m]""#),
        case("funcname-scalar", r#"f() { echo "[$FUNCNAME]"; }; f"#),
        case("funcname-has-no-main-frame-under-c", r#"f() { g; }; g() { echo "${FUNCNAME[*]} n=${#FUNCNAME[@]}"; }; f"#),
        // Parser strictness. Sourced, as above.
        case("two-defs-no-separator", "printf 'f() { :; } f() { :; }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case(
            "brace-group-needs-a-terminator",
            "printf '{ : }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\"; printf '{ echo }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\"",
        ),
        case("unterminated-brace-expansion", "printf 'echo ${x\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("c-for-needs-both-parens", "printf 'for ((i=0; i<2; i++) do :; done\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        // bash rejects this at parse time and bish when the condition runs;
        // sourced, both are a non-zero status.
        case("dbracket-rejects-a-fourth-operand", "printf '[[ a == b == c ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        // Inside `${...}` a shell metacharacter is text: the body was already
        // scanned to its matching brace.
        case(
            "expansion-operands-keep-shell-metacharacters",
            r#"s=a:b; echo "${s//:/|}" "${s//:/a|b}" "${s//:/;}" "${s//:/>}" "${s//:/(}" "${s//|/x}" "${s:-x|y}""#,
        ),
        // `&` in a replacement is the matched text and `\&` an ampersand.
        // The difference has to survive quote removal, so it's decided on
        // the chunks, not the string.
        case(
            "ampersand-in-a-replacement-is-the-match",
            r#"s=aXb; echo "${s//X/&}" "${s//X/[&]}" "${s//X/&&}" "${s//X/\&}" "${s//X/\\&}" "${s/#a/&&}""#,
        ),
        case("an-ampersand-from-a-variable-is-still-the-match", r#"s=aXb; r="&"; q="\&"; echo "${s//X/$r}" "${s//X/$q}""#),
        case("dbracket-unary-with-no-operand", "printf '[[ -z ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("dbracket-binary-with-no-right-side", "printf '[[ a -eq ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("dbracket-two-words-and-no-operator", "printf '[[ a b ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("dbracket-with-nothing-in-it", "printf '[[ ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        // `-a` is both a unary and a binary operator, decided by the word
        // count: two words is the file test, three the `and`.
        case(
            "dash-a-is-a-file-test-or-an-and-depending-on-arity",
            r#"[[ -a /etc/passwd ]] && echo exists; [ x -a y ] && echo both; [[ -a /nope ]] || echo gone"#,
        ),
        // The permission tests ask "may I", not "does it exist".
        case(
            "permission-tests-ask-the-kernel",
            r#"[ -r /etc/passwd ] && echo r; [ -w / ] || echo "not w"; [ -x /bin/sh ] && echo x; [ -w /etc/passwd ] || echo "passwd not w""#,
        ),
        case(
            "nocasematch-covers-case",
            r#"shopt -s nocasematch; case AB in ab) echo ci;; esac; [[ ABC == abc ]] && echo dbracket; [ ABC = abc ] || echo literal"#,
        ),
        // Less common builtins and flags
        case("printf-star-width", r#"printf '%*d|%-*d|%.*f|%*.*f|\n' 5 42 5 42 2 3.14159 8 2 3.14159"#),
        case("tilde-user-and-dirs", r#"cd /tmp; cd /; echo "$(echo ~+) $(echo ~-)"; echo ~nosuchuser_zz"#),
        // Not through a pipe: `jobs` as a pipeline stage is its own
        // divergence.
        case("jobs-p-and-l", r#"sleep 0.2 & jobs -p > p; jobs > j; grep -cE '^[0-9]+$' p; grep -c Running j"#),
        case("type-a-lists-every-match", r#"type -a echo"#),
        // Declaring an array isn't assigning one: bash prints
        // `declare -a A` for the first and `declare -a A=()` for the
        // second. The attribute is what makes a later `A=x` mean `A[0]=x`.
        case("declare-p-of-a-declared-but-unassigned-array", r#"declare -a A; declare -p A; A=(); declare -p A"#),
        case("declare-p-of-a-declared-but-unassigned-assoc", r#"declare -A M; declare -p M; M=(); declare -p M"#),
        case("plain-assignment-to-an-array-writes-element-zero", r#"declare -a B; B=x; declare -p B; declare -A N; N=y; declare -p N"#),
        case("plain-assignment-to-an-array-that-has-values", r#"a=(1 2); a=x; declare -p a"#),
        case(
            "a-local-array-attribute-does-not-outlive-the-function",
            r#"f() { local -a arr; declare -p arr; local -A m; declare -p m; }; f; declare -p arr; declare -p m"#,
        ),
        case(
            "a-local-array-shadows-a-global-one-and-gives-it-back",
            r#"g=(1 2); f() { local -a g; declare -p g; g=(9); declare -p g; }; f; declare -p g"#,
        ),
        case("alias-p", r#"alias x=y; alias -p"#),
        case("dollar-quoted-string", r#"x=1; echo $"lit $x""#),
        case("source-takes-arguments", "printf 'echo \"[$1][$#]\"\\n' > s.sh; set -- outer; . ./s.sh a b; echo \"after=[$1]\""),
        case("xtrace-traces-assignments", r#"PS4='XX '; set -x; x=1; y="a b"; a=(1 2)"#),
        case("true-and-false-are-builtins", r#"type -t true; type -t false; true; echo "rc=$?"; false; echo "rc=$?""#),
        // Redirects
        case("stderr-to-a-saved-fd", r#"exec 3>&1; echo a 2>&3; exec 3>&-; echo b 3>&1 >&2 2>&3 3>&-"#),
        case("close-an-unopened-fd", r#"{ echo a; } 3>&-; echo b 4>&-"#),
        case("dup-to-a-word-fd", r#"exec 3>&1; f=3; echo hi >&$f; echo two >&$((1+2)); exec 3>&-"#),
        case("dup-to-a-closed-fd", r#"echo x >&9; echo "rc=$?""#),
        // The redirect list runs in source order, for an external as well
        // as a builtin, and a descriptor names what it names at that point:
        // that's what makes the swap idiom work. Both spellings, since
        // builtins and externals get there by different routes.
        case("fd-swap-then-redirect-external", r#"/bin/echo x 3>&1 1>&2 2>&3 3>&- 2>/dev/null"#),
        case("fd-swap-then-redirect-compound", r#"{ echo o; echo e >&2; } 3>&1 1>&2 2>&3 3>&- 2>/dev/null"#),
        // Every file the list names is opened, even one a later redirect
        // supersedes: `a` is still created and truncated.
        case("a-superseded-redirect-still-creates-its-file", r#"/bin/echo x > a > b; ls a b; echo x 2>c 2>d; ls c d"#),
        // A construct's `2>` reaches the externals inside it too.
        case("a-compounds-stderr-reaches-externals", r#"{ /bin/ls /nosuch; } 2>/dev/null; echo done; { /bin/ls /nosuch; } 2>e; wc -l < e"#),
        case("a-loops-stderr-reaches-externals", r#"for i in 1; do /bin/ls /nosuch; done 2>/dev/null; echo done"#),
        case("a-functions-stderr-reaches-externals", r#"f() { /bin/ls /nosuch; }; f 2>/dev/null; echo done"#),
        // A dup names the descriptor as it stands at that point in the
        // list, so these are two different destinations. Both spellings,
        // because a builtin and a compound command resolve redirects by
        // different paths.
        case("dup-to-stderr-that-is-itself-redirected", r#"echo e >&2 2>/dev/null; echo done"#),
        case("dup-to-stderr-that-was-already-redirected", r#"echo e 2>/dev/null >&2; echo done"#),
        case("compound-dup-to-stderr-that-is-itself-redirected", r#"{ echo e; } >&2 2>/dev/null; echo done"#),
        case("compound-dup-to-stderr-that-was-already-redirected", r#"{ echo e; } 2>/dev/null >&2; echo done"#),
        // Both streams on one destination share a write position: opened
        // once, or duped, never opened twice.
        case("both-streams-one-file-keeps-one-position", r#"{ echo out; echo err >&2; } >f 2>&1; cat f"#),
        case("dup-before-the-redirect-it-would-have-followed", r#"{ echo a; echo b >&2; } 2>&1 >f; cat f; echo "--""#),
        case("coproc-round-trip", r#"coproc CP { cat; }; echo hi >&"${CP[1]}"; sleep 0.2; read -r l <&"${CP[0]}"; echo "back=[$l]""#),
        // A lone bracket is not a pattern
        case("lone-bracket-is-literal", r#"printf '%s,' [ ] '[abc'; echo; [ 1 -lt 2 ] && echo test-works"#),
        // Parameter expansion corners
        case("count-of-the-positionals", r#"set -- a b c; echo "${#@} ${#*} $#""#),
        case("omitted-slice-offset", r#"a=(1 2 3); echo "${a[@]::2}"; x=abcdef; echo "${x::3}"; set -- a b c; echo "${@:1:1}""#),
        // A redirect on a function call
        case("function-call-redirect", r#"f() { echo a; /bin/echo b; echo e >&2; }; f > o 2> e; cat o e; rm -f o e"#),
        case("function-call-redirect-fd", r#"f() { echo x; }; f 3>&1 >o; cat o; rm -f o"#),
        case(
            "dbracket-quoted-pattern",
            r#"[[ abc == a* ]] && echo glob; [[ abc == "a*" ]] || echo literal; p='a*'; [[ abc == $p ]] && echo var-globs; [[ abc == "$p" ]] || echo var-literal"#,
        ),
        // How the shell was invoked
        case("bash-versinfo", r#"echo "${#BASH_VERSINFO[@]} ${BASH_VERSINFO[3]} ${BASH_VERSINFO[4]}""#),
        // The harness clears the environment, so both shells start with no
        // SHLVL and must reach 1.
        case("shlvl-starts-at-one", r#"echo "$SHLVL""#),
        // `cd` reads the shell's HOME and OLDPWD, not the process
        // environment's.
        case("cd-uses-the-shells-home", r#"mkdir -p h; HOME=$PWD/h; cd; [ "$PWD" = "$(cd h 2>/dev/null; pwd)" ] || pwd"#),
        case("cd-dash-uses-the-shells-oldpwd", r#"mkdir -p a b; cd a; cd ../b; cd - > /dev/null; basename "$PWD""#),
        case("cd-with-no-home-fails", r#"unset HOME; cd 2>/dev/null; echo "rc=$?""#),
        // A shell remembers the route, not the destination: after
        // `cd link`, `$PWD` ends in `link`, and `cd ..` goes back to where
        // `link` sits. Only `pwd -P` resolves symlinks.
        case("cd-keeps-the-route-it-was-given", r#"mkdir -p b/c; ln -s b/c link; cd link; echo "${PWD##*/}"; pwd -P | sed 's|.*/\(.\)$|\1|'"#),
        case("cd-dot-dot-is-lexical", r#"mkdir -p b/c; ln -s b/c link; cd link; cd ..; echo "${PWD##*/}""#),
        case("cd-physical-resolves-first", r#"mkdir -p b/c; ln -s b/c link; cd -P link; echo "${PWD##*/}"; cd ..; echo "${PWD##*/}""#),
        case("cd-normalises-dots-it-was-given", r#"mkdir -p b/c; cd ./b/../b/c; echo "${PWD##*/}""#),
        case("cd-dash-with-no-oldpwd-fails", r#"unset OLDPWD; cd - 2>/dev/null; echo "rc=$?""#),
        // Unsetting a variable inherited from the environment unsets it for
        // lookups and children alike.
        case("unset-an-inherited-variable", r#"echo "[${HOME-gone}]"; unset HOME; echo "[${HOME-gone}]""#),
        case("unset-an-inherited-variable-under-nounset", r#"unset HOME; set -u; echo "$HOME" 2>/dev/null; echo "rc=$?""#),
        case("unset-then-set-again", r#"unset HOME; HOME=/x; echo "$HOME""#),
        // `type` knows five kinds, in this order.
        case("type-of-a-keyword", r#"type if; type -t for; type -t "[["; type -t "!"; type -t time"#),
        // Reported only where it would be expanded, hence the shopt: under
        // `-c` without it, bash's `type` says "not found".
        case("type-of-an-alias", r#"shopt -s expand_aliases; alias ll="ls -l"; type ll; type -t ll"#),
        case("type-of-an-unexpanded-alias", r#"alias ll="ls -l"; alias -p; type -t ll; echo "rc=$?""#),
        // An alias applies from the statement after the one that defined
        // it.
        //
        // The lines go through a sourced file, not newlines in the case
        // itself, so the case means the same after the serializer
        // round-trip test flattens it: newlines are what's being tested.
        // "command not found" is worded differently by the shells, so these
        // check the status.
        case(
            "an-alias-applies-to-later-lines",
            r#"shopt -s expand_aliases; printf '%s\n' 'alias hi="echo A"' 'hi' 'hi extra' 'true; hi' > al; . ./al"#,
        ),
        // ...but not within the statement that defined it, since bash reads a
        // whole statement before running any of it.
        case(
            "an-alias-does-not-apply-within-its-own-statement",
            r#"shopt -s expand_aliases; alias hi="echo A"; hi 2>/dev/null; echo "rc=$?"; alias ho="echo B" ; true ; ho 2>/dev/null; echo "rc=$?""#,
        ),
        // A statement spanning lines is one statement, so a use on the line
        // it ends on doesn't expand, while the next line's does. Start
        // lines alone can't tell those apart; that's what
        // `ListItem::end_line` is for.
        case(
            "an-alias-defined-inside-a-multi-line-statement",
            r#"shopt -s expand_aliases; printf '%s\n' 'for i in 1; do alias hi="echo A"' 'done; hi 2>/dev/null; echo "rc=$?"' 'hi' > al; . ./al"#,
        ),
        case(
            "an-alias-defined-in-a-function-applies-after-the-call",
            r#"shopt -s expand_aliases; printf '%s\n' 'f(){ alias hi="echo A"; }; f' 'hi' 'g(){ alias ho="echo B"; ho; }; g 2>/dev/null; echo "rc=$?"' > al; . ./al"#,
        ),
        // A `$( )` body is read on its own, so an alias defined in it
        // applies to the rest of it. A `( )` body is read with the source
        // around it.
        case(
            "an-alias-defined-inside-a-substitution-and-inside-a-subshell",
            r#"shopt -s expand_aliases; printf '%s\n' 'v=$(alias hi="echo A"' 'hi); echo "[$v]"' '( alias ho="echo B"' 'ho ) 2>/dev/null; echo "rc=$?"' > al; . ./al"#,
        ),
        // Without the option nothing expands, on any line, and `unalias`
        // undoes it.
        case(
            "an-alias-with-expansion-off-and-an-unaliased-one",
            r#"printf '%s\n' 'alias hi="echo A"' 'hi 2>/dev/null; echo "rc=$?"' > al; . ./al; shopt -s expand_aliases; alias ho="echo B"; unalias ho; ho 2>/dev/null; echo "rc=$?""#,
        ),
        case("keyword-completions", r#"compgen -A keyword | sort | tr '\n' ' '; echo"#),
        // Every getopts branch that doesn't set OPTARG unsets it.
        case("getopts-clears-optarg", r#"f(){ while getopts "ab:c" o; do echo "$o=[${OPTARG-UNSET}]"; done; }; f -a -b val -c"#),
        case("getopts-silent-missing-argument", r#"f(){ while getopts ":ab:" o; do echo "$o=[${OPTARG-UNSET}]"; done; }; f -a -b"#),
        // `$-` is every option letter currently on, lowercase first,
        // then uppercase, then how the shell was invoked.
        case("dollar-dash", r#"echo "$-""#),
        case("dollar-dash-with-options-set", r#"set -e -f -u -x -C -m -T -E; echo "$-""#),
        // `select` reads through the loop's redirect, like `read`. An empty
        // list prints nothing and leaves at once.
        case("select-reads-its-own-redirect", r#"select x in a b; do echo "[$x][$REPLY]"; break; done <<< "2""#),
        case("select-of-nothing", r#"select x in; do echo hi; done; echo "rc=$?""#),
        case("select-at-end-of-input", r#"select x in a b; do break; done < /dev/null; echo done"#),
        case("select-without-an-in-clause", r#"set -- p q; select x; do echo "[$x]"; break; done <<< "2""#),
        // `read`'s short options cluster: `-ra` is two options.
        case("read-clustered-short-options", r#"read -ra p <<< "a b c"; echo "${#p[@]}${p[2]}""#),
        case("read-clustered-option-with-its-value", r#"read -rn2 v <<< "abcd"; echo "[$v]""#),
        // Without `-r`, a backslash escapes the next character, including a
        // separator, which then stays in its field, and the delimiter, which
        // continues the line.
        case("read-without-r-takes-escapes-off", r#"read v <<< 'a\tb'; echo "[$v]"; read -r v <<< 'a\tb'; echo "[$v]""#),
        case("read-without-r-keeps-an-escaped-separator", r#"read a b <<< 'x\ y z'; echo "[$a][$b]""#),
        case("read-without-r-continues-a-line", "printf 'a\\\\\nb\\n' | { read v; echo \"[$v]\"; }"),
        case("integer-append-is-addition", r#"declare -i I=3; I+=2; echo $I; declare -i J; J+=5; echo $J; x=a; x+=b; echo $x"#),
        // Unsetting a local leaves the name unset for the rest of the
        // function, without uncovering the global.
        case("unset-a-local-does-not-uncover-the-global", r#"f() { local x=1; unset x; echo "[${x-gone}]"; }; x=outer; f; echo "[$x]""#),
        case("assign-after-unsetting-a-local", r#"f() { local x=1; unset x; x=2; echo "[$x]"; }; x=outer; f; echo "[$x]""#),
        // A pseudo-trap fires at the depth it was set and in what it
        // returns into, not in what it calls. That's what "not inherited"
        // means, and what `functrace` turns off.
        case("return-trap-set-inside-a-function", r#"f() { trap "echo RET" RETURN; :; }; f"#),
        case("return-trap-set-at-the-top-level", r#"trap "echo RET" RETURN; f() { :; }; f; echo end"#),
        case("return-trap-under-functrace", r#"set -T; trap "echo RET" RETURN; f() { :; }; f"#),
        case("return-trap-does-not-follow-a-call", r#"f() { trap "echo RET" RETURN; g; }; g() { echo ing; }; f"#),
        case("return-trap-fires-on-the-way-out", r#"g() { trap "echo RET" RETURN; :; }; f() { g; echo after; }; f"#),
        // `trap -p` reports the three pseudo-signals too, and only those
        // asked for.
        case("trap-p-of-a-pseudo-signal", r#"trap "echo x" RETURN; trap -p RETURN; trap "echo y" ERR; trap -p ERR"#),
        case("trap-p-names-only-what-it-was-asked", r#"trap "echo a" USR1; trap "echo b" USR2; trap -p USR2"#),
        case("trap-p-of-an-unknown-signal", r#"trap -p NOSUCH 2>/dev/null; echo "rc=$?""#),
        // Bases above 36 use bash's digit set (see `arith::base_number`).
        case("arithmetic-bases-above-36", r#"echo $((64#zZ)) $((64#@)) $((64#_)) $((62#z))"#),
        // A literal runs to the end of the digit-set run and is checked
        // against its base afterwards: `12a` and `1e2` are errors.
        case(
            "a-literal-is-checked-after-it-is-read",
            r#"( echo $((1e2)) ) 2>/dev/null; echo "rc=$?"; ( echo $((12a)) ) 2>/dev/null; echo "rc=$?"; ( echo $((0b101)) ) 2>/dev/null; echo "rc=$?"; ( echo $((08)) ) 2>/dev/null; echo "rc=$?""#,
        ),
        case("a-bare-hex-prefix-is-zero", r#"echo $((0x)) $((0X)) $((00)) $((010)) $((0x1f))"#),
        // `${!name}` is fatal, like `${x:?}`, when `name` is unset, empty,
        // or not a parameter name.
        case("indirect-expansion-of-an-unset-name", r#"echo "[${!nosuch}]"; echo after"#),
        case("indirect-expansion-of-an-empty-name", r#"x=""; echo "[${!x}]"; echo after"#),
        case("indirect-expansion-of-a-non-name", r#"x="1bad"; echo "[${!x}]"; echo after"#),
        case("indirect-expansion-follows-a-subscript", r#"a=(u v); x="a[1]"; echo "[${!x}]"; declare -A m=([k]=w); y="m[k]"; echo "[${!y}]""#),
        case("indirect-expansion-of-a-positional", r#"set -- p q; x=1; echo "[${!x}]"; x=@; echo "[${!x}]"; x=#; echo "[${!x}]""#),
        case("arithmetic-bases-fold-case-below-37", r#"echo $((16#FF)) $((16#ff)) $((36#ZZ)) $((36#zz))"#),
        // In a subshell, so the message can be redirected away: it's printed
        // during expansion, before the command's own `2>` applies.
        case(
            "arithmetic-base-out-of-range",
            r#"( echo $((65#a)) ) 2>/dev/null; echo "rc=$?"; ( echo $((37#Z)) ) 2>/dev/null; echo "rc=$?"; ( echo $((1#a)) ) 2>/dev/null; echo "rc=$?""#,
        ),
        // `${a[@]OP}` applies OP to each element, not to the joined text.
        case("array-wide-replace-is-per-element", r#"a=(one two three); echo "${a[@]/o/0}"; echo "${a[@]//o/0}""#),
        case("array-wide-strip-is-per-element", r#"a=(one two); echo "${a[@]%e}"; echo "${a[@]#o}"; a=(a.txt b.txt); echo "${a[@]%.txt}""#),
        // POSIX character classes, in both matchers: `[:name:]` carries its
        // own `]`, which doesn't close the bracket around it.
        case(
            "character-classes-in-a-regex",
            r#"[[ "a b" =~ [[:space:]] ]] && echo s; [[ ab =~ ^[[:alpha:]]+$ ]] && echo a; [[ 5 =~ [^[:digit:]] ]] || echo neg"#,
        ),
        case(
            "character-classes-in-a-glob",
            r#"case "a b" in *[[:space:]]*) echo s;; esac; case a] in [[:alpha:]]]) echo bracket;; esac; x="a b c"; echo "${x//[[:space:]]/_}""#,
        ),
        // Quoting decides whether a metacharacter in a `${v/pat}` pattern is
        // syntax or text, as on the right of `[[ x == pat ]]`.
        case(
            "quoting-in-a-parameter-expansion-pattern",
            r#"x="a*b"; echo "${x//\*/Y}" "${x//"*"/Y}" "${x#a\*}" "${x%\*b}" "${x#"a*"}"; y=axb; echo "${y#a\*}" "${y//\?/Y}""#,
        ),
        // ...including inside a bracket expression, where `]`, `^` and `-`
        // are the class's syntax.
        case(
            "escaped-characters-inside-a-pattern-bracket",
            r#"x=a^b; echo "${x//[\^]/X}"; y=aqz; echo "${y//[a\-z]/X}"; z="a]b"; echo "${z//[\]]/X}"; w=a-b; echo "${w//[\-]/X}""#,
        ),
        // A quoted `&` in a replacement is an ampersand on the array and
        // positional paths too.
        case(
            "an-escaped-ampersand-in-a-replacement-is-literal",
            r#"a=(abc); echo "${a[@]//b/\&}" "${a[0]//b/\&}" "${a[@]//b/[&]}"; set -- abc; echo "${1//b/\&}" "${@//b/\&}"; r="&"; echo "${a[@]//b/$r}""#,
        ),
        // A match that reaches the end of the string ends the walk (bash's
        // loop is `while (*s)`), so `${s//*/Z}` replaces once.
        case(
            "a-replacement-match-that-reaches-the-end-of-the-string",
            r#"s=abc; echo "${s//*/Z}" "${s//c*/Z}" "${s//b*/Z}" "${s//?*/Z}"; t=abab; echo "${t//a/Z}"; u=aa; echo "${u//a*/Z}""#,
        ),
        // An empty pattern matches nothing unanchored, though `/#` and `/%`
        // still fire. A pattern that can match empty, like `@()`, does
        // splice; it needs extglob, so it's in a unit test.
        case(
            "an-empty-replacement-pattern-matches-nothing",
            r#"s=abc; echo "${s//""/Z}" "${s/""/Z}" "${s/#""/Z}" "${s/%""/Z}"; e=; echo "${s//$e/Z}""#,
        ),
        // A fixed-width pattern wider than the string can't match.
        case(
            "a-replacement-pattern-longer-than-the-string",
            r#"s=ab; echo "[${s//abc/Z}]" "[${s//[ab]c/Z}]"; e=; echo "[${e//?/Z}]" "[${e//[a]/Z}]" "[${e//a/Z}]""#,
        ),
        // `kill -0` probes whether a process exists, and `-s`/`-n` take the
        // signal from the next argument.
        case("kill-signal-zero", r#"kill -0 $$ && echo alive; kill -s 0 $$ && echo alive2; kill -n 0 $$ && echo alive3"#),
        case("kill-of-a-process-that-is-gone", r#"kill -0 999999 2>/dev/null; echo "rc=$?""#),
        // `-o OPTNAME` is a unary test. A connective needs something on its
        // left, so at the start of a clause `-o` is the test.
        case("test-o-asks-about-a-shell-option", r#"test -o errexit; echo "rc=$?"; set -e; test -o errexit; echo "rc=$?""#),
        case("test-o-still-combines", r#"set -e; set -u; [ -o errexit -a -o nounset ] && echo both; [ a -o b ] && echo or"#),
        case("test-v-in-the-bracket-form", r#"x=1; [ -v x ] && echo set; [ -v nosuch ]; echo "rc=$?""#),
        case("xtrace-quotes-what-needs-it", r#"set -x; echo "a b"; echo "*"; echo ""; [ x = x ]"#),
        // `-P` forces the PATH search past a builtin of the same name;
        // `-p` prints a path only where `type` would say "file".
        case("type-capital-p-forces-the-path-search", r#"type -P echo; type -p echo; f(){ :; }; type -P f; echo "rc=$?""#),
        // `mapfile -d` sets the delimiter, and its options cluster: `-d,`.
        case("mapfile-delimiter", r#"mapfile -t -d, arr <<< "a,b,c"; printf "[%s]" "${arr[@]}"; echo"#),
        case("mapfile-clustered-options", r#"mapfile -td, arr <<< "a,b,c"; echo "${#arr[@]}""#),
        // A quoted part of a `case` pattern is literal, so a variable
        // holding `*` matches only `*`.
        case(
            "a-quoted-case-pattern-is-literal",
            r#"p="*"; case abc in "$p") echo lit;; *) echo no;; esac; case abc in '*') echo sq;; *) echo no2;; esac"#,
        ),
        case(
            "an-unquoted-case-pattern-is-still-a-glob",
            r#"p="*"; case abc in $p) echo glob;; *) echo no;; esac; x=/tmp; case /tmp/f in "$x"/*) echo prefix;; esac"#,
        ),
        // Variables the shell computes. `$BASHPID` equals `$$` here even in
        // a subshell, which isn't a process of its own, so this asks only
        // what both shells agree on.
        case("bash-subshell-counts-nesting", r#"echo "$BASH_SUBSHELL"; (echo "$BASH_SUBSHELL"; (echo "$BASH_SUBSHELL"))"#),
        case("bashpid-at-the-top-level", r#"[ "$BASHPID" = "$$" ] && echo same"#),
        case("shellopts-lists-what-is-on", r#"[[ $SHELLOPTS == *errexit* ]] || echo off; set -e; [[ $SHELLOPTS == *errexit* ]] && echo on"#),
        case("bashopts-lists-what-is-on", r#"[[ $BASHOPTS == *dotglob* ]] || echo off; shopt -s dotglob; [[ $BASHOPTS == *dotglob* ]] && echo on"#),
        // A computed variable is still a name, so `${!PREFIX*}` finds it.
        case("prefix-listing-finds-computed-names", r#"echo "${!BASHO*}" "${!BASHP*}" "${!BASH_SUB*}" "${!EUI*}""#),
        case("bash-versinfo-is-readonly", r#"declare -p BASH_VERSINFO | cut -c1-14"#),
        // An EXIT trap fires only for the exit of the shell that armed it.
        // A subshell inherits it, and `trap -p EXIT` shows it there, but
        // ending a subshell isn't that exit. The trap is nearly always a
        // cleanup, so firing early removes what the script still uses.
        case("an-exit-trap-does-not-fire-in-a-subshell", r#"trap "echo E" EXIT; ( echo s ); echo m"#),
        case("an-exit-trap-does-not-fire-in-a-substitution", r#"trap "echo E" EXIT; x=$(echo s); echo "$x""#),
        case("an-exit-trap-does-not-fire-per-pipeline-stage", r#"trap "echo E" EXIT; echo a | cat; echo m"#),
        case("an-exit-trap-set-in-a-subshell-fires-there", r#"( trap "echo E" EXIT; echo s ); echo m; echo a | { trap "echo P" EXIT; cat; }"#),
        case("an-inherited-exit-trap-is-still-visible", r#"trap "echo E" EXIT; ( trap -p EXIT ); echo m"#),
        case(
            "the-cleanup-idiom-survives-a-substitution",
            r#"d=$(mktemp -d); trap "rmdir $d" EXIT; x=$(echo hi); [ -d "$d" ] && echo "still there: $x""#,
        ),
        // A positional is set only when there is one at that position, so
        // `set -u` covers `$1`.
        case("nounset-covers-the-positionals", r#"set -u; f() { echo "$1"; }; f"#),
        case("nounset-leaves-the-specials-alone", r#"set -u; echo "$@$*$#"; echo "${1-d}"; f() { echo "${1:-e}"; }; f; echo ok"#),
        case("a-positional-past-the-end-is-unset", r#"set -- a; echo "[${1+set}][${2+set}]"; set -u; echo "$2""#),
        // The right-hand side of an array assignment is expanded before the
        // old value goes, so `a=("${a[@]/x/y}")` rewrites in place.
        case("an-array-can-be-built-from-itself", r#"a=(1 2); a=("${a[@]/1/9}"); echo "${a[*]}"; b=(1 2); b=("${b[@]}" 3); echo "${b[*]}""#),
        case("an-array-can-be-prepended-to", r#"a=(1 2); a=(0 "${a[@]}"); echo "${a[*]}"; a+=("${a[@]}"); echo "${a[*]}""#),
        // The whole arithmetic expression is expanded before evaluation, so
        // `$((10#$x))` forces base ten on a zero-padded number.
        case("a-base-prefix-in-front-of-an-expansion", r#"x=010; echo $((10#$x)) $((16#$x)); v=07; echo $((10#$v)) $((8#$v))"#),
        case("an-expansion-inside-arithmetic", r#"x=1+1; echo $((x)) $(($x)); n=3; echo $(( n * $n )); echo $((2#$(echo 101)))"#),
        // The declare family's flags cluster, and `local` takes `-r`.
        case("declare-clusters-its-flags", r#"declare -ix n=1; echo "${n@a}"; declare -ir m=1; declare -p m; declare -ax a=(1); declare -p a"#),
        case("local-clusters-its-flags", r#"f(){ local -ir v=1; declare -p v; local -ax a=(1); declare -p a; }; f"#),
        case("a-readonly-local-is-readonly", r#"f(){ local -r v=1; v=2; }; f 2>/dev/null; echo "rc=$?""#),
        // `declare -F name` prints the bare name; `declare -F` with no
        // names prints a re-readable `declare -f NAME` line for each.
        case("declare-capital-f-with-and-without-a-name", r#"f(){ :; }; g(){ :; }; declare -F f; declare -F | head -2"#),
        case("the-upper-first-transform", r#"x=abc; echo "${x@u}${x@U}${x@L}"; a=(one two); echo "${a[@]@u}""#),
        // `umask -p` prints the command that would set it again.
        case("umask-p-is-re-readable", r#"umask 022; umask -p; umask -S; umask -pS"#),
        // The working directory and the umask belong to the process, which
        // a `( )` or `$( )` shares with the shell around it, so each is put
        // back on the way out. The restore relies on whatever moves them
        // leaving a note (see ProcessRestore), and these catch a `cd` or
        // `umask` that doesn't. fds 0-2 are pinned in tests/.
        //
        // Builtins only: an external inside `( )` loses its output in a
        // pane, which is a separate defect.
        case(
            "a-cd-inside-a-subshell-does-not-escape",
            r#"mkdir -p a b; cd a; ( cd ../b ); echo "${PWD##*/}"; v=$(cd ../b && echo "${PWD##*/}"); echo "$v ${PWD##*/}""#,
        ),
        case(
            "a-cd-inside-a-nested-subshell-does-not-escape",
            r#"mkdir -p a b c; cd a; ( cd ../b; ( cd ../c; echo "${PWD##*/}" ); echo "${PWD##*/}" ); echo "${PWD##*/}""#,
        ),
        case("a-umask-inside-a-subshell-does-not-escape", r#"umask 022; ( umask 077 ); umask; v=$(umask 077; umask); echo "$v $(umask)""#),
        case("a-umask-inside-a-nested-subshell-does-not-escape", r#"umask 022; ( umask 077; ( umask 002; umask ); umask ); umask"#),
        // ...and a subshell that changed nothing has nothing to put back.
        case(
            "a-subshell-that-moves-nothing-leaves-everything",
            r#"mkdir -p a; cd a; umask 022; ( : ); v=$(:); echo "${PWD##*/}"; umask; echo "[$v]after""#,
        ),
        // `printf -v 'arr[0]'` writes an array element.
        case(
            "printf-v-into-an-array-element",
            r#"for i in 0 1; do printf -v "a[$i]" "v$i"; done; echo "${a[*]}"; declare -A m; printf -v "m[k]" z; echo "${m[k]}""#,
        ),
        // `$BASH_COMMAND` is what a DEBUG or ERR trap reads to say what it
        // fired for.
        case("bash-command-in-an-err-trap", r#"trap "echo E:\$BASH_COMMAND" ERR; grep -q zz /dev/null"#),
        case("bash-command-names-an-assignment-as-written", r#"trap "echo D:\$BASH_COMMAND" DEBUG; x=1"#),
        case("bash-command-in-an-err-trap-from-a-function", r#"f(){ false; }; trap "echo E:\$BASH_COMMAND" ERR; f"#),
        case("bash-command-in-a-debug-trap", r#"trap "echo D:\$BASH_COMMAND" DEBUG; true; echo two"#),
        // PS4 is expanded before it's printed.
        case("ps4-expands-a-parameter", r#"PS4='+$LINENO '; set -x; echo t"#),
        case("ps4-expands-a-substitution", r#"PS4='$(echo S) '; set -x; echo t"#),
        // `unset PS4` traces with no prefix at all.
        case("ps4-unset-is-no-prefix", r#"unset PS4; set -x; echo t"#),
        case("ps4-defaults-to-plus", r#"set -x; echo t"#),
        // For a nameref, `${!x}` names what the reference points at,
        // following the chain to the end, rather than indirecting.
        case("indirect-expansion-of-a-nameref", r#"declare -n r=v; v=x; echo "${!r}"; echo "$r""#),
        case("indirect-expansion-follows-a-nameref-chain", r#"declare -n a=b; declare -n b=c; c=1; echo "${!a}""#),
        case("a-nameref-parameter-names-its-argument", r#"f() { local -n r=$1; echo "${!r}"; }; v=x; f v"#),
        // A subshell resets the traps it caught, so a DEBUG trap doesn't fire
        // per command inside `$( )`, and an ERR trap doesn't fire both inside
        // `( false )` and for the subshell. A pipeline stage isn't reset.
        case("a-debug-trap-does-not-follow-a-substitution", r#"trap "echo D" DEBUG; x=$(echo hi); echo done"#),
        case(
            "an-err-trap-does-not-fire-twice-for-a-subshell",
            r#"trap "echo E" ERR; ( false ); echo done; trap "echo F" ERR; x=$(false); echo done"#,
        ),
        case("a-return-trap-does-not-follow-a-substitution", r#"trap "echo R" RETURN; x=$(echo hi); echo done"#),
        // `&` makes a job, whatever the command: a builtin, function,
        // group, loop or bare assignment runs in a child too, sets `$!`,
        // and is collected by `wait`.
        //
        // Nothing here asserts an order between a background job's output
        // and the shell's next; that's a race in both shells. `wait` first,
        // then look.
        case("backgrounding-a-builtin", r#": & wait -n; echo "rc=$?"; false & wait -n; echo "rc=$?""#),
        case("backgrounding-a-function", r#"f(){ echo a; }; f & wait -n; echo "rc=$?""#),
        case("an-exit-in-a-background-job-is-the-jobs-exit", r#"f(){ exit 5; }; f & wait -n; echo "rc=$?"; echo alive"#),
        case("a-background-group-does-not-touch-this-shell", r#"v=0; { v=1; } & wait; echo "[$v]""#),
        case("a-background-function-does-not-touch-this-shell", r#"v=0; f(){ v=1; }; f & wait; echo "[$v]""#),
        case("a-background-assignment-does-not-touch-this-shell", r#"x=0; x=1 & wait; echo "[$x]""#),
        case("backgrounding-a-loop", r#"for i in 1; do echo $i; done & wait -n; echo "rc=$?""#),
        case("backgrounding-a-conditional", r#"if true; then echo a; fi & wait -n; echo "rc=$?""#),
        case("a-backgrounded-builtin-sets-the-pid-variable", r#"echo "[$!]"; : & echo "[${!:+pid}]""#),
        case("a-backgrounded-builtin-keeps-its-redirect", r#"echo a > o & wait; cat o"#),
        case("two-background-jobs-are-both-collected", r#": & : & wait -n; echo "rc=$?"; wait -n; echo "rc=$?""#),
        // `declare -f`'s layout is bish's own (a recorded divergence), but
        // its output still has to define the function again in another
        // shell with a command straight after it:
        // `ssh host "$(declare -f f); f"`.
        case("a-function-definition-can-be-shipped-to-another-shell", r#"f(){ echo hi; }; bash -c "$(declare -f f); f""#),
        case(
            "a-shipped-function-keeps-its-compound-commands",
            r#"f() { local x=1; if [ "$x" = 1 ]; then echo "yes $x"; fi; for i in a b; do echo $i; done; }; bash -c "$(declare -f f); f""#,
        ),
        case("two-shipped-functions", r#"f(){ echo a; }; g(){ echo b; }; bash -c "$(declare -f f g); f; g""#),
        // A `;` where a command is expected is a syntax error, before the
        // first command as much as between two. Through `eval`, so the
        // script keeps running and the status can be checked.
        case("empty-command-between-separators", r#"eval "echo a; ; echo b" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon", r#"eval "; echo a" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon-in-a-group", r#"eval "{ ; echo a; }" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon-in-a-then-branch", r#"eval "if true; then ; echo x; fi" 2>/dev/null; echo "rc=$?""#),
        case("semicolon-after-a-background-ampersand", r#"eval "echo a &; echo b" 2>/dev/null; echo "rc=$?""#),
        // Blank lines are skippable anywhere, and an empty case arm is
        // still an empty case arm.
        case("blank-lines-between-commands", "eval 'echo a\n\n\necho b' 2>/dev/null; echo \"rc=$?\""),
        case("an-empty-case-arm-is-not-an-empty-command", r#"eval "case x in x) ;; esac" 2>/dev/null; echo "rc=$?""#),
    ];

    // Cases bish doesn't match today, each with why. Asserted to still
    // diverge, so fixing one fails this test until its line is removed.
    const DIVERGENCES: &[(&str, &str)] = &[
        // `( )` and `$( )` run in this shell, so there's no second process to
        // have a pid; a made-up one would break `kill $BASHPID`.
        ("bashpid-is-the-shells-own-in-a-subshell", "`( echo $BASHPID )` names this shell; bash forks a subshell, so its BASHPID differs from `$$`"),
        // Extended globbing is always on here, so `shopt -u extglob` can't
        // turn it off; honouring it would mean a second, weaker matcher.
        ("extglob-cannot-be-turned-off", "`shopt extglob` says on where bash says off, because here it is always on"),
        // `set -o` lists only the options that do something here. Listing
        // `allexport off` would read as support for it.
        ("set-o-lists-fewer-options", "`set -o` lists 10 options; bash lists 27, most of which bish does not implement"),
        // bish has builtins bash doesn't (`abbr`, `win`, `::bish`) and no
        // `bind`: key bindings are bish's keymap, not readline's.
        ("compgen-b-lists-this-shells-builtins", "`compgen -b` lists bish's own builtins and not bash's `bind`"),
        // Both shells agree on where the whole match lands: leftmost, then
        // longest. They differ on which spelling of an equally long match
        // the groups report. bash's answer is glibc's, which isn't
        // consistently POSIX either; this engine breaks the tie by branch
        // order. The differences are all patterns with two ways to spell
        // the same match, mostly an empty alternation branch.
        (
            "regex-submatch-of-an-ambiguous-empty-branch",
            "`(|a)(a|)` against `a` fills the first group in bash and the second here; the whole match agrees",
        ),
    ];

    // The divergent cases, apart from `CASES` so that list is only what
    // works.
    const PENDING: &[Case] = &[
        case("set-o-lists-fewer-options", r#"set -o | wc -l"#),
        case("bashpid-is-the-shells-own-in-a-subshell", r#"echo $(( $$ == BASHPID )); ( echo $(( $$ == BASHPID )) )"#),
        case("extglob-cannot-be-turned-off", r#"shopt extglob; shopt -u extglob; shopt -q extglob; echo "q=$?""#),
        case("compgen-b-lists-this-shells-builtins", r#"compgen -b | sort | head -3 | tr '\n' ' '; echo"#),
        case("regex-submatch-of-an-ambiguous-empty-branch", r#"re='(|a)(a|)'; [[ a =~ $re ]]; echo "[${BASH_REMATCH[1]}][${BASH_REMATCH[2]}]""#),
        // Not recordable as a case: through two levels of `-c`, `SHLVL`
        // counts one higher in bish, because bash decrements it before
        // `exec`ing the last command and bish spawns instead. Seeing it
        // needs the shell invoked by path, and both shells report `$0` as a
        // bare name.
    ];

    /// `target/<profile>/bish`, worked out from the test binary's path
    /// (`target/<profile>/deps/bish-<hash>`). Kept current by
    /// `tests/the_binary_under_test_is_the_one_just_built.rs`; without it
    /// `cargo test` would leave whatever `cargo build` last made. Missing,
    /// the tests skip, as they do without bash.
    // A signal ignored before the shell started is the shell's to report
    // and not to change, as in bash. The disposition has to be SIG_IGN when
    // the shell is exec'd, as `nohup` leaves SIGHUP, so this can't be an
    // ordinary corpus case.
    #[test]
    fn a_signal_ignored_before_the_shell_started_is_listed_and_cannot_be_trapped() {
        let Some(bish) = bish_binary() else { return };
        // The listing first (HUP is signal 1, so it sorts above the INT
        // trap), then a trap on it, which succeeds and has no effect.
        let script = r#"trap 'echo t' INT; trap -p; trap 'echo h' HUP; echo "status=$?"; trap -p HUP"#;
        let dir = std::env::temp_dir().join(format!("bish-entry-ignored-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let got = run_with_sighup_ignored(bish.as_os_str(), script, &dir);
        assert_eq!(got, "trap -- '' SIGHUP\ntrap -- 'echo t' SIGINT\nstatus=0\ntrap -- '' SIGHUP\n");
        if let Some(bash) = oracle_bash() {
            assert_eq!(
                got,
                run_with_sighup_ignored(bash.as_os_str(), script, &dir),
                "bish and bash must say the same thing about an inherited ignore"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // A shell's state must not be readable by other users. bish re-execs for
    // a background subshell, a coproc and some pipeline stages, and the child
    // needs the parent's functions and variables. In argv they'd show in
    // `/proc/<pid>/cmdline`, mode 0444. Linux only.
    #[test]
    fn a_backgrounded_subshell_does_not_publish_the_shells_variables() {
        let Some(bish) = bish_binary() else { return };
        if !std::path::Path::new("/proc/self/cmdline").exists() {
            return;
        }
        let script = r#"SECRET=hunter2; ( sleep 0.4 ) & p=$!; sleep 0.1; tr '\0' '\n' < /proc/$p/cmdline | grep -c hunter2"#;
        let dir = std::env::temp_dir().join(format!("bish-proc-leak-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let got = run_with_sighup_ignored(bish.as_os_str(), script, &dir);
        // `grep -c` says 0 and exits 1, so the text is the whole answer.
        assert_eq!(got.trim(), "0", "bish published its own variables in /proc: {got:?}");
        if let Some(bash) = oracle_bash() {
            assert_eq!(run_with_sighup_ignored(bash.as_os_str(), script, &dir).trim(), "0", "the control: bash must also show nothing");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `run_with`, with SIGHUP ignored when the shell starts. It's set
    /// between fork and exec; SIG_IGN is one of the two dispositions that
    /// survive `exec`, which is why the rule under test exists. `dir` is
    /// HOME and cwd, as in `run_with`.
    fn run_with_sighup_ignored(shell: &std::ffi::OsStr, script: &str, dir: &std::path::Path) -> String {
        let mut command = Command::new(shell);
        command
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()))
            .env("LC_ALL", "C")
            .env("HOME", dir)
            .env("PS1", "$ ")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        use std::os::unix::process::CommandExt;
        let sighup = crate::term::SIGHUP;
        // The Rust runtime ignored SIGPIPE before `main` (see
        // `record_signals_ignored_at_entry`), so it's restored here or the
        // child inherits it and bash lists it too. Looked up before
        // `pre_exec` so the closure only makes syscalls.
        let sigpipe = crate::exec::signal_number("PIPE").expect("every platform names PIPE");
        // SAFETY: two sigaction calls, which are async-signal-safe and
        // allocate nothing.
        unsafe {
            command.pre_exec(move || {
                crate::platform::default_signal(sigpipe);
                crate::platform::ignore_signal(sighup);
                Ok(())
            });
        }
        let (out, timed_out) = command.spawn().and_then(|child| wait_with_timeout(child, CASE_TIMEOUT)).expect("the shell under test must run");
        // A killed shell's partial output would otherwise read as an ordinary
        // mismatch.
        assert!(!timed_out, "{shell:?} did not finish within {CASE_TIMEOUT:?}");
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    }

    fn bish_binary() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.parent()?.join("bish");
        path.exists().then_some(path)
    }

    /// The oldest bash worth comparing against. 3.2, which macOS ships,
    /// predates much of what the corpus uses (`declare -A`, `mapfile`,
    /// `coproc`, `${v@Q}`, `wait -n`, ...) and would report bash's age as
    /// divergences.
    const OLDEST_USABLE_BASH: u32 = 4;

    /// What the oracle is spawned as: a bare name, resolved through each
    /// case's PATH. bash prefixes its errors with its `argv[0]`, and the
    /// corpus normalises `bash: ` away; spawned by path it would say
    /// `/usr/bin/bash: line 1: ...`.
    fn oracle() -> &'static std::ffi::OsStr {
        std::ffi::OsStr::new("bash")
    }

    /// The bash to compare against, or `None` when there's none worth it.
    /// Looked up on PATH and its version asked, so a newer bash installed
    /// ahead of the system one is used. Resolved once per test binary.
    fn oracle_bash() -> Option<&'static std::path::Path> {
        static FOUND: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        FOUND
            .get_or_init(|| {
                let path = crate::toolpath::find("bash")?;
                let version = Command::new(&path).arg("-c").arg("echo ${BASH_VERSINFO[0]}").output().ok()?;
                let major: u32 = String::from_utf8_lossy(&version.stdout).trim().parse().ok()?;
                (major >= OLDEST_USABLE_BASH).then(|| PathBuf::from(path))
            })
            .as_deref()
    }

    // Both shells get the same scratch directory as HOME and cwd and the
    // same minimal environment. `env_clear` matters because a shell run
    // in-process by another test writes its exports into the real process
    // environment (see `raw_var_write`), so a leaked GLOBIGNORE would fail
    // cases depending on test order.
    //
    // Then killed after a timeout, so a deadlocking case reports instead of
    // wedging the run. The timeout catches deadlocks, not slowness, so it's
    // far longer than any case needs: under load even bash can take
    // seconds.
    const CASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    fn run(shell: &std::ffi::OsStr, script: &str, dir: &std::path::Path) -> Outcome {
        run_with(shell, &[], script, dir)
    }

    /// The grid `--promoted -c` runs a case in; main.rs's
    /// `run_source_in_a_pane` has to agree.
    const PANE_ROWS: usize = 400;
    const PANE_COLS: usize = 400;

    /// Waits for a file to stop being written to: length-stability rather
    /// than a fixed sleep, so a finished consumer costs one poll and a slow
    /// one is waited for while it produces.
    fn settle(path: &std::path::Path) {
        // Five quiet polls: a consumer that hasn't written yet leaves the
        // file empty across two close reads, which would look finished.
        const QUIET: usize = 5;
        let poll = std::time::Duration::from_millis(20);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut last = None;
        let mut quiet = 0;
        while std::time::Instant::now() < deadline && quiet < QUIET {
            std::thread::sleep(poll);
            let now = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            quiet = if last == Some(now) { quiet + 1 } else { 0 };
            last = Some(now);
        }
    }

    /// `run`, with both streams written to one file in the order they were
    /// written, as a pane's grid receives them. Two descriptors on one file
    /// share an offset, so bash interleaves them exactly as a terminal
    /// would see them.
    fn run_merged(shell: &std::ffi::OsStr, options: &[&str], script: &str, dir: &std::path::Path) -> Outcome {
        // Outside the case's directory, since cases glob their cwd, and
        // unique per call, since the two pane tests run the same cases
        // concurrently.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("bish-bashdiff-merged-{}-{n}", std::process::id()));
        let Ok(file) = std::fs::File::create(&path) else {
            return Outcome { text: "<could not open the merged output file>".to_string(), timed_out: false };
        };
        let Ok(errors) = file.try_clone() else {
            return Outcome { text: "<could not clone the merged output file>".to_string(), timed_out: false };
        };
        let out = Command::new(shell)
            .args(options)
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()))
            .env("LC_ALL", "C")
            .env("HOME", dir)
            .env("PS1", "$ ")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(errors))
            .spawn()
            .and_then(|child| wait_with_timeout(child, CASE_TIMEOUT));
        let timed_out = match out {
            Ok((_, timed_out)) => timed_out,
            Err(e) => return Outcome { text: format!("<could not run: {e}>"), timed_out: false },
        };
        // The shell exiting doesn't mean the output is complete: a `>( )`
        // consumer holds the file open and can write after the shell is
        // gone. So the file gets a moment to stop growing, only for scripts
        // that can have such a writer, since the wait costs real time.
        if script.contains(">(") {
            settle(&path);
        }
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        Outcome { text: normalise(&as_a_terminal_would_show_it(&text)), timed_out }
    }

    /// What a terminal would show after `text` was written to it: bash's
    /// bytes through the same vt100 grid as a pane's. A grid expands tabs,
    /// has no trailing spaces and wraps long lines, and those aren't
    /// differences between the shells. The same size as the case's pane, so
    /// a line wraps in both or neither.
    fn as_a_terminal_would_show_it(text: &str) -> String {
        let mut screen = crate::vt100::Screen::new(PANE_ROWS, PANE_COLS);
        // A pipe's `\n` is a terminal's `\r\n`; the grid sink does the same
        // to everything a shell writes (exec.rs's `onlcr`).
        screen.feed(text.replace('\n', "\r\n").as_bytes());
        screen.text_unwrapped()
    }

    // `run`, with options ahead of `-c`: the pane corpus passes `--promoted`.
    fn run_with(shell: &std::ffi::OsStr, options: &[&str], script: &str, dir: &std::path::Path) -> Outcome {
        let out = Command::new(shell)
            .args(options)
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()))
            .env("LC_ALL", "C")
            .env("HOME", dir)
            .env("PS1", "$ ")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|child| wait_with_timeout(child, CASE_TIMEOUT));
        match out {
            Ok((out, timed_out)) => {
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                Outcome { text: normalise(&text), timed_out }
            }
            Err(e) => Outcome { text: format!("<could not run: {e}>"), timed_out: false },
        }
    }

    /// What one shell did with one case.
    ///
    /// `timed_out` is separate from the text because a hung case is killed
    /// and reports what it printed, which is often the right answer.
    /// Comparing text alone would call those passes.
    struct Outcome {
        text: String,
        timed_out: bool,
    }

    // Each shell names itself in an error, and bash adds a line number. A
    // case about an error is about whether it happened and what it said.
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

    // `Child::wait_with_output` with a deadline, killing the child when it
    // runs out. Output so far is still collected.
    fn wait_with_timeout(mut child: std::process::Child, limit: std::time::Duration) -> std::io::Result<(std::process::Output, bool)> {
        let deadline = std::time::Instant::now() + limit;
        let mut timed_out = false;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok((child.wait_with_output()?, timed_out))
    }

    fn compare(cases: &[Case], bish: &std::path::Path) -> Vec<(&'static str, String, String)> {
        compare_with(cases, bish, &[])
    }

    /// `compare`, running bish with `options` ahead of `-c`. With
    /// `--promoted` a case runs the way a split or tabbed window runs one
    /// (see main.rs's `run_source_in_a_pane`); bash runs as in the ordinary
    /// corpus.
    fn compare_with(cases: &[Case], bish: &std::path::Path, options: &[&str]) -> Vec<(&'static str, String, String)> {
        // A root per call, not per process: tests running the same cases
        // concurrently would delete each other's working directories.
        static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let attempt = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("bish-bashdiff-{}-{attempt}", std::process::id()));
        let mut differing = Vec::new();
        for case in cases {
            // A directory per case: cases create files, and a glob is only
            // predictable in a directory it owns.
            let dir = root.join(case.name);
            std::fs::create_dir_all(&dir).unwrap();
            // A pane merges the two streams, so bash is read the same way.
            let merged = !options.is_empty();
            let want = match merged {
                true => run_merged(oracle(), &[], case.script, &dir),
                false => run(oracle(), case.script, &dir),
            };
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let got = run_with(bish.as_os_str(), options, case.script, &dir);
            std::fs::remove_dir_all(&dir).ok();
            // A hung case is killed and reports what it printed, often
            // exactly right, so a timeout is reported on its own.
            if got.timed_out || want.timed_out {
                let who = match (want.timed_out, got.timed_out) {
                    (true, true) => "both shells hung",
                    (true, false) => "bash hung (bish did not)",
                    _ => "bish hung",
                };
                differing.push((case.name, format!("<{who}>"), format!("{}\n<killed after {CASE_TIMEOUT:?}>", got.text)));
                continue;
            }
            if want.text != got.text {
                differing.push((case.name, want.text, got.text));
            }
        }
        std::fs::remove_dir_all(&root).ok();
        differing
    }

    /// Losing the oracle quietly is the failure this guards against. Every
    /// corpus test returns early without a usable bash, and a run that skips
    /// every case still reports `ok`.
    ///
    /// macOS is accepted for now: its system bash is 3.2, too old to compare
    /// against. A newer one ahead of it on PATH (Homebrew's) makes the corpus
    /// run there. Anywhere else a missing or too-old bash fails the suite;
    /// `BISH_NO_BASH_ORACLE=1` accepts a run without one, in a minimal
    /// container say.
    #[test]
    fn the_corpus_goes_unrun_only_where_that_is_accepted() {
        if oracle_bash().is_some() || std::env::var_os("BISH_NO_BASH_ORACLE").is_some() {
            return;
        }
        if cfg!(target_os = "macos") {
            return;
        }
        panic!(
            "no bash of version {OLDEST_USABLE_BASH} or newer on PATH, so every case comparing bish against bash was skipped.\n     \
             Install one, or set BISH_NO_BASH_ORACLE=1 to accept a run without the oracle."
        );
    }

    /// Every corpus case again, run the way a split or tabbed window runs
    /// one. A pane is a different execution path: a pty per foreground
    /// external, `ExecResult::Fg` handed back to be driven elsewhere, and a
    /// vt100 grid where a pipe would be.
    #[test]
    fn bish_agrees_with_bash_in_a_pane() {
        let Some(bish) = bish_binary() else { return };
        if oracle_bash().is_none() {
            return;
        }
        let cases = pane_cases();
        let differing: Vec<_> = compare_with(&cases, &bish, &["--promoted"])
            .into_iter()
            .filter(|(name, _, _)| !PANE_DIVERGENCES.iter().any(|(known, _)| known == name))
            .collect();
        assert!(
            differing.is_empty(),
            "{} of {} cases differ from bash when run in a pane:\n{}",
            differing.len(),
            cases.len(),
            differing.iter().map(|(name, want, got)| format!("  {name}\n    bash: {want:?}\n    pane: {got:?}")).collect::<Vec<_>>().join("\n")
        );
    }

    /// Cases the pane corpus doesn't run at all.
    const PANE_SKIPPED: &[(&str, &str)] = &[];

    fn pane_cases() -> Vec<Case> {
        CASES.iter().filter(|c| !PANE_SKIPPED.iter().any(|(prefix, _)| c.name.starts_with(prefix))).copied().collect()
    }

    /// Cases whose pane answer differs from bash, with the reason. Each one
    /// claims the difference is the pane's nature, not a bug, and has to
    /// say why.
    const PANE_DIVERGENCES: &[(&str, &str)] = &[
        // A pane is a terminal and bash's side is a pipe, so `ls a b`
        // columns its output in one and not the other. Both are right.
        ("a-superseded-redirect-still-creates-its-file", "`ls a b` columns its output, because a pane is a tty and bash's side here is a pipe"),
    ];

    /// As with `DIVERGENCES`: fixing one fails the suite until its line goes.
    #[test]
    fn the_known_pane_divergences_are_still_divergences() {
        let Some(bish) = bish_binary() else { return };
        if oracle_bash().is_none() {
            return;
        }
        let differing: Vec<&str> = compare_with(&pane_cases(), &bish, &["--promoted"]).into_iter().map(|(name, _, _)| name).collect();
        for (name, why) in PANE_DIVERGENCES {
            assert!(differing.contains(name), "`{name}` agrees with bash in a pane now -- remove its line from PANE_DIVERGENCES ({why})");
        }
    }

    #[test]
    fn every_pane_divergence_names_a_real_case() {
        for (name, _) in PANE_DIVERGENCES {
            assert!(CASES.iter().any(|c| c.name == *name), "`{name}` is listed as a pane divergence with no case to prove it");
        }
        for (prefix, _) in PANE_SKIPPED {
            assert!(CASES.iter().any(|c| c.name.starts_with(prefix)), "`{prefix}` skips nothing -- no case name starts with it");
            assert!(
                !PANE_DIVERGENCES.iter().any(|(d, _)| d.starts_with(prefix)),
                "`{prefix}` is skipped, so nothing starting with it can also be listed as a divergence"
            );
        }
    }

    #[test]
    fn bish_agrees_with_bash() {
        let Some(bish) = bish_binary() else { return };
        if oracle_bash().is_none() {
            return;
        }
        let differing = compare(CASES, &bish);
        assert!(
            differing.is_empty(),
            "{} of {} cases differ from bash:\n{}",
            differing.len(),
            CASES.len(),
            differing.iter().map(|(name, want, got)| format!("  {name}\n    bash: {want:?}\n    bish: {got:?}")).collect::<Vec<_>>().join("\n")
        );
    }

    /// Cases that only mean something in a login shell, run under `-l` by
    /// both shells. The exit status isn't in the compared text, so each
    /// case shows it from a `$( )` (which bash counts as the login shell)
    /// or an EXIT trap.
    const LOGIN_CASES: &[Case] = &[
        case("logout-leaves-from-a-command-substitution", r#"x=$(echo a; logout 3; echo b); echo "[$x] rc=$?""#),
        case("logout-runs-the-exit-trap", r#"trap 'echo "trapped rc=$?"' EXIT; logout 4; echo not-reached"#),
        case("logout-in-a-subshell-refuses", r#"( logout 4; echo "in rc=$?" ) 2>/dev/null; echo "out rc=$?""#),
        case("logout-in-a-pipeline-stage-refuses", r#"logout 4 2>/dev/null | cat; echo "rc=${PIPESTATUS[0]}""#),
        case("logout-with-a-word-carries-on", r#"logout x 2>/dev/null; echo "rc=$?""#),
        case("logout-with-two-numbers-leaves-with-one", r#"x=$(logout 5 2 2>/dev/null); echo "rc=$?""#),
        case("logout-status-is-one-byte", r#"x=$(logout -1); echo "rc=$?"; x=$(logout -- 300); echo "rc=$?""#),
        case("login-shell-cannot-be-unset", r#"shopt login_shell; shopt -u login_shell; echo "rc=$?"; shopt login_shell; ( shopt login_shell )"#),
    ];

    #[test]
    fn bish_agrees_with_bash_as_a_login_shell() {
        let Some(bish) = bish_binary() else { return };
        if oracle_bash().is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("bish-bashdiff-login-{}", std::process::id()));
        let mut differing = Vec::new();
        for case in LOGIN_CASES {
            let dir = root.join(case.name);
            let answer = |shell: &std::ffi::OsStr| {
                std::fs::create_dir_all(&dir).unwrap();
                let outcome = run_with(shell, &["-l"], case.script, &dir);
                std::fs::remove_dir_all(&dir).ok();
                outcome
            };
            let want = answer(oracle());
            let got = answer(bish.as_os_str());
            if want.timed_out || got.timed_out || want.text != got.text {
                differing.push((case.name, want.text, got.text));
            }
        }
        std::fs::remove_dir_all(&root).ok();
        assert!(
            differing.is_empty(),
            "{} of {} login cases differ from bash:\n{}",
            differing.len(),
            LOGIN_CASES.len(),
            differing.iter().map(|(name, want, got)| format!("  {name}\n    bash: {want:?}\n    bish: {got:?}")).collect::<Vec<_>>().join("\n")
        );
    }

    /// Scripts that don't survive the round trip, and why. Kept empty: the
    /// round trip is how a construct reaches a self-exec'd child, so losing
    /// something there means the construct quietly does something else.
    const ROUND_TRIP_BREAKS: &[&str] = &[];

    /// As with `DIVERGENCES`: fixing one fails the suite until its line goes.
    #[test]
    fn the_known_round_trip_breaks_are_still_broken() {
        let Some(bish) = bish_binary() else { return };
        let root = std::env::temp_dir().join(format!("bish-roundtrip-guard-{}", std::process::id()));
        let mut fixed = Vec::new();
        for case in CASES.iter().chain(PENDING.iter()).filter(|c| ROUND_TRIP_BREAKS.contains(&c.name)) {
            let Ok(tokens) = crate::lexer::Lexer::new(case.script).tokenize() else { continue };
            let Ok(program) = crate::parser::Parser::new(tokens).parse_program() else { continue };
            let round_tripped = crate::serialize::serialize_program(&program);
            let dir = root.join(case.name);
            let run_one = |script: &str| {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let out = run(bish.as_os_str(), script, &dir);
                let _ = std::fs::remove_dir_all(&dir);
                out
            };
            let direct = run_one(case.script);
            let after = run_one(&round_tripped);
            if direct.text == after.text && direct.timed_out == after.timed_out {
                fixed.push(case.name);
            }
        }
        let _ = std::fs::remove_dir(&root);
        assert!(
            fixed.is_empty(),
            "{} script(s) survive the round trip now -- remove them from ROUND_TRIP_BREAKS:\n{}",
            fixed.len(),
            fixed.iter().map(|n| format!("  {n}")).collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn every_round_trip_break_names_a_real_case() {
        for name in ROUND_TRIP_BREAKS {
            assert!(CASES.iter().chain(PENDING.iter()).any(|c| c.name == *name), "`{name}` is listed as a round-trip break with no case to prove it");
        }
    }

    /// Every corpus script, run as written and again after a round trip
    /// through the serializer, must do the same thing. Behaviour rather
    /// than AST equality, because the question is whether the text means
    /// the same.
    #[test]
    fn every_corpus_script_survives_a_round_trip_through_the_serializer() {
        let Some(bish) = bish_binary() else { return };
        let root = std::env::temp_dir().join(format!("bish-roundtrip-{}", std::process::id()));
        let mut broken = Vec::new();
        for case in CASES.iter().chain(PENDING.iter()) {
            if ROUND_TRIP_BREAKS.contains(&case.name) {
                continue;
            }
            let Ok(tokens) = crate::lexer::Lexer::new(case.script).tokenize() else { continue };
            let Ok(program) = crate::parser::Parser::new(tokens).parse_program() else { continue };
            let round_tripped = crate::serialize::serialize_program(&program);
            let dir = root.join(case.name);
            let run_one = |script: &str| {
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir_all(&dir).unwrap();
                let out = run(bish.as_os_str(), script, &dir);
                let _ = std::fs::remove_dir_all(&dir);
                out
            };
            let direct = run_one(case.script);
            let after = run_one(&round_tripped);
            if direct.text != after.text || direct.timed_out != after.timed_out {
                broken.push((case.name, round_tripped, direct.text, after.text));
            }
        }
        let _ = std::fs::remove_dir(&root);
        assert!(
            broken.is_empty(),
            "{} script(s) behave differently after a round trip through the serializer:\n{}",
            broken.len(),
            broken
                .iter()
                .map(|(name, text, before, after)| format!("  {name}\n    serialized: {text:?}\n    direct: {before:?}\n    after:  {after:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn the_known_divergences_are_still_divergences() {
        let Some(bish) = bish_binary() else { return };
        if oracle_bash().is_none() {
            return;
        }
        let differing: Vec<&str> = compare(PENDING, &bish).into_iter().map(|(name, _, _)| name).collect();
        for (name, why) in DIVERGENCES {
            assert!(differing.contains(name), "`{name}` matches bash now -- remove its line from DIVERGENCES ({why})");
        }
        let unlisted: Vec<&str> = differing.iter().filter(|n| !DIVERGENCES.iter().any(|(d, _)| d == *n)).copied().collect();
        assert!(unlisted.is_empty(), "differing but not listed: {unlisted:?}");
    }

    /// How far a producer gets before its reader is given a turn. Not
    /// differential: bash forks its stages, so its count is up to the
    /// kernel's scheduler and varies with load. bish's stages share a
    /// thread, and a producer that never blocks would run a pipe buffer
    /// ahead. The bound is generous; it's there to catch a runaway, not to
    /// pin a number.
    #[test]
    fn a_producer_does_not_run_away_from_its_reader() {
        let Some(bish) = bish_binary() else { return };
        let dir = std::env::temp_dir().join(format!("bish-runaway-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"while true; do echo x; echo p >> prod.log; done | { read a; echo "got=$a"; }; wc -l < prod.log"#;
        let out = run(bish.as_os_str(), script, &dir);
        let _ = std::fs::remove_dir_all(&dir);
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(lines.first().copied(), Some("got=x"), "the reader never ran: {:?}", out.text);
        let produced: usize = lines.last().and_then(|n| n.trim().parse().ok()).unwrap_or_else(|| panic!("no count in {:?}", out.text));
        assert!(produced < 1000, "the producer ran {produced} times before its reader had a turn");
    }

    // Not differential; bash has nothing like it. A recursion that has run
    // no command and changed no argument since it last entered the same
    // function is at a fixed point, so it's reported at the second call
    // rather than at the stack limit. A recursion that does anything at all
    // must be left alone.
    #[test]
    fn a_recursion_that_cannot_terminate_is_reported_as_such() {
        let Some(bish) = bish_binary() else { return };
        let root = std::env::temp_dir().join(format!("bish-nonproductive-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let proven: [(&str, &str); 4] = [
            ("f() { f; }; f", "called itself"),
            ("a() { b; }; b() { a; }; a", "cycle of 2 calls"),
            ("a() { b; }; b() { c; }; c() { a; }; a", "cycle of 3 calls"),
            // An empty branch runs no command, so this is a fixed point.
            ("f() { case a in a) ;; esac; f; }; f", "called itself"),
        ];
        for (script, expected) in proven {
            let out = run(bish.as_os_str(), script, &root);
            assert!(out.text.contains("cannot terminate"), "{script:?} was not recognised: {:?}", out.text);
            assert!(out.text.contains(expected), "{script:?} misdescribed the cycle: {:?}", out.text);
        }

        // These recurse forever but productively: a command, an assignment,
        // or a value from outside the shell. They hit the stack limit
        // instead.
        for script in [
            "f() { x=1; f; }; f",
            "f() { true; f; }; f",
            "f() { f $((${1:-0}+1)); }; f",
            "f() { f $RANDOM; }; f",
            "f() { f $SECONDS; }; f",
            // No external command per frame: that would be a thousand
            // spawns, and `true` reaches the same bookkeeping. Arithmetic
            // that assigns counts as an effect, in each of its spellings.
            "f() { ((i++)); f; }; f",
            "f() { : $(( j = j + 1 )); f; }; f",
            "f() { x[0]=$((n++)); f; }; f",
            "f() { shift; f; }; f",
            "f() { unset zz; f; }; f",
        ] {
            let out = run(bish.as_os_str(), script, &root);
            assert!(
                !out.text.contains("cannot terminate"),
                "{script:?} does something every time round and was still called a fixed point: {:?}",
                out.text
            );
            assert!(out.text.contains("nesting level exceeded"), "{script:?} should have run into the stack limit: {:?}", out.text);
        }

        let out = run(bish.as_os_str(), r#"f() { if [ "$1" -gt 0 ]; then f $(($1-1)); else echo done; fi; }; f 50"#, &root);
        assert_eq!(out.text, "done", "a bounded recursion must simply run");

        // Reported once, and the script stops there, as with `FUNCNEST`.
        let out = run(bish.as_os_str(), "f() { f; }; f; echo unreached", &root);
        assert_eq!(out.text.matches("cannot terminate").count(), 1, "reported once per runaway, not once per frame: {:?}", out.text);
        assert!(!out.text.contains("unreached"), "the script should have stopped: {:?}", out.text);

        std::fs::remove_dir_all(&root).ok();
    }

    // Not differential: bash dies on a signal for three of these four. bish
    // must not, since an unbounded recursion is usually a typo. Each reaches
    // the stack by a different route (function call, `eval`, a self-sourcing
    // file, the arithmetic parser), and one stack measurement covers all four
    // (see stackguard).
    #[test]
    fn a_runaway_recursion_is_an_error_rather_than_a_crash() {
        let Some(bish) = bish_binary() else { return };
        let root = std::env::temp_dir().join(format!("bish-noabort-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("self.sh"), format!("source {}\n", root.join("self.sh").display())).unwrap();
        let deep = format!("echo $(( {}1{} ))", "(".repeat(4000), ")".repeat(4000));
        let cases: [(&str, String); 5] = [
            ("a function calling itself", "f() { f; }; f".to_string()),
            ("a function whose body is two self-calls", "f() { f; f; }; f".to_string()),
            ("eval calling itself", r#"f() { eval "f"; }; f"#.to_string()),
            ("a file sourcing itself", format!("source {}", root.join("self.sh").display())),
            // A `FUNCNEST` too high to reach doesn't disable the stack
            // backstop.
            ("a FUNCNEST too large to ever be reached", "FUNCNEST=1000000; f() { x=1; f; }; f".to_string()),
        ];
        for (what, script) in cases.iter().map(|(w, s)| (*w, s.clone())).chain([("arithmetic nested very deeply", deep)]) {
            let out = run(bish.as_os_str(), &script, &root);
            assert!(
                !out.text.contains("stack overflow") && !out.text.contains("core dumped") && !out.text.contains("Aborted"),
                "{what}: bish aborted rather than reporting a limit -- {:?}",
                out.text
            );
            assert!(!out.text.is_empty(), "{what}: bish said nothing at all, so it did not report the limit either");
        }
        std::fs::remove_dir_all(&root).ok();
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
