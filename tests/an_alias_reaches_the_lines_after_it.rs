//! An alias defined by a script applies to the lines after it.
//!
//! bash reads a statement at a time, so an alias is defined by the time
//! the next line is read. bish reads the whole source up front, so it
//! reads the rest again when a statement changes the alias table.
//!
//! Not in `bashdiff.rs`: that corpus also goes through the serializer
//! round trip, which turns newlines into `;`, and newlines are what's
//! tested here. The corpus's cases source a file, covering
//! `run_source_here`; this covers main.rs's `-c`/script path, which
//! parses its source itself.

use std::process::Command;

fn bish(script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bish")).arg("-c").arg(script).output().expect("bish could not be run");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_later_line_sees_an_alias_the_line_before_it_defined() {
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nhi"), "A\n");
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\nhi one\nhi two"), "A one\nA two\n");
    // Still a later line with something else run in between.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\ntrue; hi"), "A\n");
    // And when the defining statement was a function call or a compound.
    assert_eq!(bish("shopt -s expand_aliases\nf(){ alias hi=\"echo A\"; }; f\nhi"), "A\n");
    assert_eq!(bish("shopt -s expand_aliases\nif true; then alias hi=\"echo A\"; fi\nhi"), "A\n");
}

#[test]
fn the_statement_that_defines_one_does_not_see_it() {
    // bash reads a whole statement before running any of it, so its rest
    // was read before the alias existed.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"; hi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    // A `&&` carries the statement across the newline: one statement.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\" &&\nhi 2>/dev/null; echo \"rc=$?\""), "rc=127\n");
    // A statement spanning lines is one statement: a use on its last line
    // is inside it, one after it isn't.
    assert_eq!(bish("shopt -s expand_aliases\nfor i in 1; do alias hi=\"echo A\"\ndone; hi 2>/dev/null; echo \"rc=$?\"\nhi"), "rc=127\nA\n");
}

#[test]
fn a_subshell_body_is_not_a_source_of_its_own() {
    // `$( )` is read separately, so an alias defined in one reaches the rest
    // of it; `( )` is read with the source around it, so it doesn't. Both
    // as bash does.
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
    // The re-read lexes the whole source again rather than a slice, so
    // `$LINENO` and the debugger's position don't shift.
    assert_eq!(bish("shopt -s expand_aliases\nalias hi=\"echo A\"\necho $LINENO\nhi\necho $LINENO"), "3\nA\n5\n");
}
