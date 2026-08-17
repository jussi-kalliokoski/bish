// `bish tool <subcommand>` -- a small namespace for standalone,
// non-interactive tooling built on top of the same bash-language
// machinery the shell/editor already have, reached via `bish tool ...`
// on the real command line (see main.rs's own dispatch, which routes
// here *before* its ordinary `-c`/script-path handling, since `tool`
// isn't a script to run). `check` is the first subcommand; `format` and
// `lsp-server` are the next two planned (see bishedit::lint's own doc
// comment for why the actual rule engine lives there instead of here --
// this module is just the CLI wrapper around it).
use crate::bishedit::lint::{BashLinter, Diagnostic, Fix, Linter};
use std::io::{self, Read, Write};

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("check") => run_check(&args[1..]),
        Some(other) => {
            eprintln!("bish tool: unknown subcommand '{other}' (expected: check)");
            2
        }
        None => {
            eprintln!("bish tool: expected a subcommand (usage: bish tool check [--fix] [FILE...])");
            2
        }
    }
}

fn print_check_usage() {
    eprintln!("usage: bish tool check [--fix] [FILE...]");
    eprintln!("  lints bash script(s) for non-cosmetic issues (word-splitting/globbing");
    eprintln!("  hazards, masked command-substitution exit statuses, ...). With no FILE");
    eprintln!("  arguments, reads a single script from stdin. --fix rewrites every");
    eprintln!("  diagnostic that has an automatic fix in place (in-place for a real file,");
    eprintln!("  to stdout for stdin).");
}

fn run_check(args: &[String]) -> i32 {
    let mut fix = false;
    let mut files: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--fix" => fix = true,
            "-h" | "--help" => {
                print_check_usage();
                return 0;
            }
            other if other.starts_with('-') && other != "-" => {
                eprintln!("bish tool check: unrecognized option '{other}'");
                print_check_usage();
                return 2;
            }
            other => files.push(other),
        }
    }

    if files.is_empty() {
        return check_stdin(fix);
    }

    let mut worst = 0;
    for path in files {
        worst = worst.max(check_file(path, fix));
    }
    worst
}

fn check_stdin(fix: bool) -> i32 {
    let mut text = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut text) {
        eprintln!("bish tool check: error reading stdin: {e}");
        return 2;
    }
    let diagnostics = BashLinter.check(&text);
    if fix {
        let (fixed, applied, remaining) = apply_fixes(&text, &diagnostics);
        print!("{fixed}");
        let _ = io::stdout().flush();
        report_remaining("<stdin>", &text, remaining.iter().copied());
        if applied > 0 {
            eprintln!("<stdin>: fixed {applied} issue(s)");
        }
        if remaining.is_empty() { 0 } else { 1 }
    } else {
        report_remaining("<stdin>", &text, diagnostics.iter());
        if diagnostics.is_empty() { 0 } else { 1 }
    }
}

fn check_file(path: &str, fix: bool) -> i32 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bish tool check: {path}: {e}");
            return 2;
        }
    };
    let diagnostics = BashLinter.check(&text);
    if fix {
        let (fixed, applied, remaining) = apply_fixes(&text, &diagnostics);
        // Only rewrite the file when something actually changed --
        // touching every scanned file's mtime even when there was
        // nothing to fix would be a surprising side effect of a plain
        // `--fix` run over a directory of already-clean scripts.
        if applied > 0 {
            if let Err(e) = std::fs::write(path, &fixed) {
                eprintln!("bish tool check: {path}: error writing fix: {e}");
                return 2;
            }
            eprintln!("{path}: fixed {applied} issue(s)");
        }
        report_remaining(path, &text, remaining.iter().copied());
        if remaining.is_empty() { 0 } else { 1 }
    } else {
        report_remaining(path, &text, diagnostics.iter());
        if diagnostics.is_empty() { 0 } else { 1 }
    }
}

