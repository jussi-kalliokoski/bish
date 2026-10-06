//! Where a tool a test needs actually is on this machine.
//!
//! Tests used to name one outright: `/usr/bin/vim`, `/bin/sh`,
//! `/bin/cp`. Every one of those is a guess about the filesystem, and
//! the guesses were Linux's -- on a Mac the vim a developer runs is
//! whichever one their PATH finds, usually Homebrew's under
//! `/opt/homebrew/bin`, and `/usr/bin/vim` is a different and older
//! build of it. On a distribution that puts everything under
//! `/usr/bin`, or in a container with a trimmed `/bin`, the guess is
//! simply wrong and the test fails for a reason that has nothing to do
//! with what it was measuring.
//!
//! Resolution goes through `exec::resolve_in_path`, which is what bish
//! itself resolves a command with, so a test finds the same tool the
//! shell under test would.

#![cfg(test)]

/// Where `name` is, or `None` when this machine has no such tool -- for
/// an optional one, where the test has something sensible to do without
/// it (the vim corpus skips itself).
pub(crate) fn find(name: &str) -> Option<String> {
    crate::exec::resolve_in_path(name, &std::env::var("PATH").unwrap_or_default())
}

/// Where `name` is, failing the test if it is nowhere.
///
/// For the tools every machine that can run this suite has -- `sh`,
/// `cp`, `true` -- where going on without one would measure nothing.
pub(crate) fn require(name: &str) -> String {
    find(name).unwrap_or_else(|| panic!("this test needs `{name}`, and PATH has no such command"))
}
