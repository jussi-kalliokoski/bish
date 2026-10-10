//! What a subshell moves about the process, it puts back.
//!
//! The cwd, the umask and fds 0/1/2 belong to the process, not a `Shell`.
//! A `( )` or `$( )` is its own Shell but not its own process, so it
//! restores them on the way out. Each is recorded by whatever moves it,
//! and only what moved is put back.
//!
//! The cwd and umask halves, and most of the fd half, are in
//! `bashdiff.rs`, checked against bash. These assert the file's exact
//! bytes, and that a top-level `exec > file` keeps applying, which
//! leaves a script no stdout to report on itself.

use std::process::Command;

fn bish_in(dir: &std::path::Path, script: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bish")).arg("-c").arg(script).current_dir(dir).output().expect("bish could not be run");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    text
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bish-procstate-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_bare_exec_redirect_does_not_escape_the_subshell_that_made_it() {
    let dir = tmp("stdout");
    // Without the restore, everything after went into the file too,
    // including in the enclosing script.
    let out = bish_in(&dir, "( exec > out.txt; echo inside ); echo outside; echo done");
    assert_eq!(out, "outside\ndone\n", "output after the subshell went somewhere else");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "inside\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bare_exec_stderr_redirect_does_not_escape_either() {
    let dir = tmp("stderr");
    let out = bish_in(&dir, "( exec 2> err.txt; echo e >&2 ); echo e2 >&2; echo done");
    assert_eq!(out, "done\ne2\n");
    assert_eq!(std::fs::read_to_string(dir.join("err.txt")).unwrap(), "e\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bare_exec_redirect_in_a_nested_subshell_unwinds_one_level_at_a_time() {
    let dir = tmp("nested");
    let out = bish_in(&dir, "( ( exec > out.txt; echo deep ); echo mid ); echo top");
    assert_eq!(out, "mid\ntop\n");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "deep\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bare_exec_at_the_top_level_is_still_persistent() {
    // Restoring is a subshell's business: a top-level `exec > file` means it
    // and keeps applying.
    let dir = tmp("toplevel");
    let out = bish_in(&dir, "exec > out.txt; echo one; echo two");
    assert_eq!(out, "", "nothing should have reached the real stdout");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "one\ntwo\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_subshell_that_redirects_nothing_leaves_the_descriptors_alone() {
    // Nothing recorded, nothing restored, and output carries on as before.
    let dir = tmp("untouched");
    let out = bish_in(&dir, "echo before; ( : ); v=$(:); ( echo in ); echo after");
    assert_eq!(out, "before\nin\nafter\n");
    let _ = std::fs::remove_dir_all(&dir);
}