// Applies every diagnostic's own fix (skipping the ones with none) in
// one pass and returns (fixed_text, applied_count, remaining_diagnostics
// -- everything left over, whether it had no fix at all or the fix just
// wasn't applied). Sorted by descending `start` before splicing: earlier
// edits then never need their own offsets adjusted for a later one that
// already happened, since every edit still to come sits entirely before
// the point already spliced. Fixes are assumed non-overlapping (true for
// both of today's rules -- see unquoted_expansion/masked_return_value's
// own doc comments) but a defensive check still skips any fix whose span
// overlaps one already applied, rather than risk a corrupted splice.
fn apply_fixes<'a>(text: &str, diagnostics: &'a [Diagnostic]) -> (String, usize, Vec<&'a Diagnostic>) {
    // Indexed alongside each Fix so a fix skipped for overlapping one
    // already applied (the defensive case -- see this function's own
    // doc comment) still surfaces in `remaining` via its own Diagnostic,
    // instead of silently vanishing from every report just because it
    // *had* a fix that didn't end up applying.
    let mut candidates: Vec<(usize, &Fix)> = diagnostics.iter().enumerate().filter_map(|(i, d)| d.fix.as_ref().map(|f| (i, f))).collect();
    candidates.sort_by_key(|(_, f)| std::cmp::Reverse(f.start));

    let mut chars: Vec<char> = text.chars().collect();
    let mut applied_indices = std::collections::HashSet::new();
    let mut last_applied_start = chars.len() + 1;
    for (i, f) in &candidates {
        if f.end > last_applied_start {
            continue;
        }
        chars.splice(f.start..f.end, f.replacement.chars());
        last_applied_start = f.start;
        applied_indices.insert(*i);
    }

    let applied = applied_indices.len();
    let remaining = diagnostics.iter().enumerate().filter(|(i, _)| !applied_indices.contains(i)).map(|(_, d)| d).collect();
    (chars.into_iter().collect(), applied, remaining)
}

fn report_remaining<'a>(path: &str, text: &str, diagnostics: impl Iterator<Item = &'a Diagnostic>) {
    let chars: Vec<char> = text.chars().collect();
    for d in diagnostics {
        let (line, col) = line_col(&chars, d.start);
        println!("{path}:{line}:{col}: [{}] {}", d.code, d.message);
    }
}

// 1-based (line, column), matching every other compiler/linter's own
// convention -- computed by scanning rather than kept alongside each
// Diagnostic, since this is the only place in the whole check/--fix path
// that ever needs it (bishedit::lint's own char-offset spans are what
// both --fix's splicing and a future editor/LSP consumer actually want).
fn line_col(chars: &[char], offset: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for &c in chars.iter().take(offset) {
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bishedit::lint::Severity;

    fn diag(start: usize, end: usize, fix: Option<Fix>) -> Diagnostic {
        Diagnostic { start, end, severity: Severity::Warning, code: "test", message: "test".to_string(), fix }
    }

    #[test]
    fn apply_fixes_splices_multiple_non_overlapping_fixes_in_one_pass() {
        let text = "echo $a $b";
        let diags = vec![
            diag(5, 7, Some(Fix { start: 5, end: 7, replacement: "\"$a\"".to_string() })),
            diag(8, 10, Some(Fix { start: 8, end: 10, replacement: "\"$b\"".to_string() })),
        ];
        let (fixed, applied, remaining) = apply_fixes(text, &diags);
        assert_eq!(fixed, "echo \"$a\" \"$b\"");
        assert_eq!(applied, 2);
        assert!(remaining.is_empty());
    }

    #[test]
    fn apply_fixes_leaves_diagnostics_with_no_fix_in_remaining() {
        let text = "local x=$(cmd) y=2";
        let diags = vec![diag(0, 18, None)];
        let (fixed, applied, remaining) = apply_fixes(text, &diags);
        assert_eq!(fixed, text);
        assert_eq!(applied, 0);
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn apply_fixes_skips_a_fix_overlapping_one_already_applied() {
        // Defensive only -- today's rules never actually produce
        // overlapping fixes (see apply_fixes's own doc comment) -- this
        // just confirms a corrupted splice can't happen if that ever
        // changes: the earlier (higher-start) fix wins, the overlapping
        // one is left in `remaining` untouched.
        let text = "abcdef";
        let diags = vec![
            diag(2, 5, Some(Fix { start: 2, end: 5, replacement: "X".to_string() })),
            diag(0, 3, Some(Fix { start: 0, end: 3, replacement: "Y".to_string() })),
        ];
        let (fixed, applied, remaining) = apply_fixes(text, &diags);
        assert_eq!(fixed, "abXf");
        assert_eq!(applied, 1);
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn line_col_tracks_newlines() {
        let text: Vec<char> = "ab\ncd\nef".chars().collect();
        assert_eq!(line_col(&text, 0), (1, 1));
        assert_eq!(line_col(&text, 2), (1, 3));
        assert_eq!(line_col(&text, 3), (2, 1));
        assert_eq!(line_col(&text, 7), (3, 2));
    }
}
