//! `git` in a fixture repository, with the machine kept out of it.
//!
//! Tests that need a repository build one in a temp directory and then
//! run real `git` in it. Every such call used to inherit whatever the
//! machine it runs on has configured, which is a long list of ways for a
//! test to fail -- or hang -- for reasons that have nothing to do with
//! bish:
//!
//! - `commit.gpgsign = true`, which plenty of people set globally, makes
//!   every fixture commit try to sign. With a passphrase-protected key
//!   that means a pinentry prompt, and the suite stops dead on a
//!   question nobody is there to answer.
//! - `init.defaultBranch` decides what the branch in a fresh fixture is
//!   called, and tests look for `main` by name. Without this module they
//!   passed on a machine configured one way and failed on another.
//! - `core.hooksPath` runs the developer's own hooks inside the fixture,
//!   and a `pre-commit` that runs their formatter can fail the commit.
//! - `core.excludesFile` hides files a fixture just created, which is
//!   precisely what the gitignore comparison is measuring.
//! - `log.showSignature`, `color.ui = always` and friends add lines or
//!   escape codes to output a test parses.
//!
//! So: no system config, no global config, no user identity from the
//! machine, no hooks, no terminal to prompt at, and no `GIT_DIR` from a
//! surrounding git process (a suite run from a hook has one, and it
//! points at the wrong repository). What a fixture is, it is because
//! this module said so.

#![cfg(test)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A `git` command in `dir` that the machine cannot influence.
pub(crate) fn command(dir: &Path) -> Command {
    let mut git = Command::new("git");
    git.current_dir(dir)
        .stdin(Stdio::null())
        // `/dev/null` is a readable, empty config file, which is git's
        // own documented way to say "there is no config here".
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        // For a git older than the two above (2.32).
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // The last resort for anything that still looks in a home
        // directory of its own accord, including `~/.config/git/`.
        .env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        // With no config to read an identity from, the environment is
        // where a fixture's commits get one.
        // The same identity the fixtures used to set with `git config
        // user.*` of their own, which some of them assert on.
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        // Nothing here talks to a remote, so anything that wants a
        // credential is a mistake worth failing on rather than waiting
        // for.
        .env("GIT_TERMINAL_PROMPT", "0")
        // A suite run from inside a git hook inherits these, and they
        // would silently point every fixture call at the real repository.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        // Keeps "is this a repository?" a question about `dir` alone: a
        // fixture that failed to initialise finds nothing above itself
        // instead of whatever repository the temp directory happens to
        // sit in.
        .env("GIT_CEILING_DIRECTORIES", std::env::temp_dir())
        // Not config but a built-in default, so this one has to be said
        // out loud even with the config gone. Harmless on the commands
        // that are not `init`.
        .args(["-c", "init.defaultBranch=main"]);
    git
}

/// Runs `git` in `dir` and hands back everything it said, failure
/// included -- for a caller that is asking a question rather than
/// setting a fixture up.
pub(crate) fn output(dir: &Path, args: &[&str]) -> Output {
    command(dir).args(args).output().expect("git runs")
}

/// Runs `git` in `dir`, and fails the test with git's own complaint if
/// it did not work.
pub(crate) fn run(dir: &Path, args: &[&str]) {
    let out = output(dir, args);
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
}

/// A fresh repository in `dir`, on `main`, ready to be committed in.
pub(crate) fn init(dir: &Path) {
    run(dir, &["init", "-q"]);
}
