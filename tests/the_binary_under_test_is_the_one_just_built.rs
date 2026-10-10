//! Exists so that `cargo test` builds `target/debug/bish`.
//!
//! The bash corpus, the editor corpus and the prompt's paste test run the
//! binary, as they measure a whole shell. With only `#[cfg(test)]`
//! modules, cargo builds the test harness, a different artifact, and
//! leaves the binary as `cargo build` last made it, so a corpus could
//! pass against a stale shell.
//!
//! One integration test under `tests/` fixes that: cargo builds every
//! target before running any, so the binary is built for this file and
//! the harnesses get it too. The file's existence is the point.

#[test]
fn the_binary_exists_and_answers() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bish")).arg("-c").arg("echo hi").output().expect("bish could not be run");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}
