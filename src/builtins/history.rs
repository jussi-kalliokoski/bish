// `history` and `fc`: the command history, as the shell exposes it.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use std::rc::Rc;
use crate::exec::{sh_eprintln, sh_println, Shell};

// Fish-style abbreviations: `sh.abbrs`'s own doc comment covers the
// storage/trigger split (this builtin only ever stores/queries/lists;
// the actual expansion happens in editor.rs's read_line). Deliberately
// scoped down from real fish's own `abbr`: no `--rename`, no
// `--position anywhere` (always command position, fish's own default),
// no regex/function-backed abbreviations, no scope flags (`-U`/`-g`,
// meaningless here -- bish has no fish-variable-style scoping at all)
// -- just add/erase/list/show/query, the part of `abbr` people
// actually reach for day to day. An expansion *can* carry `%s`
// placeholders, which makes it a snippet rather than plain text (see
// bishedit::snippet, and `snippet::parse_order` for how a trailing
// `2 1` is told apart from two more words of expansion).
// `--lang=GLOB` scopes an abbreviation to the languages it's for
// (default `bash`, which is what the shell prompt itself counts as),
// so an abbreviation's identity here is `(name, lang)` and the same
// short name can mean one thing at a prompt and another in a Rust
// file. See `take_lang_flag` for why it's only recognized among the
// leading options.
// `-a`/`--add` is optional (`abbr NAME EXPANSION` alone means add, `abbr`
// with a recognized name misparsed as NAME would just mean "add an
// abbreviation literally named `-x`" -- an accepted, unvalidated edge
// case, same spirit as `alias`'s own lack of name validation above).
// Bare `abbr` (no args at all) shows everything, matching this
// codebase's own `alias`'s bare-listing convention rather than real
// fish's (which errors) -- consistency with the sibling builtin wins
// here since nothing else in bish already commits to fish's own
// no-args-is-an-error behavior.
// `history [N]`, `history -c`, `history -d N`.
//
// Deliberately not `-w`/`-r`/`-a`: those are about a *file*, and
// bish's history file is appended to by every concurrent bish
// process (see history.rs's own load()), so "write the in-memory
// list to the file" would mean deciding whose list wins. That is a
// real design question, not a missing flag.
pub(crate) fn run_history(sh: &mut Shell, args: &[String]) -> i32 {
    let history = Rc::clone(&sh.history);
    match args.first().map(String::as_str) {
        Some("-c") => {
            history.borrow_mut().clear();
            0
        }
        Some("-d") => {
            let Some(n) = args.get(1).and_then(|a| a.parse::<usize>().ok()) else {
                sh_eprintln!(sh, "bish: history: -d: usage: history -d OFFSET");
                return 2;
            };
            if history.borrow_mut().delete(n) {
                0
            } else {
                sh_eprintln!(sh, "bish: history: {n}: history position out of range");
                1
            }
        }
        Some(flag) if flag.starts_with('-') => {
            sh_eprintln!(sh, "bish: history: {flag}: invalid option (expected -c or -d)");
            2
        }
        other => {
            let entries = history.borrow().entries();
            // A bare count shows the *last* N, which is what makes
            // `history 20` the useful spelling it is.
            let start = match other.and_then(|a| a.parse::<usize>().ok()) {
                Some(n) => entries.len().saturating_sub(n),
                None => 0,
            };
            // bash's `HISTTIMEFORMAT`: set, and each line is
            // prefixed with when the command ran. Unset (the
            // default) and nothing changes -- which is also what an
            // entry with no recorded time shows, padded to keep the
            // commands lined up.
            let time_format = sh.var_is_set("HISTTIMEFORMAT").then(|| sh.raw_var_lookup("HISTTIMEFORMAT"));
            for (i, (entry, when)) in entries.iter().enumerate().skip(start) {
                match &time_format {
                    Some(fmt) if !fmt.is_empty() => {
                        let stamp = match when {
                            Some(secs) => crate::time::strftime_at(fmt, &crate::time::local_time_at(*secs), Some(*secs)),
                            None => String::new(),
                        };
                        sh_println!(sh, "{:5}  {}{}", i + 1, stamp, entry);
                    }
                    _ => sh_println!(sh, "{:5}  {}", i + 1, entry),
                }
            }
            0
        }
    }
}

// `fc -l [first [last]]` -- the listing half, which is what `fc` is
// actually reached for.
//
// Bare `fc` opens the last command in an editor and runs the result
// on exit. bish has an editor and could, but "run whatever comes
// back" is a different and much sharper thing than "show me what I
// ran", and shipping it as a surprise inside a listing command would
// be wrong. So it is refused by name, which is this shell's own
// convention for something it does not do yet.
pub(crate) fn run_fc(sh: &mut Shell, args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("-l") {
        sh_eprintln!(sh, "bish: fc: only `fc -l [first [last]]` is implemented (use `history` to list, and an editor to edit)");
        return 2;
    }
    let entries = sh.history.borrow().entries();
    if entries.is_empty() {
        return 0;
    }
    // bash's own defaults: the last 16, and a negative number counts
    // back from the end.
    let resolve = |arg: Option<&String>, fallback: usize| -> usize {
        match arg.and_then(|a| a.parse::<i64>().ok()) {
            Some(n) if n < 0 => entries.len().saturating_sub((-n) as usize).saturating_add(1).max(1),
            Some(n) if n > 0 => (n as usize).min(entries.len()),
            _ => fallback,
        }
    };
    let first = resolve(args.get(1), entries.len().saturating_sub(15).max(1));
    let last = resolve(args.get(2), entries.len());
    let (lo, hi) = (first.min(last), first.max(last));
    for n in lo..=hi.min(entries.len()) {
        sh_println!(sh, "{:5}\t{}", n, entries[n - 1].0);
    }
    0
}
