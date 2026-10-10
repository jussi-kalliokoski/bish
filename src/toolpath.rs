//! Where a tool a test needs is on this machine. A hardcoded
//! `/usr/bin/vim` or `/bin/cp` is a guess about the filesystem: on a Mac
//! the vim in PATH is usually Homebrew's, and a trimmed container may
//! lack `/bin`. Resolved with `exec::resolve_in_path`, as bish resolves
//! commands, so a test finds the tool the shell would.

#![cfg(test)]

/// Where `name` is, or `None`, for an optional tool the test can do
/// without (the vim corpus skips itself).
pub(crate) fn find(name: &str) -> Option<String> {
    crate::exec::resolve_in_path(name, &std::env::var("PATH").unwrap_or_default())
}

/// Where `name` is, failing the test if nowhere: for tools every machine
/// running the suite has (`sh`, `cp`, `true`).
pub(crate) fn require(name: &str) -> String {
    find(name).unwrap_or_else(|| panic!("this test needs `{name}`, and PATH has no such command"))
}
