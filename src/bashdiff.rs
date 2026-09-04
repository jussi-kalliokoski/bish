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
        // A pipeline stage that needs a shell runs in this process
        // rather than as a re-exec'd bish, when it is the only such
        // stage -- see run_multi's own `inproc_stage`. Everything below
        // is what must not change because of that: the stage is still a
        // subshell, it still reports its own status positionally, and
        // it still dies when the thing reading it goes away.
        case("pipeline-stage-is-a-subshell", r#"x=1; { x=2; echo "in=$x"; } | cat; echo "out=$x""#),
        // The whole of what the re-exec'd stage could not carry across.
        // Each of these was individually fixable by adding one more
        // thing to the preamble a re-exec'd stage is handed; none of
        // them needed fixing once the stage stopped being a re-exec.
        case("dirs-in-a-pipeline", r#"cd /; pushd /usr >/dev/null; dirs | cat"#),
        case("alias-in-a-pipeline", r#"alias q=x; alias | cat"#),
        case("trap-in-a-pipeline", r#"trap 'echo t' INT; trap -p | cat"#),
        case("complete-in-a-pipeline", r#"complete -W 'a b' foo; complete -p foo | cat"#),
        case("jobs-in-a-pipeline", r#"sleep 0.2 & jobs -p | grep -cE '^[0-9]+$'"#),
        case("pipeline-stage-cd-does-not-escape", r#"cd /; { cd /usr; echo "in=$PWD"; } | cat; echo "out=$PWD""#),
        // Paths a script names resolve against the *shell's* directory.
        // Indistinguishable from the process's until two shells share
        // one process, which is what an in-process pipeline stage is --
        // so these pin the behaviour that has to survive that.
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
        case("pipeline-stage-through-three", r#"echo a b c | tr ' ' '\n' | wc -l"#),
        // More than a pipe buffer's worth, so the stage has to be
        // running at the same time as the thing draining it.
        case("pipeline-stage-outlasts-the-pipe-buffer", r#"for i in $(seq 1 5000); do echo "line $i"; done | wc -l"#),
        // The reader leaves first. A stage that is its own process dies
        // of SIGPIPE here; one running in the shell has to stop anyway,
        // with the same status, or this never terminates at all.
        case("pipeline-stage-reader-leaves-early", r#"for i in $(seq 1 100000); do echo "l$i"; done | head -2; echo "ps=${PIPESTATUS[*]}""#),
        case("pipeline-stage-unbounded-reader-leaves", r#"while true; do echo x; done | head -2; echo "ps=${PIPESTATUS[*]}""#),
        // Two or more stages that need a shell cannot take turns by
        // running one of them in this shell: each would be waiting on a
        // pipe the other is on the far end of. Those run as coroutines
        // over one thread -- see run_multi_scheduled -- and everything
        // below is what that has to keep true.
        case("two-shell-stages", r#"echo x | while read l; do echo "[$l]"; done"#),
        case("two-shell-stages-both-compound", r#"{ echo one; echo two; } | { read a; read b; echo "$b $a"; }"#),
        case("three-shell-stages", r#"echo hi | { read v; echo "$v $v"; } | { read a b; echo "$b-$a"; }"#),
        case("shell-stage-between-two-externals", r#"/bin/echo a b | { read x y; echo "$y $x"; } | cat"#),
        // More than a pipe buffer, so neither stage can finish without
        // the other running -- the thing a single-threaded shell could
        // not do at all before.
        case(
            "two-shell-stages-outlast-the-pipe-buffer",
            r#"for i in $(seq 1 5000); do echo "l$i"; done | { c=0; while read l; do c=$((c+1)); done; echo "$c"; }"#,
        ),
        // A stage's pipes have to be its *real* fd 0 and fd 1, or none
        // of this works: an external inheriting the rest of the input,
        // and `exec {fd}<&0`, both go through the descriptors.
        case("shell-stage-hands-the-rest-to-an-external", r#"{ echo a; echo b; } | { read -r x; echo "x=$x"; cat; }"#),
        case("shell-stage-can-dup-its-own-stdin", r#"echo x | { exec {fd}<&0; read -r -u "$fd" l; echo "[$l]"; }"#),
        case("two-shell-stages-status", r#"{ exit 3; } | { read v; }; echo "ps=${PIPESTATUS[*]}""#),
        case("two-shell-stages-pipefail", r#"set -o pipefail; { exit 4; } | { read v; }; echo "rc=$?""#),
        case("two-shell-stages-are-subshells", r#"x=1; { x=2; echo "$x"; } | { read v; echo "in=$v"; }; echo "out=$x""#),
        case("two-shell-stages-cd-does-not-escape", r#"{ echo a; } | { read v; cd /tmp; }; echo "$PWD""#),
        case("two-shell-stages-reader-leaves-early", r#"while true; do echo x; done | { read v; echo "got=$v"; }"#),
        // Every shape where one side stops before the other is done.
        // These are the cases that hang rather than answer wrongly,
        // which is why the harness reports a timeout as its own kind of
        // failure -- every one of them printed the right thing first.
        case("unbounded-shell-producer-into-an-external-head", r#"while true; do echo x; done | head -1"#),
        case("unbounded-external-producer-into-a-shell-reader", r#"yes | { read v; echo "got=$v"; }"#),
        case("unbounded-producer-in-the-middle", r#"echo start | { while read l; do echo "$l"; done; } | head -1"#),
        case("reader-leaves-between-two-shell-stages", r#"while true; do echo x; done | { read a; echo "$a"; } | head -1"#),
        // How *far* the producer got, which is the part a timeout only
        // notices when the machine is loaded. Two stages share one
        // thread, and a stage gives it up when it blocks -- so a
        // producer whose reader is keeping up never blocked, and ran
        // 32768 times, until the pipe buffer filled, before the reader
        // took its first turn. Exactly 32768: a 64KB buffer and two
        // bytes a line. The count is what makes this a guard rather
        // than a race -- separate processes get fairness from the
        // kernel, and this asks for the same bound.
        case(
            "a-producer-does-not-run-away-from-its-reader",
            r#"while true; do echo x; echo p >> prod.log; done | { read a; echo "got=$a"; }; n=$(wc -l < prod.log); if [ "$n" -lt 200 ]; then echo bounded; else echo "runaway=$n"; fi"#,
        ),
        case("both-sides-bounded-but-uneven", r#"seq 1 10000 | { read v; echo "got=$v"; }"#),
        case("subshell-scope", r#"x=1; (x=2); echo $x"#),
        case("subshell-exit", r#"(exit 4); echo $?"#),
        // The real process environment is shared by every in-process
        // construct here, so what a foreground subshell exports must not
        // outlive it -- a real fork gets that isolation from the kernel,
        // and this shell has to put it back by hand. See the env journal
        // in exec.rs.
        case("subshell-export-does-not-escape", r#"(export E=inner); echo "[${E-unset}]""#),
        case("subshell-export-restores-the-old-value", r#"export E=outer; (export E=inner); echo "$E""#),
        case("subshell-unset-does-not-escape", r#"export E=outer; (unset E); echo "$E""#),
        case("substitution-export-does-not-escape", r#"x=$(export E=inner; echo hi); echo "$x [${E-unset}]""#),
        case("nested-subshell-exports-unwind-in-order", r#"export E=a; (export E=b; (export E=c); echo "$E"); echo "$E""#),
        case("subshell-export-is-visible-to-a-child-of-that-subshell", r#"(export E=inner; env | grep '^E='); echo "[${E-unset}]""#),
        case("exported-var-still-reaches-an-external-command", r#"export E=v; env | grep '^E='"#),
        // What a child's environment is built from -- see
        // `Shell::command`. Each of these is a way the shell's own view
        // and the process environment can disagree.
        case("exported-local-shadows-the-global-for-a-child", r#"export E=outer; f() { local -x E=inner; env | grep '^E='; }; f; env | grep '^E='"#),
        case("exported-but-unset-exports-nothing", r#"export E; env | grep -c '^E=' || true"#),
        case("exported-then-unset-is-gone-from-a-child", r#"export E=v; unset E; env | grep -c '^E=' || true"#),
        case("export-n-removes-it-from-a-child", r#"export E=v; export -n E; env | grep -c '^E=' || true"#),
        case("assignment-after-export-is-seen-by-a-child", r#"export E=first; E=second; env | grep '^E='"#),
        case("a-child-sees-a-var-exported-inside-a-function", r#"f() { export E=fromfn; }; f; env | grep '^E='"#),
        // `export` is global even inside a function; `declare` and
        // `local -x` are not. bish stored all three as locals, which
        // appeared to work only because the value was also written to
        // the real process environment and read back from there.
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
        // The content is right whichever way round, and only reached a
        // builtin's redirect once every simple command drained.
        case("proc-sub-out-to-a-builtins-redirect", r#"echo hi > >(cat)"#),
        case("proc-sub-out-to-an-externals-redirect", r#"/bin/echo hi > >(cat)"#),
        case("proc-sub-out-that-answers-at-eof", r#"printf 'a\nb\n' > >(wc -l)"#),
        case("proc-sub-out-as-an-argument", r#"tee >(wc -l) < /etc/hostname > /dev/null"#),
        // `>( )` runs concurrently with the command writing to it, so
        // a body that answers only at end-of-input still answers.
        //
        // Both of these are piped into `sort` on purpose. Where the
        // body's output lands relative to the *next* command is a race
        // neither shell settles: the substitution is not waited for, so
        // it comes down to whether the body gets scheduled before the
        // shell's next write. bash reliably wins that race because it
        // does almost nothing between closing the pipe and the next
        // command; bish unlinks, pumps and reaps in between, and loses
        // it about one run in five under load. Asserted directly, the
        // case passed four whole-suite runs and then failed one --
        // which is worse than not asserting it, because a corpus that
        // fails at random stops being read. `sort` makes the case about
        // what is actually promised: the body runs, and everything it
        // writes arrives.
        case("proc-sub-out-body-output-all-arrives", r#"{ echo hi > >(cat); echo done; } | sort"#),
        case("proc-sub-out-with-a-shell-body", r#"printf 'a\nb\n' > >(while read l; do echo "<$l>"; done)"#),
        case("proc-sub-out-two-at-once", r#"{ printf 'x\n' > >(cat) 2>/dev/null; printf 'y\n' > >(cat); } | sort"#),
        case("proc-sub-out-larger-than-a-pipe-buffer", r#"seq 1 20000 > >(wc -l)"#),
        // A redirect target is expanded once. It used to be expanded
        // twice for an external command -- once to build a sink that
        // an external does not use, once for the spawn -- so a target
        // with a side effect had it twice, and one that is not stable
        // could name two different files.
        case("redirect-target-is-expanded-once", r#"/bin/echo x > $(echo side >&2; echo out.txt); cat out.txt"#),
        case("redirect-target-expanded-once-for-a-builtin", r#"echo x > $(echo side >&2; echo out.txt); cat out.txt"#),
        // `<( )` streams: the producer is a coroutine given time while
        // the shell waits for whatever is consuming it, rather than
        // being run to completion into a temp file first. The two that
        // could not work the old way are the unbounded producers --
        // running those to completion never finishes.
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
        // `trap -p` prints `trap -- 'code' SIG`, so the builtin has to
        // be able to read that back -- without `--` ending the options
        // it set a trap whose action was `--`.
        case("trap-reads-its-own-output", r#"trap -- 'echo t' USR1; trap -p | head -1"#),
        case("pushd-n-adds-without-moving", r#"cd /; pushd -n /usr >/dev/null; pushd -n /tmp >/dev/null; dirs; echo "cwd=$PWD""#),
        // What a construct that still re-execs can see of the shell
        // that started it. A co-process starts once and lives, so
        // replaying these costs nothing per use -- unlike a pipeline
        // stage, which is why they are carried here and the pipeline
        // stopped re-execing instead.
        case("coproc-sees-the-directory-stack", r#"pushd /usr >/dev/null; coproc CP { dirs; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-sees-traps", r#"trap 'echo t' USR1; coproc CP { trap -p | head -1; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-sees-completions", r#"complete -W 'a b' foo; coproc CP { complete -p foo; }; read -r l <&"${CP[0]}"; echo "[$l]""#),
        case("coproc-round-trip", r#"coproc CP { cat; }; echo hi >&"${CP[1]}"; read -r l <&"${CP[0]}"; echo "[$l]""#),
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
        // A parameter expansion with an operator in it, inside `$(( ))`.
        // The plain `${x}` form was read by the arithmetic lexer itself
        // and everything else came out as 0 -- which is how
        // `f $((${1:-0}+1))` came to pass 1 for ever.
        case("arith-parameter-default", r#"unset u; echo "$(( ${u:-5} + 1 ))" "$(( ${u-5} + 1 ))""#),
        case("arith-parameter-length", r#"x=abcde; echo "$(( ${#x} ))""#),
        case("arith-array-length", r#"a=(1 2 3); echo "$(( ${#a[@]} + 1 ))""#),
        case("arith-parameter-strip", r#"p=12x; echo "$(( ${p%x} + 1 ))""#),
        case("arith-positional-with-default", r#"f() { echo "$(( ${1:-0} + 1 ))"; }; f; f 5"#),
        case("arith-unset-plain-is-zero", r#"unset u; echo "$(( ${u} ))" "$(( u ))""#),
        case("arith-empty-is-zero", r#"echo "[$(( ))]"; (( )); echo "rc=$?""#),
        case("error-if-unset", r#": ${x:?}; echo unreached"#),
        // A NUL cannot survive into a shell word -- the word ends up as
        // an argument to `execve`, which stops at the first one -- so
        // both shells drop it and say so once per substitution.
        case("command-substitution-drops-a-nul", r#"x=$(printf 'a\0b'); printf '%s\n' "$x""#),
        case("command-substitution-warns-once-per-nul-run", r#"x=$(printf 'a\0b\0c'); printf '%s\n' "$x""#),
        case("command-substitution-without-a-nul-is-quiet", r#"x=$(printf 'ab'); printf '%s\n' "$x""#),
        case("funcnest-limits-recursion", r#"FUNCNEST=5; f() { f; }; f; echo unreached"#),
        case("funcnest-ignores-a-non-number", r#"FUNCNEST=abc; f() { echo in; }; f"#),
        case("funcnest-zero-is-no-limit", r#"FUNCNEST=0; f() { echo in; }; f"#),
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
        // Declaring a name an array is not assigning one to it: bash
        // prints `declare -a A` for the first and `declare -a A=()` for
        // the second, and the attribute is what makes a later plain
        // `B=x` mean `B[0]=x`. All of it hangs on keeping the attribute
        // and the value apart, which is why the empty cases are here
        // next to the ones that carry a value.
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
        // -- roadmap 21: the redirects that were syntax errors ---------
        case("stderr-to-a-saved-fd", r#"exec 3>&1; echo a 2>&3; exec 3>&-; echo b 3>&1 >&2 2>&3 3>&-"#),
        case("close-an-unopened-fd", r#"{ echo a; } 3>&-; echo b 4>&-"#),
        case("dup-to-a-word-fd", r#"exec 3>&1; f=3; echo hi >&$f; echo two >&$((1+2)); exec 3>&-"#),
        case("dup-to-a-closed-fd", r#"echo x >&9; echo "rc=$?""#),
        // The whole redirect list, in source order, for a real child as
        // well as for a builtin. A descriptor names what it names *at
        // that point*, which is what makes the swap idiom work -- and
        // doing fds 0/1/2 first, through the Command builder, and the
        // numbered ones afterwards meant `3>&1` copied stdout's final
        // destination instead of the one it had when the dup was
        // written. Both spellings, since a builtin and an external
        // reach it by different routes.
        case("fd-swap-then-redirect-external", r#"/bin/echo x 3>&1 1>&2 2>&3 3>&- 2>/dev/null"#),
        case("fd-swap-then-redirect-compound", r#"{ echo o; echo e >&2; } 3>&1 1>&2 2>&3 3>&- 2>/dev/null"#),
        // Every file the list names is opened, even one a later
        // redirect supersedes: only `b` survives, and `a` is still
        // created and truncated.
        case("a-superseded-redirect-still-creates-its-file", r#"/bin/echo x > a > b; ls a b; echo x 2>c 2>d; ls c d"#),
        // A construct's `2>` reaches an *external* inside it, not just
        // the builtins: it used to reach the output sink and nothing
        // else, so the commonest way of silencing a block did nothing
        // for the commands it was written for.
        case("a-compounds-stderr-reaches-externals", r#"{ /bin/ls /nosuch; } 2>/dev/null; echo done; { /bin/ls /nosuch; } 2>e; wc -l < e"#),
        case("a-loops-stderr-reaches-externals", r#"for i in 1; do /bin/ls /nosuch; done 2>/dev/null; echo done"#),
        case("a-functions-stderr-reaches-externals", r#"f() { /bin/ls /nosuch; }; f 2>/dev/null; echo done"#),
        // A dup names the descriptor as it stands *at that point in the
        // list*, so these two are different destinations and not one
        // rule with an exception. Both spellings, because a builtin
        // resolves its redirects through the output sink and a compound
        // command through its child shell's -- two code paths that both
        // used to collapse the pair into "wherever the other stream
        // ends up", which silently sent the first case to /dev/null.
        case("dup-to-stderr-that-is-itself-redirected", r#"echo e >&2 2>/dev/null; echo done"#),
        case("dup-to-stderr-that-was-already-redirected", r#"echo e 2>/dev/null >&2; echo done"#),
        case("compound-dup-to-stderr-that-is-itself-redirected", r#"{ echo e; } >&2 2>/dev/null; echo done"#),
        case("compound-dup-to-stderr-that-was-already-redirected", r#"{ echo e; } 2>/dev/null >&2; echo done"#),
        // Both streams on one destination share a write position --
        // opened once, or duped, but never opened twice.
        case("both-streams-one-file-keeps-one-position", r#"{ echo out; echo err >&2; } >f 2>&1; cat f"#),
        case("dup-before-the-redirect-it-would-have-followed", r#"{ echo a; echo b >&2; } 2>&1 >f; cat f; echo "--""#),
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
        // -- what a sweep of untested ground turned up ------------------
        // `cd` reads the *shell's* HOME and OLDPWD. Reading the process
        // environment instead meant a plain assignment was invisible to
        // it (while `echo ~`, which does read the shell, honoured the
        // same assignment) and `unset` did not take.
        case("cd-uses-the-shells-home", r#"mkdir -p h; HOME=$PWD/h; cd; [ "$PWD" = "$(cd h 2>/dev/null; pwd)" ] || pwd"#),
        case("cd-dash-uses-the-shells-oldpwd", r#"mkdir -p a b; cd a; cd ../b; cd - > /dev/null; basename "$PWD""#),
        case("cd-with-no-home-fails", r#"unset HOME; cd 2>/dev/null; echo "rc=$?""#),
        // A shell remembers the route, not the destination: `cd link`
        // leaves `$PWD` ending in `link`, and `cd ..` from there goes
        // back to where `link` sits rather than to the parent of what
        // it points at. Storing what `getcwd` reported resolved every
        // symlink instead, so `pwd` answered what only `pwd -P` should
        // -- and `cd -P`, documented here as an accepted no-op, was in
        // fact the only behaviour there was.
        case("cd-keeps-the-route-it-was-given", r#"mkdir -p b/c; ln -s b/c link; cd link; echo "${PWD##*/}"; pwd -P | sed 's|.*/\(.\)$|\1|'"#),
        case("cd-dot-dot-is-lexical", r#"mkdir -p b/c; ln -s b/c link; cd link; cd ..; echo "${PWD##*/}""#),
        case("cd-physical-resolves-first", r#"mkdir -p b/c; ln -s b/c link; cd -P link; echo "${PWD##*/}"; cd ..; echo "${PWD##*/}""#),
        case("cd-normalises-dots-it-was-given", r#"mkdir -p b/c; cd ./b/../b/c; echo "${PWD##*/}""#),
        case("cd-dash-with-no-oldpwd-fails", r#"unset OLDPWD; cd - 2>/dev/null; echo "rc=$?""#),
        // Unsetting a variable that came from the environment has to
        // actually unset it. The lookups ended in a real-environment
        // fallback, so the name came straight back -- while a child's
        // environment, built from the shell's own tables, really had
        // lost it. The two answers disagreed about the same variable.
        case("unset-an-inherited-variable", r#"echo "[${HOME-gone}]"; unset HOME; echo "[${HOME-gone}]""#),
        case("unset-an-inherited-variable-under-nounset", r#"unset HOME; set -u; echo "$HOME" 2>/dev/null; echo "rc=$?""#),
        case("unset-then-set-again", r#"unset HOME; HOME=/x; echo "$HOME""#),
        // `type` knows five kinds, in this order, and knew two.
        case("type-of-a-keyword", r#"type if; type -t for; type -t "[["; type -t "!"; type -t time"#),
        // Reported only where it would actually be expanded, which is
        // why the case turns the shopt on: `alias -p` lists it either
        // way, but under `-c` bash's `type` says "not found".
        case("type-of-an-alias", r#"shopt -s expand_aliases; alias ll="ls -l"; type ll; type -t ll"#),
        case("type-of-an-unexpanded-alias", r#"alias ll="ls -l"; alias -p; type -t ll; echo "rc=$?""#),
        case("keyword-completions", r#"compgen -A keyword | sort | tr '\n' ' '; echo"#),
        // Every getopts branch that does not set OPTARG leaves it unset;
        // the argument of the *previous* option used to stay visible.
        case("getopts-clears-optarg", r#"f(){ while getopts "ab:c" o; do echo "$o=[${OPTARG-UNSET}]"; done; }; f -a -b val -c"#),
        case("getopts-silent-missing-argument", r#"f(){ while getopts ":ab:" o; do echo "$o=[${OPTARG-UNSET}]"; done; }; f -a -b"#),
        // `$-` is every option letter currently on, lowercase first,
        // then uppercase, then how the shell was invoked.
        case("dollar-dash", r#"echo "$-""#),
        case("dollar-dash-with-options-set", r#"set -e -f -u -x -C -m -T -E; echo "$-""#),
        // `select` reads through the redirect on the loop, like `read`
        // does -- it read the real stdin, so a here-string was never
        // seen and the body never ran. And an empty list is not a menu
        // with no entries: bash prints nothing and leaves at once.
        case("select-reads-its-own-redirect", r#"select x in a b; do echo "[$x][$REPLY]"; break; done <<< "2""#),
        case("select-of-nothing", r#"select x in; do echo hi; done; echo "rc=$?""#),
        case("select-at-end-of-input", r#"select x in a b; do break; done < /dev/null; echo done"#),
        case("select-without-an-in-clause", r#"set -- p q; select x; do echo "[$x]"; break; done <<< "2""#),
        // -- and what a second sweep turned up ------------------------
        // `read`'s short options cluster like every other builtin's.
        // Matching whole argument strings took `-ra` for a *variable
        // name*, so the commonest spelling of the commonest idiom read
        // into nothing at all.
        case("read-clustered-short-options", r#"read -ra p <<< "a b c"; echo "${#p[@]}${p[2]}""#),
        case("read-clustered-option-with-its-value", r#"read -rn2 v <<< "abcd"; echo "[$v]""#),
        // And `-r` was parsed and then not recorded, so every read
        // behaved as if it were given. Without it a backslash escapes
        // the next character -- including a separator, which then stays
        // part of its field, and the delimiter, which continues the
        // line.
        case("read-without-r-takes-escapes-off", r#"read v <<< 'a\tb'; echo "[$v]"; read -r v <<< 'a\tb'; echo "[$v]""#),
        case("read-without-r-keeps-an-escaped-separator", r#"read a b <<< 'x\ y z'; echo "[$a][$b]""#),
        case("read-without-r-continues-a-line", "printf 'a\\\\\nb\\n' | { read v; echo \"[$v]\"; }"),
        // `+=` on a name with the integer attribute is addition.
        case("integer-append-is-addition", r#"declare -i I=3; I+=2; echo $I; declare -i J; J+=5; echo $J; x=a; x+=b; echo $x"#),
        // Unsetting a local leaves the name unset for the rest of the
        // function; it does not hand the enclosing variable back.
        case("unset-a-local-does-not-uncover-the-global", r#"f() { local x=1; unset x; echo "[${x-gone}]"; }; x=outer; f; echo "[$x]""#),
        case("assign-after-unsetting-a-local", r#"f() { local x=1; unset x; x=2; echo "[$x]"; }; x=outer; f; echo "[$x]""#),
        // A pseudo-trap fires at the depth it was set and in what it
        // returns into, not in what it calls -- which is what "not
        // inherited" means, and what `functrace` turns off. RETURN was
        // gated on `functrace` alone, so the ordinary way of writing
        // one never ran at all.
        case("return-trap-set-inside-a-function", r#"f() { trap "echo RET" RETURN; :; }; f"#),
        case("return-trap-set-at-the-top-level", r#"trap "echo RET" RETURN; f() { :; }; f; echo end"#),
        case("return-trap-under-functrace", r#"set -T; trap "echo RET" RETURN; f() { :; }; f"#),
        case("return-trap-does-not-follow-a-call", r#"f() { trap "echo RET" RETURN; g; }; g() { echo ing; }; f"#),
        case("return-trap-fires-on-the-way-out", r#"g() { trap "echo RET" RETURN; :; }; f() { g; echo after; }; f"#),
        // `trap -p` reports the three pseudo-signals too, and reports
        // only what it was asked for.
        case("trap-p-of-a-pseudo-signal", r#"trap "echo x" RETURN; trap -p RETURN; trap "echo y" ERR; trap -p ERR"#),
        case("trap-p-names-only-what-it-was-asked", r#"trap "echo a" USR1; trap "echo b" USR2; trap -p USR2"#),
        case("trap-p-of-an-unknown-signal", r#"trap -p NOSUCH 2>/dev/null; echo "rc=$?""#),
        // -- and a third sweep -----------------------------------------
        // `base#digits` in bash goes up to base 64: 0-9, then a-z for
        // 10-35, then A-Z for 36-61, then `@` and `_`. Rust's
        // `from_str_radix` stops at 36 and *panics* past it, so
        // `$((64#zZ))` killed the shell. Below 37 there is no room for
        // two letter ranges and bash folds case instead, which is what
        // makes `16#FF` 255 while `64#zZ` is 35*64+61.
        case("arithmetic-bases-above-36", r#"echo $((64#zZ)) $((64#@)) $((64#_)) $((62#z))"#),
        // A numeric literal runs to the end of the digit-set run and is
        // checked against its base *afterwards*. Stopping at the first
        // character that did not fit read `12a` as 12 and `1e2` as 1,
        // each with something left over that quietly went nowhere.
        case(
            "a-literal-is-checked-after-it-is-read",
            r#"( echo $((1e2)) ) 2>/dev/null; echo "rc=$?"; ( echo $((12a)) ) 2>/dev/null; echo "rc=$?"; ( echo $((0b101)) ) 2>/dev/null; echo "rc=$?"; ( echo $((08)) ) 2>/dev/null; echo "rc=$?""#,
        ),
        case("a-bare-hex-prefix-is-zero", r#"echo $((0x)) $((0X)) $((00)) $((010)) $((0x1f))"#),
        // `${!name}` is refused three ways, each fatal the way `${x:?}`
        // is: `name` unset, its value empty, and its value not a
        // parameter name. Expanding to nothing meant a typo in the
        // indirection quietly became an empty string.
        case("indirect-expansion-of-an-unset-name", r#"echo "[${!nosuch}]"; echo after"#),
        case("indirect-expansion-of-an-empty-name", r#"x=""; echo "[${!x}]"; echo after"#),
        case("indirect-expansion-of-a-non-name", r#"x="1bad"; echo "[${!x}]"; echo after"#),
        case("indirect-expansion-follows-a-subscript", r#"a=(u v); x="a[1]"; echo "[${!x}]"; declare -A m=([k]=w); y="m[k]"; echo "[${!y}]""#),
        case("indirect-expansion-of-a-positional", r#"set -- p q; x=1; echo "[${!x}]"; x=@; echo "[${!x}]"; x=#; echo "[${!x}]""#),
        case("arithmetic-bases-fold-case-below-37", r#"echo $((16#FF)) $((16#ff)) $((36#ZZ)) $((36#zz))"#),
        // In a subshell so the message, which each shell words its own
        // way, is redirected away: it is emitted during *expansion*,
        // before the command's own `2>` is in place.
        case(
            "arithmetic-base-out-of-range",
            r#"( echo $((65#a)) ) 2>/dev/null; echo "rc=$?"; ( echo $((37#Z)) ) 2>/dev/null; echo "rc=$?"; ( echo $((1#a)) ) 2>/dev/null; echo "rc=$?""#,
        ),
        // `${a[@]OP}` applies OP to each element. Applied to the joined
        // text instead, the ops that act at most once per string acted
        // once for the whole array -- and the globally-acting ones came
        // out right by accident, which is why it went unnoticed.
        case("array-wide-replace-is-per-element", r#"a=(one two three); echo "${a[@]/o/0}"; echo "${a[@]//o/0}""#),
        case("array-wide-strip-is-per-element", r#"a=(one two); echo "${a[@]%e}"; echo "${a[@]#o}"; a=(a.txt b.txt); echo "${a[@]%.txt}""#),
        // POSIX character classes, in both matchers: a `[:name:]`
        // carries its own `]`, which does not close the bracket around
        // it. Scanning for the first `]` read `[[:space:]]` as the set
        // `[:space:` plus a literal `]`.
        case(
            "character-classes-in-a-regex",
            r#"[[ "a b" =~ [[:space:]] ]] && echo s; [[ ab =~ ^[[:alpha:]]+$ ]] && echo a; [[ 5 =~ [^[:digit:]] ]] || echo neg"#,
        ),
        case(
            "character-classes-in-a-glob",
            r#"case "a b" in *[[:space:]]*) echo s;; esac; case a] in [[:alpha:]]]) echo bracket;; esac; x="a b c"; echo "${x//[[:space:]]/_}""#,
        ),
        // `kill -0` is not a signal but the "is it still there" probe,
        // and `-s`/`-n` name the signal in the next argument.
        case("kill-signal-zero", r#"kill -0 $$ && echo alive; kill -s 0 $$ && echo alive2; kill -n 0 $$ && echo alive3"#),
        case("kill-of-a-process-that-is-gone", r#"kill -0 999999 2>/dev/null; echo "rc=$?""#),
        // `-o OPTNAME` is a unary test, not the OR connective -- which
        // is what it was read as, so `test -o errexit` was "empty OR
        // errexit" and always true. A connective needs something on its
        // left; at the start of a clause it is the test.
        case("test-o-asks-about-a-shell-option", r#"test -o errexit; echo "rc=$?"; set -e; test -o errexit; echo "rc=$?""#),
        case("test-o-still-combines", r#"set -e; set -u; [ -o errexit -a -o nounset ] && echo both; [ a -o b ] && echo or"#),
        case("test-v-in-the-bracket-form", r#"x=1; [ -v x ] && echo set; [ -v nosuch ]; echo "rc=$?""#),
        // The xtrace line is meant to read as the command that ran.
        case("xtrace-quotes-what-needs-it", r#"set -x; echo "a b"; echo "*"; echo ""; [ x = x ]"#),
        // `-P` forces the PATH search past a builtin of the same name;
        // `-p` prints a path only where `type` would say "file".
        case("type-capital-p-forces-the-path-search", r#"type -P echo; type -p echo; f(){ :; }; type -P f; echo "rc=$?""#),
        // `mapfile -d` was parsed and thrown away, so it read lines; and
        // its options did not cluster, so `-d,` was rejected as `-,`.
        case("mapfile-delimiter", r#"mapfile -t -d, arr <<< "a,b,c"; printf "[%s]" "${arr[@]}"; echo"#),
        case("mapfile-clustered-options", r#"mapfile -td, arr <<< "a,b,c"; echo "${#arr[@]}""#),
        // A quoted part of a `case` pattern is literal text. The whole
        // pattern was expanded as text and then matched as a glob, so
        // a variable holding `*` matched everything.
        case(
            "a-quoted-case-pattern-is-literal",
            r#"p="*"; case abc in "$p") echo lit;; *) echo no;; esac; case abc in '*') echo sq;; *) echo no2;; esac"#,
        ),
        case(
            "an-unquoted-case-pattern-is-still-a-glob",
            r#"p="*"; case abc in $p) echo glob;; *) echo no;; esac; x=/tmp; case /tmp/f in "$x"/*) echo prefix;; esac"#,
        ),
        // The variables the shell answers for without storing them.
        // `$BASHPID` is the same as `$$` here where bash's differs
        // inside a subshell -- a subshell is not a process here -- so
        // the case asks only what both can agree on.
        case("bash-subshell-counts-nesting", r#"echo "$BASH_SUBSHELL"; (echo "$BASH_SUBSHELL"; (echo "$BASH_SUBSHELL"))"#),
        case("bashpid-at-the-top-level", r#"[ "$BASHPID" = "$$" ] && echo same"#),
        case("shellopts-lists-what-is-on", r#"[[ $SHELLOPTS == *errexit* ]] || echo off; set -e; [[ $SHELLOPTS == *errexit* ]] && echo on"#),
        case("bashopts-lists-what-is-on", r#"[[ $BASHOPTS == *dotglob* ]] || echo off; shopt -s dotglob; [[ $BASHOPTS == *dotglob* ]] && echo on"#),
        // A computed variable is still a name: enumeration has to find
        // it, or `${!SHELL*}` reports only the stored half.
        case("prefix-listing-finds-computed-names", r#"echo "${!BASHO*}" "${!BASHP*}" "${!BASH_SUB*}" "${!EUI*}""#),
        // BASH_VERSINFO is readonly, which `declare -p` reports.
        case("bash-versinfo-is-readonly", r#"declare -p BASH_VERSINFO | cut -c1-14"#),
        // -- roadmap 05 ------------------------------------------------
        // An EXIT trap fires for the exit of the shell that armed it and
        // for no other. A subshell inherits it and can still see it --
        // `trap -p EXIT` inside one prints it -- but reaching the end of
        // a subshell is not that exit. Running it there is silently
        // destructive, because the shape this trap is nearly always
        // written in is a cleanup: the first command substitution ended
        // a subshell and removed the directory the script was still
        // using.
        case("an-exit-trap-does-not-fire-in-a-subshell", r#"trap "echo E" EXIT; ( echo s ); echo m"#),
        case("an-exit-trap-does-not-fire-in-a-substitution", r#"trap "echo E" EXIT; x=$(echo s); echo "$x""#),
        case("an-exit-trap-does-not-fire-per-pipeline-stage", r#"trap "echo E" EXIT; echo a | cat; echo m"#),
        case("an-exit-trap-set-in-a-subshell-fires-there", r#"( trap "echo E" EXIT; echo s ); echo m; echo a | { trap "echo P" EXIT; cat; }"#),
        case("an-inherited-exit-trap-is-still-visible", r#"trap "echo E" EXIT; ( trap -p EXIT ); echo m"#),
        case(
            "the-cleanup-idiom-survives-a-substitution",
            r#"d=$(mktemp -d); trap "rmdir $d" EXIT; x=$(echo hi); [ -d "$d" ] && echo "still there: $x""#,
        ),
        // A positional is set only when there is one at that position,
        // which is exactly what `set -u` is for. Every digit counted as
        // always-set, so the whole family was exempt from it.
        case("nounset-covers-the-positionals", r#"set -u; f() { echo "$1"; }; f"#),
        case("nounset-leaves-the-specials-alone", r#"set -u; echo "$@$*$#"; echo "${1-d}"; f() { echo "${1:-e}"; }; f; echo ok"#),
        case("a-positional-past-the-end-is-unset", r#"set -- a; echo "[${1+set}][${2+set}]"; set -u; echo "$2""#),
        // The right-hand side of an array assignment is expanded before
        // the old value is thrown away. Clearing first meant the array
        // could not be built from itself: `a=("${a[@]/x/y}")`, the
        // ordinary way to rewrite one in place, emptied it.
        case("an-array-can-be-built-from-itself", r#"a=(1 2); a=("${a[@]/1/9}"); echo "${a[*]}"; b=(1 2); b=("${b[@]}" 3); echo "${b[*]}""#),
        case("an-array-can-be-prepended-to", r#"a=(1 2); a=(0 "${a[@]}"); echo "${a[*]}"; a+=("${a[@]}"); echo "${a[*]}""#),
        // The whole arithmetic expression is expanded before any of it
        // is evaluated. A bare `$x` was left for the arithmetic lexer,
        // which reads it where it stands alone and not where it is part
        // of a larger token -- so `$((10#$x))`, the idiom for forcing
        // base ten on a zero-padded number, failed outright.
        case("a-base-prefix-in-front-of-an-expansion", r#"x=010; echo $((10#$x)) $((16#$x)); v=07; echo $((10#$v)) $((8#$v))"#),
        case("an-expansion-inside-arithmetic", r#"x=1+1; echo $((x)) $(($x)); n=3; echo $(( n * $n )); echo $((2#$(echo 101)))"#),
        // The flag letters of the declare family cluster, like every
        // other builtin's. Matching whole arguments saw neither letter
        // of `declare -ir`, so the attributes were silently dropped --
        // and `local` had no `-r` at all, which made a readonly local
        // not readonly.
        case("declare-clusters-its-flags", r#"declare -ix n=1; echo "${n@a}"; declare -ir m=1; declare -p m; declare -ax a=(1); declare -p a"#),
        case("local-clusters-its-flags", r#"f(){ local -ir v=1; declare -p v; local -ax a=(1); declare -p a; }; f"#),
        case("a-readonly-local-is-readonly", r#"f(){ local -r v=1; v=2; }; f 2>/dev/null; echo "rc=$?""#),
        // `declare -F name` prints the bare name; `declare -F` with no
        // names prints a re-readable `declare -f NAME` line for each.
        case("declare-capital-f-with-and-without-a-name", r#"f(){ :; }; g(){ :; }; declare -F f; declare -F | head -2"#),
        // `${v@u}` uppercases the first character.
        case("the-upper-first-transform", r#"x=abc; echo "${x@u}${x@U}${x@L}"; a=(one two); echo "${a[@]@u}""#),
        // `umask -p` prints the command that would set it again, which
        // is the whole point of the flag.
        case("umask-p-is-re-readable", r#"umask 022; umask -p; umask -S; umask -pS"#),
        // `printf -v 'arr[0]'` writes an array element -- the way a
        // loop fills an array without a subshell.
        case(
            "printf-v-into-an-array-element",
            r#"for i in 0 1; do printf -v "a[$i]" "v$i"; done; echo "${a[*]}"; declare -A m; printf -v "m[k]" z; echo "${m[k]}""#,
        ),
        // `&` makes a job, which means a child. Only an external and a
        // subshell took any notice of it: a builtin, a function, a
        // group, a loop and a bare assignment all ran in this shell,
        // synchronously, registering nothing -- so `$!` was unset,
        // `wait` had nothing to collect, the construct's own state
        // leaked into the shell, and an `exit` inside one took the
        // whole shell down.
        //
        // Nothing here asserts an *ordering* between what a background
        // job prints and what the shell prints next: that is a race in
        // both shells. `wait` first, then look.
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
        // `declare -f`'s layout is this shell's own (recorded as a
        // divergence), but what the output has to *do* is the same in
        // both: define the function again when another shell reads it
        // back, with a command straight after it. That is the idiom the
        // output exists for -- `ssh host "$(declare -f f); f"` -- and a
        // trailing `;` after the `}` made it `};; f`, a syntax error.
        case("a-function-definition-can-be-shipped-to-another-shell", r#"f(){ echo hi; }; bash -c "$(declare -f f); f""#),
        case(
            "a-shipped-function-keeps-its-compound-commands",
            r#"f() { local x=1; if [ "$x" = 1 ]; then echo "yes $x"; fi; for i in a b; do echo $i; done; }; bash -c "$(declare -f f); f""#,
        ),
        case("two-shipped-functions", r#"f(){ echo a; }; g(){ echo b; }; bash -c "$(declare -f f g); f; g""#),
        // -- roadmap 10: the last of the grammar's leniency ------------
        // A `;` where a command is expected is a syntax error, not an
        // empty statement -- in front of the first command of a list as
        // much as between two of them. Through `eval` so the case can
        // check the status and keep running: a syntax error takes the
        // whole script down otherwise, and bish's own wording for it is
        // its own, not bash's.
        case("empty-command-between-separators", r#"eval "echo a; ; echo b" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon", r#"eval "; echo a" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon-in-a-group", r#"eval "{ ; echo a; }" 2>/dev/null; echo "rc=$?""#),
        case("leading-semicolon-in-a-then-branch", r#"eval "if true; then ; echo x; fi" 2>/dev/null; echo "rc=$?""#),
        case("semicolon-after-a-background-ampersand", r#"eval "echo a &; echo b" 2>/dev/null; echo "rc=$?""#),
        // The other side of the same rule: blank lines are skippable
        // anywhere, and an empty case arm is still an empty case arm.
        case("blank-lines-between-commands", "eval 'echo a\n\n\necho b' 2>/dev/null; echo \"rc=$?\""),
        case("an-empty-case-arm-is-not-an-empty-command", r#"eval "case x in x) ;; esac" 2>/dev/null; echo "rc=$?""#),
    ];

    // Cases bish does not match today, each with why. Asserted to
    // *still* diverge -- fixing one fails this test until its line is
    // removed, which is the only way a list like this stays true.
    // Empty, for now. The list is the point, not its length: anything
    // found and not yet fixed belongs here with its reason, so that
    // "bish agrees with bash" never quietly means "except where it
    // doesn't".
    const DIVERGENCES: &[(&str, &str)] = &[
        // A deliberate choice, not an oversight: bish's `set -o` lists
        // only the ten names that gate real behaviour here, where bash
        // lists twenty-seven. Printing `allexport off` for an option
        // this shell does nothing with would read as support for it,
        // and `set -o allexport` would then silently not take. The same
        // principle keeps `compgen -A setopt` short. Recorded because
        // it is still a difference a script can see.
        ("set-o-lists-fewer-options", "`set -o` lists 10 options; bash lists 27, most of which bish does not implement"),
        // `declare -f` prints a function by reconstructing it from the
        // parse tree, through the serializer that exists to hand
        // functions to a self-exec'd child -- so every word comes out
        // maximally quoted (`'echo' 'yes '"${x}"`), because that is
        // what guarantees it parses back to the same command, and there
        // is no indentation.
        //
        // A display printer would fix the layout, which is most of the
        // ugliness. It would still not match bash, and the reason is
        // worth writing down: bash does *not* re-render each word from
        // its parse tree. It keeps the original spelling -- `${x}`
        // stays `${x}`, `a"b"c` stays `a"b"c`, `"a"'b'` stays
        // `"a"'b'` -- while normalising the layout around them.
        // Matching that is not a printer to be written but source spans
        // to be carried through the lexer and parser and held on every
        // word.
        //
        // What the output has to *do* is checked in CASES: it defines
        // the function again when another shell reads it back.
        ("function-body-formatting", "`declare -f f` reconstructs the body in bish's own layout, not bash's"),
        // The builtin *set* differs, legitimately: bish has builtins
        // bash does not (`abbr`, `win`, `::bish`) and lacks `bind` and
        // `logout`. Listed rather than fixed because the difference is
        // the point -- the list is honest about what this shell has.
        ("compgen-b-lists-this-shells-builtins", "`compgen -b` lists bish's own builtins and not bash's `bind`/`logout`"),
    ];

    // The cases the divergence list is about. Kept apart from `CASES`
    // so that list stays a description of what works.
    const PENDING: &[Case] = &[
        case("set-o-lists-fewer-options", r#"set -o | wc -l"#),
        case("function-body-formatting", r#"f() { :; }; declare -f f"#),
        case("compgen-b-lists-this-shells-builtins", r#"compgen -b | sort | head -3 | tr '\n' ' '; echo"#),
        // -- roadmap 10: parser leniency, the part still standing -----
        // Also not recordable, and for the same kind of reason: a
        // signal this shell was *started* with ignored is reported by
        // bash's `trap -p` as `trap -- '' SIGX`, and bish says nothing
        // about it. Seeing the difference needs the parent to have
        // ignored a signal first, which a case cannot arrange for the
        // shell that runs it.
        //
        // Not recordable here, and worth saying why: `SHLVL` counts one
        // higher through two levels of `-c`, because bash decrements it
        // before `exec`ing the last command of a `-c` and bish spawns
        // there instead. Seeing it needs the shell under test invoked
        // *by path*, and both shells report `$0` as a bare name, so a
        // case cannot name the thing it is testing.
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

    fn run(shell: &std::ffi::OsStr, script: &str, dir: &std::path::Path) -> Outcome {
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
            Ok((out, timed_out)) => {
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                Outcome { text: normalise(&text), timed_out }
            }
            Err(e) => Outcome { text: format!("<could not run: {e}>"), timed_out: false },
        }
    }

    /// What one shell did with one case.
    ///
    /// `timed_out` is separate from the text on purpose. A case that
    /// hangs gets killed and reports whatever it managed to print,
    /// which can easily be *the right answer* -- every hang found in
    /// the pipeline and process-substitution work printed exactly what
    /// bash printed and then failed to exit. Folding that into the
    /// comparison would have called all of them passes, and did: none
    /// of them was caught by this corpus, they were found by running
    /// shapes by hand and noticing a stray process.
    struct Outcome {
        text: String,
        timed_out: bool,
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
            // A hang is its own kind of wrong, and not one the text can
            // express: a case that hangs is killed and reports whatever
            // it printed first, which is very often exactly right. Said
            // out loud here so it cannot be mistaken for agreement.
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

    // Also not differential -- bash has no equivalent. A recursion that
    // has run no command and changed no argument since the last time it
    // entered the same function is not "probably" a loop: the whole
    // reachable state is identical to what it was at that earlier entry,
    // so the program is at a fixed point. That is reportable at the
    // second call rather than after a thousand of them, which is the
    // difference between naming the bug and naming the stack.
    //
    // The false-positive half matters more than the true-positive half:
    // a recursion that does anything at all must be left alone.
    #[test]
    fn a_recursion_that_cannot_terminate_is_reported_as_such() {
        let Some(bish) = bish_binary() else { return };
        let root = std::env::temp_dir().join(format!("bish-nonproductive-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        let proven: [(&str, &str); 4] = [
            ("f() { f; }; f", "called itself"),
            ("a() { b; }; b() { a; }; a", "cycle of 2 calls"),
            ("a() { b; }; b() { c; }; c() { a; }; a", "cycle of 3 calls"),
            // Looks like it does something and does not: an empty
            // branch runs no command, so this really is a fixed point.
            ("f() { case a in a) ;; esac; f; }; f", "called itself"),
        ];
        for (script, expected) in proven {
            let out = run(bish.as_os_str(), script, &root);
            assert!(out.text.contains("cannot terminate"), "{script:?} was not recognised: {:?}", out.text);
            assert!(out.text.contains(expected), "{script:?} misdescribed the cycle: {:?}", out.text);
        }

        // Each of these recurses forever too, but *productively* -- it
        // runs a command, assigns something, or reads a variable whose
        // value comes from outside the shell. None is a fixed point, so
        // none may be reported as one; they hit the stack limit instead,
        // which says "nesting level exceeded".
        for script in [
            "f() { x=1; f; }; f",
            "f() { true; f; }; f",
            "f() { f $((${1:-0}+1)); }; f",
            "f() { f $RANDOM; }; f",
            "f() { f $SECONDS; }; f",
            // Deliberately no case that spawns an external command
            // per frame. Each of these recurses until the stack runs
            // out, so that would be a thousand spawns, and the case
            // took longer than the harness's own per-case timeout once
            // the suite ran it under load. `true` above is a builtin
            // and reaches the same "this ran a command" bookkeeping;
            // an external adds a process, not coverage.
            // Arithmetic that assigns, in each of its spellings. The
            // `(( ))` command is not a simple command and so does not
            // pass the dispatch where effects are counted -- this was
            // reported as a fixed point until an arithmetic assignment
            // became an effect in its own right.
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

        // A recursion that terminates is not touched by any of this.
        let out = run(bish.as_os_str(), r#"f() { if [ "$1" -gt 0 ]; then f $(($1-1)); else echo done; fi; }; f 50"#, &root);
        assert_eq!(out.text, "done", "a bounded recursion must simply run");

        // Reported once, and the script stops there -- the same unwind
        // `FUNCNEST` gets, since there is nothing useful to run after.
        let out = run(bish.as_os_str(), "f() { f; }; f; echo unreached", &root);
        assert_eq!(out.text.matches("cannot terminate").count(), 1, "reported once per runaway, not once per frame: {:?}", out.text);
        assert!(!out.text.contains("unreached"), "the script should have stopped: {:?}", out.text);

        std::fs::remove_dir_all(&root).ok();
    }

    // Not a differential case: real bash dies on a signal for three of
    // these four, so there is nothing to compare against. What is being
    // asserted is only that bish does not -- an unbounded recursion is
    // a typo, and a typo should not dump core.
    //
    // Each shape reaches the stack by a different route. A function
    // calling itself and an `eval` calling itself both go through
    // `call_function`; a file that sources itself never makes a
    // function call at all; and a deeply parenthesised arithmetic
    // expression is the parser recursing, not the executor. One stack
    // measurement covers all four -- see stackguard's own doc comment
    // for why it is a measurement rather than a depth counter.
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
            // A `FUNCNEST` set high enough never to be reached must not
            // be a way back to the crash: the stack backstop applies
            // under it, not instead of it.
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
