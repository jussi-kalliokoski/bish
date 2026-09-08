//! An alias defined by a script applies to the lines after it.
//!
//! bash reads one statement at a time, so an alias a statement defines
//! is in hand by the time the next line is read. bish reads a whole
//! source up front -- faster, and the reason `alias ll="ls -d"` on one
//! line and `ll /` on the next used to find no alias at all -- so it
//! reads the rest again when a statement leaves the alias table
//! different from how it found it.
//!
//! **Not in `bashdiff.rs`, though most of this behaviour is.** That
//! corpus is also run through the serializer round-trip test, which
//! flattens a script's newlines into `;` -- and newlines are exactly
//! what is under test here, so a case written with real ones would mean
//! something different on the second run. The corpus cases therefore
//! write their lines into a file and source it, which covers
//! `run_source_here`. This file covers the other entry point: the
//! top-level `-c`/script path in main.rs, which parses its source
//! itself and so has its own call into the re-reading run.

use std::process::Command;

fn bish(script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bish")).arg("-c").arg(script).output().expect("bish could not be run");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_later_line_sees_an_alias_the_line_before_it_defined() {
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nhi"), "A\n");
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nhi one\nhi two"), "A one\nA two\n");
    // Still a later line when something else runs in between.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\ntrue; hi"), "A\n");
    // And when the defining statement was a function call or a compound.
    assert_eq!(bish("shopt -s expand_aliases\nf(){ alias hi=\"echo A\"; }; f\nhi"), "A\n");
    assert_eq!(bish("shopt -s expand_aliases\nif true; then alias hi=\"echo A\"; fi\nhi"), "A\n");
}

#[test]
fn the_statement_that_defines_one_does_not_see_it() {
    // bash reads a whole statement before running any of it, so the
    // rest of that statement was read before the alias existed.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"; hi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    // A `&&` carries the statement across the newline, so this is one
    // statement too.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\" &&\nhi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    // A statement spanning lines is still one statement: the use on the
    // line it ends on is inside it, the one after it is not.
    assert_eq!(bish("shopt -s expand_aliases\nfor i in 1; do alias hi=\"echo A\"\ndone; hi 2>/dev/null; echo \"rc=$?\"\nhi"), "rc=127\nA\n");
}

#[test]
fn a_subshell_body_is_not_a_source_of_its_own() {
    // `$( )` is read separately, so an alias defined in one reaches the
    // rest of it. `( )` was read along with the source around it, and
    // does not -- which is bash's answer for both.
    assert_eq!(bish("shopt -s expand_aliases\nv=$(alias hi=\"echo A\"\nhi); echo \"[$v]\""), "[A]\n");
    assert_eq!(bish("shopt -s expand_aliases\n( alias hi=\"echo A\"\nhi ) 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    assert_eq!(bish("shopt -s expand_aliases\n{ alias hi=\"echo A\"\nhi; } 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
}

#[test]
fn expansion_has_to_be_turned_on_and_can_be_taken_away() {
    assert_eq!(bish("alias hi=\"echo A\"\nhi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nunalias hi\nhi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    // Redefinition takes effect on the line after it, like a definition.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nhi\nalias hi=\"echo B\"\nhi"), "A\nB\n");
}

#[test]
fn line_numbers_stay_absolute_across_a_re_read() {
    // The re-read runs the whole source through the lexer again rather
    // than a slice of it, precisely so that $LINENO and the debugger's
    // idea of where it is do not shift under a script that defines an
    // alias.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\necho $LINENO\nhi\necho $LINENO"), "3\nA\n5\n");
}
