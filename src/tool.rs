// `bish tool <subcommand>`: standalone tooling on the shell's and
// editor's bash machinery. main.rs routes here before its `-c` and
// script handling. The rule engines live in `bishedit::lint` and
// `bishedit::format`; this is the command-line wrapper.
use crate::bishedit::format::BashFormatter;
use crate::bishedit::lint::{BashLinter, Diagnostic, Fix, Linter};
use std::io::{self, Read, Write};

// Used for both the "expected:" line and "did you mean", so they agree.
//
// `mcp-server` is left out while it settles: it works, but listing it
// would promise its shape will hold, and an unlisted entry point can
// still change.
const SUBCOMMANDS: &[&str] = &["check", "format", "lsp-server", "debug", "edit", "keys"];

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("check") => run_check(&args[1..]),
        Some("format") => run_format(&args[1..]),
        Some("lsp-server") => crate::lspserver::run(&args[1..]),
        // Unlisted on purpose; see `SUBCOMMANDS`.
        Some("mcp-server") => crate::mcp::run(&args[1..]),
        Some("debug") => run_debug(&args[1..]),
        Some("edit") => run_edit(&args[1..]),
        Some("keys") => run_keys(&args[1..]),
        Some(other) => {
            let hint = crate::suggest::did_you_mean(other, SUBCOMMANDS.iter().copied());
            eprintln!("bish tool: unknown subcommand '{other}'{hint} (expected: {})", SUBCOMMANDS.join(", "));
            2
        }
        None => {
            eprintln!(
                "bish tool: expected a subcommand (usage: bish tool check [--fix] [FILE...], bish tool format [--check] [FILE...], bish tool lsp-server, bish tool debug FILE, bish tool edit [--hex] FILE..., bish tool keys [SEQUENCE | --action TEXT])"
            );
            2
        }
    }
}

// `bish tool keys`: the editor's key bindings, answered by the editor.
// Both directions: what `D` does, and which key deletes to end of line.
// Built by `keymap::key_index`, which asks
// `vimkeys::describe_key_sequence` about every candidate, so it can't go
// stale.
fn run_keys(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            println!("usage: bish tool keys [SEQUENCE | --action TEXT]");
            println!();
            println!("  (no argument)    every key sequence and what it does");
            println!("  SEQUENCE         what that sequence does, e.g. `bish tool keys D`");
            println!("  --action TEXT    every key whose action mentions TEXT");
            0
        }
        Some("--action") | Some("-a") => match args.get(1) {
            Some(needle) => {
                let needle = needle.to_lowercase();
                let matched: Vec<(String, String)> =
                    crate::keymap::key_index().into_iter().filter(|(_, action)| action.to_lowercase().contains(&needle)).collect();
                if matched.is_empty() {
                    eprintln!("bish tool keys: no action mentions '{needle}'");
                    return 1;
                }
                print_index(&matched);
                0
            }
            None => {
                eprintln!("usage: bish tool keys --action TEXT");
                2
            }
        },
        Some(sequence) => match crate::keymap::parse_keys(sequence) {
            Err(e) => {
                eprintln!("bish tool keys: {e}");
                2
            }
            Ok(keys) => match crate::bishedit::vimkeys::describe_key_sequence(&keys) {
                Ok(action) => {
                    println!("{}\t{action}", crate::keymap::format_keys(&keys));
                    0
                }
                Err(reason) => {
                    eprintln!("bish tool keys: {sequence}: {reason}");
                    1
                }
            },
        },
        None => {
            print_index(&crate::keymap::key_index());
            0
        }
    }
}

// Tab-separated, for scripts as well as reading, like `::bish map`'s
// listing. Through a locked handle, as `println!` panics on the EPIPE
// when a `grep` or `head` reader stops early.
fn print_index(rows: &[(String, String)]) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (keys, action) in rows {
        if writeln!(out, "{keys}\t{action}").is_err() {
            return;
        }
    }
}

// `bish tool debug <script>`: an interactive session in the windowed
// editor with a debug session attached (`repl::run_edit_debug`; see
// debugger.rs).
fn run_debug(args: &[String]) -> i32 {
    match args.first() {
        Some(path) => crate::repl::run_edit_debug(path),
        None => {
            eprintln!("usage: bish tool debug FILE");
            2
        }
    }
}

