//! `git` in a fixture repository, unaffected by the machine's setup.
//!
//! Tests build a repository in a temp directory and run real `git` in it,
//! and the machine's configuration can break or hang them:
//!
//! - `commit.gpgsign` makes fixture commits sign, and a passphrase
//!   prompt stops the suite.
//! - `init.defaultBranch` decides the branch name; tests expect `main`.
//! - `core.hooksPath` runs the developer's hooks in the fixture.
//! - `core.excludesFile` hides files a fixture just made.
//! - `log.showSignature`, `color.ui = always` and the like change output
//!   tests parse.
//!
//! So: no system or global config, no identity from the machine, no
//! hooks, no terminal prompts, and no `GIT_DIR` from a surrounding git
//! process (a suite run from a hook has one).

#![cfg(test)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A `git` command in `dir` that the machine cannot influence.
pub(crate) fn command(dir: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(dir)
        .stdin(Stdio::null())
        // `/dev/null` is a readable, empty config file, git's documented
        // way to say there's no config.
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // For git older than 2.32.
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // For anything that still looks in a home directory, including
        // `~/.config/git/`.
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        // With no config, the identity comes from the environment; some tests
        // assert on it.
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        // Nothing here talks to a remote, so a credential request should
        // fail, not wait.
        .env("GIT_TERMINAL_PROMPT", "0")
        // A suite run from a git hook inherits these, pointing fixtures at
        // the real repository.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // "Is this a repository?" is about `dir` alone: a fixture that
        // failed to initialise finds nothing above it.
        .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
        // A built-in default, not config, so it's given explicitly;
        // harmless for commands other than `init`.
        .args(["-c", "init.defaultBranch=main"]);
    git
}

/// Runs `git` in `dir` and returns everything it said, failures
/// included, for a test asking a question.
pub(crate) fn output(dir: &Path, args: &[&str]) -> Output {
    command(dir).args(args).output().expect("git runs")
}

/// Runs `git` in `dir`, failing the test with git's complaint if it
/// didn't work.
pub(crate) fn run(dir: &Path, args: &[&str]) {
    let out = output(dir, args);
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
}

/// A fresh repository in `dir`, on `main`, ready to be committed in.
pub(crate) fn init(dir: &Path) {
    run(dir, &["init", "-q"]);
}
