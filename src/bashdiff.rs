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
        // A list joins on IFS's first character, not on a space, and
        // an unquoted one is that same join split again.
        case("ifs-join-star", r#"set -- a b c; IFS=-; printf '[%s]' "$*"; echo"#),
        case("ifs-join-array", r#"a=(x y z); IFS=-; printf '[%s]' "${a[*]}"; echo"#),
        case("ifs-join-unquoted", r#"set -- a b c; IFS=-; printf '[%s]' $*; printf '[%s]' $@; echo"#),
        case("ifs-join-empty", r#"set -- a b c; IFS=; printf '[%s]' "$*"; echo"#),
        case("ifs-join-multichar", r#"set -- a b c; IFS=xy; printf '[%s]' "$*"; echo"#),
        case("ifs-join-keys", r#"aa_1=; aa_2=; set -- ${!aa_*}; printf '%s' "$#""#),
        case("param-nested", r#"x=abc; echo "${x/$(printf b)/Z}""#),
        // -- arrays ---------------------------------------------------
        case("array-basic", r#"a=(1 2 3); echo "${a[0]}" "${a[@]}" "${#a[@]}" "${!a[@]}""#),
        case("array-append", r#"a=(1); a+=(2 3); echo "${a[@]}""#),
        case("array-negative", r#"a=(1 2 3); echo "${a[-1]}""#),
        // `:off:len` counts elements for a list and characters for a
        // string -- the one operator whose meaning changes with what it
        // is applied to.
        case("array-slice", r#"a=(z1 z2 z3 z4 z5); echo "${a[@]:1:2}" "${a[@]:2}" "${a[@]:0:2}" "${a[@]: -2}" "${a[@]:9}""#),
        case("array-slice-unquoted", r#"a=(z1 z2 z3); printf '[%s]' ${a[@]:1:2}; echo"#),
        case("array-slice-sparse", r#"b=(x y); b[5]=z; printf '[%s]' "${b[@]:1:2}"; echo"#),
        case("array-slice-star", r#"a=(z1 z2 z3); IFS=-; echo "${a[*]:1:2}""#),
        case("args-slice", r#"set -- p1 p2 p3 p4; printf '[%s]' "${@:2:2}" "${@:2}" "${@: -1}"; echo"#),
        case("args-slice-zero", r#"set -- p1 p2; printf '[%s]' "${@:1:1}"; echo"#),
        case("string-slice-negative-length", r#"x=abcdef; echo "${x:1:-1}""#),
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
        // `time` is a reserved word, so it takes a whole pipeline and
        // a group, and `echo time` is still a word. The numbers vary,
        // so these look at the shape.
        // The numbers vary, so the cases that can be compared are the
        // ones a format makes stable. `format_times`' own unit test
        // covers the arithmetic.
        case("time-format-literal", r#"TIMEFORMAT='timed'; time true"#),
        case("time-format-percent", r#"TIMEFORMAT='%%'; time true"#),
        case("time-status", r#"TIMEFORMAT='t'; time false; echo "status=$?""#),
        case("time-pipeline", r#"TIMEFORMAT='t'; time printf 'b\na\n' | sort | head -1"#),
        case("time-group", r#"TIMEFORMAT='t'; time { echo g; }"#),
        case("time-negated", r#"TIMEFORMAT='t'; ! time false; echo "status=$?""#),
        case("time-not-a-word", r#"echo time; x=time; echo "$x"; echo "$(echo time)""#),
        // -- globbing -------------------------------------------------
        case("glob-star", r#": > ga; : > gb; printf '%s,' g*; echo"#),
        case("glob-class", r#": > g1; : > g2; printf '%s,' g[12]; echo"#),
        case("glob-question", r#": > gx; printf '%s,' g?; echo"#),
        case("glob-nomatch", r#"printf '%s,' zz_no_match*; echo"#),
        case("glob-dot-literal", r#": > .hidden; printf '%s,' .h*; echo"#),
        // The shopts that change what a pattern matches.
        case("shopt-nullglob", r#": > kept; shopt -s nullglob; printf '[%s]' zz_no_match*; printf '[%s]' kept; echo"#),
        case("shopt-dotglob", r#": > .hidden; : > shown; shopt -s dotglob; printf '[%s]' *; echo"#),
        case("shopt-nocaseglob", r#": > ABC; shopt -s nocaseglob; printf '[%s]' abc*; echo"#),
        // A quoted word is not a pattern however many metacharacters
        // it has in it, so none of the above may touch it.
        case("shopt-nullglob-quoted", r#"shopt -s nullglob; printf '[%s]' 'a*b'; echo"#),
        // Every component may be a pattern, not only the last.
        case("glob-multi-component", r#"mkdir -p m/one m/two; : > m/one/f.c; : > m/two/f.c; printf '[%s]' m/*/f.c */*.c; echo"#),
        case("glob-question-component", r#"mkdir -p m/one; : > m/one/f; printf '[%s]' m/?ne/f; echo"#),
        case("glob-missing-dir", r#"printf '[%s]' nodir_zz/*; echo"#),
        // `**` is any number of directories, none included, and it
        // does not descend through a symlink where a single `*` still
        // steps through one.
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
        // -- builtins -------------------------------------------------
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
        case("eval", r#"x=1; eval 'x=$((x+1))'; echo $x"#),
        case("source-file", "printf 'sourced=1\\n' > lib.sh; . ./lib.sh; echo $sourced"),
        // -- errors and status ----------------------------------------
        case("exit-status", r#"true; echo $?; false; echo $?"#),
        case("set-e", r#"set -e; (false; echo unreached); echo $?"#),
        case("set-u", r#"set -u; echo "${undefined_zz-ok}""#),
        case("and-or", r#"true && echo t; false || echo f; false && echo no; echo $?"#),
        // -- the roadmap-9 builtin gaps -------------------------------
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
        case("ulimit-p", r#"ulimit -p"#),
        // -- roadmap 10: what bish used to accept and bash never did --
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
        // stderr goes to /dev/null throughout: the *wording* of an
        // arithmetic error differs between the two shells (bish's
        // arith.rs names its own tokens), and what this case is about
        // is that a `$(( ))` failure stops the script while the `(( ))`
        // command below does not.
        case("arith-expansion-is-fatal", r#"{ echo $((1+)); } 2>/dev/null; echo unreached"#),
        case("arith-command-is-not", r#"((1+)) 2>/dev/null; echo after"#),
        case("error-if-unset", r#": ${x:?}; echo unreached"#),
        // Sourced from a file, with stderr discarded: a syntax error
        // is reported differently by the two shells (bash echoes the
        // offending line back, bish does not), and what these are about
        // is that the construct is rejected at all -- which `.` turns
        // into an ordinary non-zero status the script can see.
        case("array-assignment-needs-attached-parens", "printf 'arr= (a b)\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("function-body-must-be-compound", "printf 'f() echo hi\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("empty-if-body", "printf 'if true; then fi\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("empty-loop-condition", "printf 'while; do :; done\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("declare-subscript", r#"declare 'a[0]=5'; echo "${a[0]}""#),
        // -- roadmap 11: variables are not the environment ------------
        case("plain-assignment-is-not-exported", r#"x=1; env | grep -c '^x=1$'"#),
        case("export-is", r#"export y=2; env | grep -c '^y=2$'"#),
        case("export-n-keeps-the-value", r#"export z=1; export -n z; env | grep -c '^z='; echo "[$z]""#),
        case("a-child-sees-only-exports", r#"a=1; export b=2; env | grep -cE '^(a|b)='"#),
        case("read-nul-delimited", "printf 'a\\0b\\0' | while read -r -d '' v; do printf '[%s]' \"$v\"; done; echo"),
        // -- roadmap 12: an array is many words -----------------------
        case("array-copy", r#"a=(1 2 3); b=("${a[@]}"); c=(x "${a[@]}" y); echo "${#b[@]} ${#c[@]}""#),
        case("array-literal-splits", r#"s="p q"; d=($s); e=("$s"); echo "${#d[@]} ${#e[@]}""#),
        case("array-transform-per-element", r#"a=("x y" z); printf '[%s]' "${a[@]@Q}"; echo"#),
        case("arith-subscript", r#"a=(1 2 3); i=2; declare -A m=([k]=7); echo $((a[1])) $((a[i])) $((m[k]))"#),
        case("arith-subscript-assign", r#"a=(0 0); ((a[1]=5)); ((a[1]++)); echo "${a[*]}""#),
        case("assoc-key-with-spaces", r#"declare -A m; m[x y]=1; k="a b"; m[$k]=2; echo "${m[x y]}${m[a b]}""#),
        case("nested-subscript-assign", r#"a=(1 2); b=(0); a[b[0]]=9; echo "${a[0]}""#),
        case("index-append", r#"declare -A m; m[k]+=v; m[k]+=w; a=(x); a[0]+=y; echo "${m[k]} ${a[0]}""#),
        case("nameref-to-an-array", r#"a=(1 2); declare -n r=a; echo "${r[1]} ${#r[@]} ${r[*]}"; r[0]=9; echo "${a[0]}""#),
        // -- roadmap 13: a pipeline stage is this shell's child --------
        case("stage-sees-globals", r#"x=1; f() { echo "[$x]"; }; { echo "[$x]"; } | cat; f | cat; (echo "[$x]") | cat"#),
        case("stage-sees-shell-options", r#"set -u; { echo "${nosuch-ok}"; } | cat; shopt -s nullglob; { shopt nullglob; } | cat"#),
        case("read-leaves-the-rest-of-the-pipe", r#"{ echo a; echo b; } | { read -r x; echo "x=$x"; cat; }"#),
        case("lastpipe", r#"shopt -s lastpipe; set +m; n=0; seq 1 3 | while read -r l; do n=$((n+1)); done; echo "$n""#),
        case("lastpipe-status", r#"shopt -s lastpipe; set +m; false | true; echo "${PIPESTATUS[*]}"; true | false; echo "rc=$?""#),
        case("shopt-status", r#"shopt nullglob; echo "rc=$?"; shopt -s nullglob; shopt nullglob; echo "rc=$?""#),
        // -- roadmap 14: the descriptors the shell owns ----------------
        case(
            "builtin-reads-a-shell-fd",
            r#"exec 3</etc/hostname; read -r a <&3; exec 3<&-; exec 4</etc/hostname; read -r -u 4 b; exec 4<&-; [ "$a" = "$b" ] && echo same"#,
        ),
        case("read-u-bad-fd", r#"read -r -u 9 l 2>/dev/null; echo "rc=$?""#),
        case("varfd-open-and-close", r#"exec {fd}</etc/hostname; echo "$((fd>=10))"; read -r -u "$fd" l; exec {fd}<&-; [ -n "$l" ] && echo read"#),
        case("varfd-dup", r#"echo x | { exec {fd}<&0; read -r -u "$fd" l; echo "[$l]"; }"#),
        case("mapfile-counted-options", "printf '1\\n2\\n3\\n4\\n' | { mapfile -t -s 1 -n 2 a; echo \"${#a[@]} ${a[*]}\"; }"),
        case("mapfile-origin", "printf '1\\n2\\n' | { mapfile -t -O 5 a; echo \"${!a[*]}\"; }"),
        // -- roadmap 15: errexit's actual rules ------------------------
        case("errexit-exempts-a-chain-member", r#"set -e; f() { false; }; f && echo ok; false && echo no; echo after"#),
        case("errexit-still-fires-on-the-last", r#"set -e; true && false; echo unreached"#),
        case("errexit-and-pipefail", r#"set -euo pipefail; false | true; echo unreached"#),
        case("assignment-takes-its-substitution-status", r#"x=$(exit 7); echo "rc=$?"; y=$(true); echo "rc=$?""#),
        case("errexit-on-a-failed-capture", r#"set -e; x=$(false); echo unreached"#),
        case("inherit-errexit", r#"set -e; x=$(false; echo reached); echo "[$x]""#),
        case("set-cluster-ending-in-o", r#"set -euo pipefail; set -o | grep -cE '^(errexit|nounset|pipefail) +on'"#),
        // -- roadmap 16: expansions that nest, diagnostics that read --
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
        // -- roadmap 17: local, FUNCNAME and the call stack ------------
        case("local-shadows-as-unset", r#"x=out; f() { local x; echo "[${x-unset}]"; }; f; echo "[$x]""#),
        case("declare-with-no-value-is-unset", r#"declare z; echo "[${z-unset}]"; g() { declare y; echo "[${y-unset}]"; }; y=out; g"#),
        case("nounset-status", r#"set -u; echo "$nosuch_zz""#),
        case("bare-array-is-element-zero", r#"a=(1 2); echo "[$a]"; declare -A m=([k]=v); echo "[$m]""#),
        case("funcname-scalar", r#"f() { echo "[$FUNCNAME]"; }; f"#),
        case("funcname-has-no-main-frame-under-c", r#"f() { g; }; g() { echo "${FUNCNAME[*]} n=${#FUNCNAME[@]}"; }; f"#),
        // -- roadmap 18: the last of the parser's leniency -------------
        // Sourced with stderr discarded, so what is compared is that
        // the construct is rejected -- the two shells word a syntax
        // error differently (bash echoes the offending line back).
        case("two-defs-no-separator", "printf 'f() { :; } f() { :; }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case(
            "brace-group-needs-a-terminator",
            "printf '{ : }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\"; printf '{ echo }\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\"",
        ),
        case("unterminated-brace-expansion", "printf 'echo ${x\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case("c-for-needs-both-parens", "printf 'for ((i=0; i<2; i++) do :; done\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        // bash rejects this at parse time and abandons the whole
        // input; bish rejects it when the condition runs. Sourced, so
        // both are an ordinary non-zero status the script can see.
        case("dbracket-rejects-a-fourth-operand", "printf '[[ a == b == c ]]\\n' > s.sh; . ./s.sh 2>/dev/null; echo \"rc=$?\""),
        case(
            "nocasematch-covers-case",
            r#"shopt -s nocasematch; case AB in ab) echo ci;; esac; [[ ABC == abc ]] && echo dbracket; [ ABC = abc ] || echo literal"#,
        ),
        // -- roadmap 19: the builtins and flags nobody had needed -------
        case("printf-star-width", r#"printf '%*d|%-*d|%.*f|%*.*f|\n' 5 42 5 42 2 3.14159 8 2 3.14159"#),
        case("tilde-user-and-dirs", r#"cd /tmp; cd /; echo "$(echo ~+) $(echo ~-)"; echo ~nosuchuser_zz"#),
        // Not through a pipe: `jobs` as a pipeline stage is its own
        // divergence (see jobs-in-a-pipeline).
        case("jobs-p-and-l", r#"sleep 0.2 & jobs -p > p; jobs > j; grep -cE '^[0-9]+$' p; grep -c Running j"#),
        case("type-a-lists-every-match", r#"type -a echo"#),
        case("alias-p", r#"alias x=y; alias -p"#),
        case("dollar-quoted-string", r#"x=1; echo $"lit $x""#),
        case("source-takes-arguments", "printf 'echo \"[$1][$#]\"\\n' > s.sh; set -- outer; . ./s.sh a b; echo \"after=[$1]\""),
        case("xtrace-traces-assignments", r#"PS4='XX '; set -x; x=1; y="a b"; a=(1 2)"#),
        case("true-and-false-are-builtins", r#"type -t true; type -t false; true; echo "rc=$?"; false; echo "rc=$?""#),
        // -- roadmap 21: the redirects that were syntax errors ---------
        case("stderr-to-a-saved-fd", r#"exec 3>&1; echo a 2>&3; exec 3>&-; echo b 3>&1 >&2 2>&3 3>&-"#),
        case("close-an-unopened-fd", r#"{ echo a; } 3>&-; echo b 4>&-"#),
        case("dup-to-a-word-fd", r#"exec 3>&1; f=3; echo hi >&$f; echo two >&$((1+2)); exec 3>&-"#),
        case("dup-to-a-closed-fd", r#"echo x >&9; echo "rc=$?""#),
        case("coproc-round-trip", r#"coproc CP { cat; }; echo hi >&"${CP[1]}"; sleep 0.2; read -r l <&"${CP[0]}"; echo "back=[$l]""#),
        // -- roadmap 20: a lone bracket is not a pattern ---------------
        case("lone-bracket-is-literal", r#"printf '%s,' [ ] '[abc'; echo; [ 1 -lt 2 ] && echo test-works"#),
        // -- roadmap 22: parameter expansion's last corners ------------
        case("count-of-the-positionals", r#"set -- a b c; echo "${#@} ${#*} $#""#),
        case("omitted-slice-offset", r#"a=(1 2 3); echo "${a[@]::2}"; x=abcdef; echo "${x::3}"; set -- a b c; echo "${@:1:1}""#),
        // -- roadmap 24: a redirect on a function call -----------------
        case("function-call-redirect", r#"f() { echo a; /bin/echo b; echo e >&2; }; f > o 2> e; cat o e; rm -f o e"#),
        case("function-call-redirect-fd", r#"f() { echo x; }; f 3>&1 >o; cat o; rm -f o"#),
        case(
            "dbracket-quoted-pattern",
            r#"[[ abc == a* ]] && echo glob; [[ abc == "a*" ]] || echo literal; p='a*'; [[ abc == $p ]] && echo var-globs; [[ abc == "$p" ]] || echo var-literal"#,
        ),
        // -- roadmap 23: the command line the shell is invoked with ----
        case("bash-versinfo", r#"echo "${#BASH_VERSINFO[@]} ${BASH_VERSINFO[3]} ${BASH_VERSINFO[4]}""#),
        // The harness clears the environment, so both shells start
        // from no SHLVL at all and must reach 1 -- the increment is
        // what is being checked, not the inherited value.
        case("shlvl-starts-at-one", r#"echo "$SHLVL""#),
    ];

    // Cases bish does not match today, each with why. Asserted to
    // *still* diverge -- fixing one fails this test until its line is
    // removed, which is the only way a list like this stays true.
    const DIVERGENCES: &[(&str, &str)] = &[
        // The last of the grammar's leniency: bash calls it a syntax
        // error, bish skips the empty statement and runs the rest. The
        // other six of this group are fixed; this one lives inside
        // skip_terminators, which every construct's list parsing goes
        // through, and is not worth the blast radius on its own.
        ("empty-command-between-separators", "`;` twice in a row is skipped rather than reported"),
        // The pipeline architecture's residue: a stage that is a
        // builtin, function or compound command re-execs (see roadmap
        // 13), and a re-exec carries no job table across. (The coproc
        // entry that used to sit here was not this at all -- the
        // descriptors were fine, `>&$word` was being dropped. See
        // roadmap 21.)
        ("jobs-in-a-pipeline", "`jobs` as a pipeline stage runs in a child with an empty job table"),
        // Pre-existing, and named now that the redirect simulation made
        // the rest of the list exact: `>&2` means "the enclosing
        // stderr", and OutputSink::Builtin's two booleans can only say
        // "this sink's other stream, else the enclosing one" -- a
        // different thing once the same command also redirects fd 2.
        // Expressing it wants the sink's stdout/stderr fields to hold a
        // small enum (a file, or the enclosing out/err) rather than a
        // file plus two flags.
        (
            "dup-to-stderr-that-is-itself-redirected",
            "`echo e >&2 2>/dev/null` writes to /dev/null; bash writes to the stderr `>&2` named, which is the one from before this command",
        ),
    ];

    // The cases the divergence list is about. Kept apart from `CASES`
    // so that list stays a description of what works.
    const PENDING: &[Case] = &[
        // -- roadmap 10: parser leniency, the part still standing -----
        case("empty-command-between-separators", "echo a; ; echo b"),
        // -- the pipeline architecture's residue ----------------------
        case("jobs-in-a-pipeline", r#"sleep 0.2 & jobs -p | grep -cE '^[0-9]+$'"#),
        case("dup-to-stderr-that-is-itself-redirected", r#"echo e >&2 2>/dev/null; echo done"#),
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

    // Both shells get the same scratch directory as HOME and cwd, so
    // neither reads a config file and neither sees the other's
    // leftovers -- and both get the *same, minimal* environment.
    //
    // env_clear matters more than it looks: these are subprocesses of
    // the test binary, and a shell run in-process by some other test in
    // this suite writes its exported variables straight into the real
    // process environment (see raw_var_write). Without this, a case's
    // result depends on which other tests happened to run first --
    // which showed up as every glob case failing, from a leaked
    // GLOBIGNORE, in whole-suite runs only.
    // Five seconds, then killed. A case that deadlocks -- and the
    // divergence list has one, a coproc whose descriptors go nowhere --
    // would otherwise wedge the whole test run rather than reporting a
    // difference.
    const CASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    fn run(shell: &std::ffi::OsStr, script: &str, dir: &std::path::Path) -> String {
        let out = Command::new(shell)
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

    // `Child::wait_with_output` with a deadline: polled rather than
    // blocked on, and killed if it runs out. Its output is still
    // collected afterwards, so a case that produced something before
    // hanging still reports it.
    fn wait_with_timeout(mut child: std::process::Child, limit: std::time::Duration) -> std::io::Result<std::process::Output> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        child.wait_with_output()
    }

    fn compare(cases: &[Case], bish: &std::path::Path) -> Vec<(&'static str, String, String)> {
        let root = std::env::temp_dir().join(format!("bish-bashdiff-{}", std::process::id()));
        let mut differing = Vec::new();
        for case in cases {
            // A directory per case, named after the case: several of
            // them create files, and a glob is only predictable in a
            // directory it owns. The *name* rather than an index
            // because the two tests that call this run concurrently in
            // one process -- indices would collide, and one test would
            // then delete the directory the other's shell is sitting in
            // (which shows up as bash's "getcwd: cannot access parent
            // directories", not as anything resembling its cause).
            let dir = root.join(case.name);
            std::fs::create_dir_all(&dir).unwrap();
            let want = run(std::ffi::OsStr::new("bash"), case.script, &dir);
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            let got = run(bish.as_os_str(), case.script, &dir);
            std::fs::remove_dir_all(&dir).ok();
            if want != got {
                differing.push((case.name, want, got));
            }
        }
        // Not remove_dir_all: the other test may still be using its own
        // subdirectories of this same root. Whichever finishes last
        // finds it empty and removes it.
        std::fs::remove_dir(&root).ok();
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
            differing.iter().map(|(name, want, got)| format!("  {name}\n    bash: {want:?}\n    bish: {got:?}")).collect::<Vec<_>>().join("\n")
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
            assert!(differing.contains(name), "`{name}` matches bash now -- remove its line from DIVERGENCES ({why})");
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