// `bish tool edit <file>...`: the windowed editor (`repl::run_edit`)
// without the window and tab chrome. Arguments are parsed as the `e`
// builtin parses them (`fileeditor::parse_edit_args`).
fn run_edit(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("usage: bish tool edit [--hex] [--readonly] FILE...");
        return 2;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: bish tool edit [--hex] [--readonly] FILE...");
        println!("  opens each FILE in the real editor, the first one in front.");
        println!("  --hex/--readonly are per-file: they apply to the FILE that follows");
        println!("  them, so `edit script.sh --hex core.bin` opens one of each.");
        return 0;
    }
    crate::repl::run_edit(args)
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
        // Rewritten only if something changed, so `--fix` over clean scripts
        // leaves their mtimes alone.
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

fn print_format_usage() {
    eprintln!("usage: bish tool format [--check] [FILE...]");
    eprintln!("  reformats bash script(s): tabs for indentation, and `then`/`do`/`in`/`{{`");
    eprintln!("  joined onto their own header line, one indent level per block until the");
    eprintln!("  matching `fi`/`done`/`esac`/`}}`. With no FILE arguments, reads a single");
    eprintln!("  script from stdin. Rewrites in place for a real file, to stdout for");
    eprintln!("  stdin -- unless --check is given, which only reports what would change");
    eprintln!("  (same [code] format as `bish tool check`) and writes nothing.");
}

fn run_format(args: &[String]) -> i32 {
    let mut check_only = false;
    let mut files: Vec<&str> = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--check" => check_only = true,
            "-h" | "--help" => {
                print_format_usage();
                return 0;
            }
            other if other.starts_with('-') && other != "-" => {
                eprintln!("bish tool format: unrecognized option '{other}'");
                print_format_usage();
                return 2;
            }
            other => files.push(other),
        }
    }

    if files.is_empty() {
        return format_stdin(check_only);
    }

    let mut worst = 0;
    for path in files {
        worst = worst.max(format_file(path, check_only));
    }
    worst
}

fn format_stdin(check_only: bool) -> i32 {
    let mut text = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut text) {
        eprintln!("bish tool format: error reading stdin: {e}");
        return 2;
    }
    let diagnostics = match BashFormatter.check(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("<stdin>: {e}");
            return 2;
        }
    };
    if check_only {
        report_remaining("<stdin>", &text, diagnostics.iter());
        if diagnostics.is_empty() { 0 } else { 1 }
    } else {
        let (fixed, _, remaining) = apply_fixes(&text, &diagnostics);
        print!("{fixed}");
        let _ = io::stdout().flush();
        report_remaining("<stdin>", &text, remaining.iter().copied());
        if remaining.is_empty() { 0 } else { 1 }
    }
}

fn format_file(path: &str, check_only: bool) -> i32 {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bish tool format: {path}: {e}");
            return 2;
        }
    };
    let diagnostics = match BashFormatter.check(&text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("bish tool format: {path}: {e}");
            return 2;
        }
    };
    if check_only {
        report_remaining(path, &text, diagnostics.iter());
        if diagnostics.is_empty() { 0 } else { 1 }
    } else {
        let (fixed, applied, remaining) = apply_fixes(&text, &diagnostics);
        // Only if something changed, as in `check_file`.
        if applied > 0
            && let Err(e) = std::fs::write(path, &fixed)
        {
            eprintln!("bish tool format: {path}: error writing: {e}");
            return 2;
        }
        report_remaining(path, &text, remaining.iter().copied());
        if remaining.is_empty() { 0 } else { 1 }
    }
}

// Applies every diagnostic's fix in one pass, returning the fixed text,
// how many applied, and the diagnostics left (no fix, or not applied).
// Applied by descending `start`, so no edit shifts one still to come.
// A fix overlapping one already applied is skipped rather than risk a
// corrupted splice.
fn apply_fixes<'a>(text: &str, diagnostics: &'a [Diagnostic]) -> (String, usize, Vec<&'a Diagnostic>) {
    // Indexed with each fix, so one skipped for overlap still shows in
    // `remaining` via its diagnostic.
    let mut candidates: Vec<(usize, &Fix)> = diagnostics.iter().enumerate().filter_map(|(i, d)| d.fix.as_ref().map(|f| (i, f))).collect();
    crate::sort::sort_by_key(&mut candidates, |(_, f)| std::cmp::Reverse(f.start));

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
        println!("{path}:{line}:{col}: [{}] {}", d.label(), d.message);
    }
}

// 1-based (line, column), as compilers report, computed here since
// only this path needs it; lint spans are char offsets.
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
    use std::borrow::Cow;

    fn diag(start: usize, end: usize, fix: Option<Fix>) -> Diagnostic {
        Diagnostic { start, end, severity: Severity::Warning, code: Cow::Borrowed("test"), source: None, message: "test".to_string(), fix }
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
        // Overlapping fixes: the earlier-applied (higher start) wins and the
        // other stays in `remaining`.
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
