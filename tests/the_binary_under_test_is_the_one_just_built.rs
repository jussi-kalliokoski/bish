//! Exists so that `cargo test` builds `target/debug/bish`.
//!
//! Three harnesses in this crate -- the bash corpus, the editor corpus
//! and the prompt's own paste test -- run the *binary*, found at
//! `target/debug/bish`, because what they measure is a whole shell and
//! not a function. Cargo does not build that for a bin-only crate when
//! all the tests are `#[cfg(test)]` modules inside it: it builds the
//! test harness, which is a different artifact, and leaves the plain
//! binary at whatever `cargo build` last made it.
//!
//! So `cargo test` after an edit could compare a fresh corpus against
//! yesterday's shell and pass. That is not hypothetical -- it is how
//! the paste test came to pass with its own fix disabled, twice, and
//! how a corpus could quietly stop being evidence about the code in
//! front of you.
//!
//! One integration test anywhere under `tests/` fixes it, because
//! cargo builds every target in an invocation before running any of
//! them: the bin is built for *this* file's sake, and the harnesses
//! inside the crate get a current binary as a side effect. The
//! assertion below is almost beside the point; the file's existence is
//! the mechanism.

#[test]
fn the_binary_exists_and_answers() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bish")).arg("-c").arg("echo hi").output().expect("bish could not be run");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}
