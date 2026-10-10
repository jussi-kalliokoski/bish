// Default interactive prompt: "user@host:path (branch)<terminator> "
// (no space before the terminator, matching classic `\u@\h:\w\$ ` PS1
// style). The path is shown as it is until the width says otherwise, at
// which point whole directories are left out and, only if that is still
// not enough, the survivors are abbreviated to what they start with and
// an ellipsis (e.g. "~/…/D…/bish") -- a name cut down to `D` would read
// as a directory actually called that,
// and the terminator glyph is "$" for a normal user or "#" for root. The
// `(branch)` segment (git::head_status) only appears inside a real git
// repo, colored green when the tree is clean, yellow-with-a-trailing-`*`
// when it isn't. Command mode (see command_mode_prompt) uses a
// deliberately different, minimal prompt rather than a variant of this
// one.

use crate::bishedit::unicode_width::char_width;
use crate::exec::{self, Shell};

const RESET: &str = "\x1b[0m";
const USER_HOST_COLOR: &str = "\x1b[1;32m"; // bold green
const ROOT_USER_HOST_COLOR: &str = "\x1b[1;31m"; // bold red, a deliberate warning color
const PATH_COLOR: &str = "\x1b[1;36m"; // bold cyan
const ROOT_PATH_COLOR: &str = "\x1b[1;31m"; // bold red
const OK_COLOR: &str = "\x1b[1;32m"; // bold green
const ERR_COLOR: &str = "\x1b[1;31m"; // bold red
// Deliberately distinct from both terminator colors above, so the armed/
// command-mode state reads as "a different mode," not just a recolored
// version of the normal prompt.
const CMD_MODE_COLOR: &str = "\x1b[1;35m"; // bold magenta
const GIT_CLEAN_COLOR: &str = "\x1b[0;32m"; // plain green -- deliberately dimmer than the bold user@host/path segments, secondary info
const GIT_DIRTY_COLOR: &str = "\x1b[0;33m"; // plain yellow

/// What a component may take of the line, in columns, from its own
/// percentage bishopt.
///
/// A percentage rather than a column count because what is worth
/// bounding is the share of *this* terminal the prompt eats: the same
/// number is too tight in a pane and too loose in a full window. The
/// three bounds are independent maxima and not a partition -- a
/// component rarely reaches its own -- so they do not have to sum to a
/// hundred, though all three at once is what the prompt's worst case
/// then is.
///
/// `0` is no limit, the same spelling `wrap_column` uses for off.
fn budget(percent: i64, cols: usize) -> Option<usize> {
    match percent {
        0 => None,
        p => Some((cols.saturating_mul(p.clamp(0, 100) as usize) / 100).max(1)),
    }
}

/// How many columns `text` draws in.
fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// `text`, cut to `budget` columns with an ellipsis where it was cut.
///
/// The ellipsis is a column of its own, so a budget of 1 is the ellipsis
/// alone -- which is still the honest answer: something was there.
pub(crate) fn ellipsize_end(text: &str, budget: usize) -> String {
    if width(text) <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return "\u{2026}".to_string();
    }
    let mut out = String::new();
    let mut taken = 0;
    for c in text.chars() {
        let w = char_width(c);
        if taken + w > budget - 1 {
            break;
        }
        taken += w;
        out.push(c);
    }
    out.push('\u{2026}');
    out
}

/// The path, fitted by leaving out whole directories from the middle.
///
/// Four things it will do, in that order, so that the cheapest loss
/// always comes first:
///
/// 1. nothing, when the path already fits;
/// 2. leave out directories, keeping as many of the ones nearest the end
///    as will fit *in full* -- an ellipsis of its own standing in for
///    those left out, since the immediate parents of where you are say
///    more about it than the top of the tree does;
/// 3. when not even one full name will fit, abbreviate the survivors to
///    what each starts with instead -- `b…/s…/billing` says there were
///    two levels where leaving them out says there were none, and this
///    is the only thing abbreviating is for;
/// 4. cut the final directory's own name, which is the last thing anyone
///    wants to lose and so the last thing this takes.
///
/// Abbreviating is deliberately late. It used to happen to every parent
/// always, whatever the width, which cost a column per level for
/// information nobody had asked to lose.
pub(crate) fn fit_path(display: &str, budget: usize) -> String {
    if width(display) <= budget {
        return display.to_string();
    }
    let parts: Vec<&str> = display.split('/').collect();
    // A root with no directories under it has nothing to leave out.
    if parts.len() < 3 {
        return ellipsize_end(display, budget);
    }
    let (base, last) = (parts[0], parts[parts.len() - 1]);
    let middle = &parts[1..parts.len() - 1];

    // Full names first, as many of them as fit; only when not one of
    // them will fit does abbreviating buy a level or two instead. A name
    // is worth more than a count of levels: `…/services/billing` says
    // where you are, where `…/c…/a…/b…/s…/billing` says how deep you are
    // and leaves you guessing -- so the second shape is what happens
    // when there is no room for the first, not an upgrade over it.
    for abbreviated in [false, true] {
        for keep in (1..middle.len()).rev() {
            let names: String = middle[middle.len() - keep..]
                .iter()
                .map(|p| match abbreviated {
                    true => format!("{}/", abbreviate_parent(p)),
                    false => format!("{p}/"),
                })
                .collect();
            let candidate = format!("{base}/\u{2026}/{names}{last}");
            if width(&candidate) <= budget {
                return candidate;
            }
        }
    }

    let frame = format!("{base}/\u{2026}/");
    // Strictly less: the final name needs a column of its own, even when
    // all that fits in it is the ellipsis.
    match width(&frame) < budget {
        true => format!("{frame}{}", ellipsize_end(last, budget - width(&frame))),
        // Narrower than the frame itself: nothing structural survives, so
        // fall back to plain truncation rather than to a shape that
        // claims the path has parts it cannot show.
        false => ellipsize_end(display, budget),
    }
}

/// A branch name, fitted by shortening each of its `/` segments.
///
/// The last segment is the branch; the ones before it are the namespace
/// it was filed under. So the last gets what it needs first, and what is
/// left over is shared evenly among the others -- every one of which
/// keeps at least a character, because `f/a/thing` still says there were
/// two levels above it where dropping them says there were none.
///
/// Every segment that loses characters says so with an ellipsis, the
/// final one included. A segment cut silently claims to be a name it is
/// not -- `f/a/thing` reads as a branch filed under `f` and `a` -- so a
/// segment that has to be cut needs two columns: one character and the
/// mark. Which is why the floor below is two, for a name long enough to
/// need it.
pub(crate) fn fit_branch(branch: &str, budget: usize) -> String {
    if width(branch) <= budget {
        return branch.to_string();
    }
    let parts: Vec<&str> = branch.split('/').collect();
    if parts.len() < 2 {
        return ellipsize_end(branch, budget);
    }
    let separators = parts.len() - 1;
    // The floor per segment: a name that fits in one column keeps it, and
    // one that does not needs two -- a character and the ellipsis that
    // says there was more. Under that there is no honest shape to draw.
    let floor: Vec<usize> = parts.iter().map(|p| width(p).min(2)).collect();
    let floors: usize = floor.iter().sum();
    if separators + floors > budget {
        return ellipsize_end(branch, budget);
    }
    let last = parts.len() - 1;
    let mut share = floor.clone();
    let mut spare = budget - separators - floors;

    // The branch itself first, up to its own length.
    let wanted = width(parts[last]).saturating_sub(share[last]).min(spare);
    share[last] += wanted;
    spare -= wanted;

    // Then the namespace, a column at a time around the segments that
    // still want one -- which is what makes it an even share rather than
    // the first segment taking everything.
    while spare > 0 {
        let hungry: Vec<usize> = (0..last).filter(|i| share[*i] < width(parts[*i])).collect();
        if hungry.is_empty() {
            break;
        }
        for i in hungry {
            if spare == 0 {
                break;
            }
            share[i] += 1;
            spare -= 1;
        }
    }

    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('/');
        }
        // Every segment, the namespace included, says when it was cut.
        out.push_str(&ellipsize_end(part, share[i]));
    }
    out
}

fn username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

// `git status`'s branch/dirty segment, or empty outside a repo (or with
// no `git` on $PATH -- `git::head_status`'s own doc comment covers why
// those two cases aren't told apart here).
fn git_segment(cwd: &std::path::Path, budget: Option<usize>, owned_default: &str) -> String {
    let fitted = |branch: &str, taken: usize| match budget {
        // The parentheses and the dirty marker are part of the segment
        // the budget is about, so the name itself gets what is left of
        // it.
        Some(columns) => fit_branch(branch, columns.saturating_sub(taken)),
        None => branch.to_string(),
    };
    // The `git` capability: in a directory you have not trusted, bish
    // reads the branch from .git/HEAD and runs no git in the worktree
    // (see git::head_status), so a repository's own config cannot turn a
    // prompt draw into code execution.
    let trusted = crate::trust::is_trusted(cwd, "git", owned_default);
    match crate::git::head_status(cwd, trusted) {
        Some(status) if status.dirty => format!(" {GIT_DIRTY_COLOR}({}*){RESET}", fitted(&status.branch, 3)),
        Some(status) => format!(" {GIT_CLEAN_COLOR}({}){RESET}", fitted(&status.branch, 2)),
        None => String::new(),
    }
}

fn prefix(shell: &Shell, is_root: bool, cols: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = display_path(&shell.cwd.to_string_lossy(), &home);
    let host = format!("{}@{}", username(), exec::get_hostname());
    // Each component against its own bound, each in the way that loses
    // the least of what it is for: a path gives up whole directories, a
    // branch shortens the namespace it is filed under, and `user@host`
    // -- two short names and nothing to choose between -- is simply cut.
    let host = match budget(shell.bishopt_int("prompt_host_budget"), cols) {
        Some(columns) => ellipsize_end(&host, columns),
        None => host,
    };
    let path = match budget(shell.bishopt_int("prompt_cwd_budget"), cols) {
        Some(columns) => fit_path(&path, columns),
        None => path,
    };
    let git = git_segment(&shell.cwd, budget(shell.bishopt_int("prompt_git_budget"), cols), &shell.bishopt_str("trust_owned"));
    let uh_color = if is_root { ROOT_USER_HOST_COLOR } else { USER_HOST_COLOR };
    let path_color = if is_root { ROOT_PATH_COLOR } else { PATH_COLOR };
    format!("{uh_color}{host}{RESET}:{path_color}{path}{RESET}{git}")
}

/// The prompt, fitted to a line `cols` wide.
///
/// The width is the caller's because the prompt is drawn into whatever
/// it is drawn into -- a pane is not the terminal, and the same prompt
/// has to give up more of itself in a narrow split than in a full
/// window.
pub fn render(shell: &Shell, cols: usize) -> String {
    let is_root = crate::platform::effective_user() == 0;
    let glyph_color = if shell.last_status == 0 { OK_COLOR } else { ERR_COLOR };
    let glyph = if is_root { "#" } else { "$" };
    format!("{}{glyph_color}{glyph}{RESET} ", prefix(shell, is_root, cols))
}

// Command mode's own prompt (repl.rs's run_command_mode): deliberately
// *not* a variant of render()'s "user@host:path$ " -- showing that full
// prefix here read as if you were at the ordinary shell prompt able to
// type any command, when command mode is actually a restricted, builtins-
// only line (see restrict_to_builtins in exec.rs). A bare colon, matching
// vim's own ':' Ex command line, doesn't carry that false suggestion.
pub fn command_mode_prompt() -> String {
    format!("{CMD_MODE_COLOR}:{RESET} ")
}

// Continuation-line prompt for an unfinished multi-line construct (open
// if/for/while/quote/paren) -- kept plain and dim rather than repeating
// the full cwd prompt.
pub fn continuation() -> String {
    "\x1b[2m…\x1b[0m ".to_string()
}

// The path as a prompt shows it: `$HOME` as `~`, and nothing else
// changed. Shortening it is `fit_path`'s job, and happens only as far as
// the width actually requires -- see that function.
//
// pub: repl.rs's tab bar and `window ls` both reuse this, so a window's
// path reads exactly like the prompt's own.
// The result is display text and only display text, which is why the
// control-character guard belongs here rather than on the caller: a
// directory called `sub<ESC>[2Jdir` would otherwise clear the terminal
// on every prompt redraw -- that is, on every keystroke. The prompt is
// built as a raw SGR string rather than as cells, so it does not pass
// through `render_linked`'s own gate.
pub fn display_path(cwd: &str, home: &str) -> String {
    crate::term::safe_text(&display_path_raw(cwd, home))
}

/// One parent directory, down to what it starts with -- and an ellipsis
/// saying so.
///
/// The ellipsis is the whole point. `~/w/c/billing` reads as a tree with
/// directories called `w` and `c` in it, which is a claim about the
/// filesystem rather than about what was left out; `~/w…/c…/billing`
/// says what happened. A name that is already one character is shown as
/// itself, because nothing was cut from it.
///
/// Hidden directories keep their dot: `.config` is `.c…`, since the dot
/// is what distinguishes it from `config` next to it.
fn abbreviate_parent(name: &str) -> String {
    let keep = match name.starts_with('.') {
        true => 2,
        false => 1,
    };
    let mut out: String = name.chars().take(keep).collect();
    if name.chars().count() > keep {
        out.push('\u{2026}');
    }
    out
}

fn display_path_raw(cwd: &str, home: &str) -> String {
    let (base, rest) = if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) {
        ("~".to_string(), cwd[home.len()..].trim_start_matches('/').to_string())
    } else {
        ("/".to_string(), cwd.trim_start_matches('/').to_string())
    };
    if rest.is_empty() {
        return base;
    }
    match base.ends_with('/') {
        true => format!("{base}{rest}"),
        false => format!("{base}/{rest}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The path gives up as little as it can, in this order: whole
    // directories first, then abbreviating the ones that survive, and
    // only last the name of the directory you are actually in.
    #[test]
    fn a_path_leaves_out_middle_directories_before_cutting_a_name() {
        let deep = display_path("/home/jussi/work/clients/acme/backend/services/billing", "/home/jussi");
        assert_eq!(deep, "~/work/clients/acme/backend/services/billing");

        // Room for everything: untouched, every name in full.
        assert_eq!(fit_path(&deep, 60), deep);

        // Tighter, and whole directories go -- from the top of the tree
        // down, so the immediate parents survive longest. The `…` segment
        // is the directories left out entirely.
        assert_eq!(fit_path(&deep, 40), "~/…/acme/backend/services/billing");
        assert_eq!(fit_path(&deep, 30), "~/…/backend/services/billing");
        assert_eq!(fit_path(&deep, 20), "~/…/services/billing");

        // Then, and only then, the survivors are abbreviated -- which
        // keeps more levels than dropping them would: `b…/s…/billing`
        // says there were two where `billing` alone says there were none.
        assert_eq!(fit_path(&deep, 19), "~/…/b…/s…/billing");
        assert_eq!(fit_path(&deep, 15), "~/…/s…/billing");
        assert_eq!(fit_path(&deep, 12), "~/…/billing");

        // Only once the frame itself is all that fits does the final
        // directory's own name get cut.
        assert_eq!(fit_path(&deep, 10), "~/…/billi…");
        assert_eq!(fit_path(&deep, 6), "~/…/b…");

        // Narrower than the frame: no structure left to keep, so this
        // says "there was more" rather than lying about the shape.
        assert_eq!(fit_path(&deep, 3), "~/…");

        // A path with nothing between the root and the name has nothing
        // to leave out.
        assert_eq!(fit_path("~/averylongdirectoryname", 10), "~/averylo…");
    }

    // The branch is the last segment; what it is filed under is the rest.
    #[test]
    fn a_branch_keeps_its_last_segment_and_shortens_the_namespace_evenly() {
        let branch = "feature/auth/oauth-callback-fix";
        assert_eq!(fit_branch(branch, 40), branch);

        // The last segment is served first, so it is whole while there is
        // room for it -- and the namespace shares what is left evenly
        // rather than the first segment taking it all. Every segment that
        // lost characters says so.
        // `auth` fits whole in the four columns its share came to, so it
        // carries no mark -- the mark appears exactly where something was
        // removed, which is the whole point of it.
        assert_eq!(fit_branch(branch, 28), "fea…/auth/oauth-callback-fix");
        assert_eq!(fit_branch(branch, 26), "fe…/au…/oauth-callback-fix");

        // Then the branch itself starts giving way, and the namespace
        // still keeps a character and its mark each: `f…/a…/` says there
        // were two levels above this, and that both were cut.
        assert_eq!(fit_branch(branch, 20), "f…/a…/oauth-callbac…");
        assert_eq!(fit_branch(branch, 12), "f…/a…/oauth…");

        // Under two columns for a name that has to be cut there is no
        // honest shape left, so the whole thing is cut as one.
        assert_eq!(fit_branch(branch, 7), "featur…");

        // Evenness is about the segments that still want columns: a short
        // one stops taking and the rest get its share. `a` is whole at one
        // column, so it carries no mark.
        assert_eq!(fit_branch("a/muchlonger/tip", 14), "a/muchlon…/tip");
    }

    // The contract both fitters owe, at every width: never wider than the
    // budget, and never empty. Checked across the range rather than at
    // the handful of widths the cases above name, because an off-by-one
    // in a fitter is exactly the kind of thing a chosen example misses.
    #[test]
    fn neither_fitter_ever_exceeds_its_budget() {
        let paths = [
            display_path("/home/jussi/work/clients/acme/backend/services/billing", "/home/jussi"),
            display_path("/home/jussi", "/home/jussi"),
            display_path("/usr/local/share", ""),
            display_path("/a/b/c", ""),
            display_path("/home/jussi/.config/bish", "/home/jussi"),
        ];
        let branches = ["main", "feature/auth/oauth-callback-fix", "a/muchlonger/tip", "release/2026/10/08/hotfix"];
        for budget in 1..=48 {
            for path in &paths {
                let fitted = fit_path(path, budget);
                assert!(width(&fitted) <= budget, "{path:?} at {budget}: {fitted:?} is {} wide", width(&fitted));
                assert!(!fitted.is_empty(), "{path:?} at {budget} came back empty");
            }
            for branch in branches {
                let fitted = fit_branch(branch, budget);
                assert!(width(&fitted) <= budget, "{branch:?} at {budget}: {fitted:?} is {} wide", width(&fitted));
                assert!(!fitted.is_empty(), "{branch:?} at {budget} came back empty");
            }
        }
    }

    // A budget is a share of the line, so the same prompt gives up more
    // of itself in a narrow pane than in a wide one -- and `0` is off.
    #[test]
    fn a_budget_is_a_percentage_of_the_line_and_zero_is_no_limit() {
        assert_eq!(budget(0, 200), None);
        assert_eq!(budget(25, 80), Some(20));
        assert_eq!(budget(40, 80), Some(32));
        // Never zero columns: a component that is there is worth a column.
        assert_eq!(budget(1, 10), Some(1));
    }

    #[test]
    fn a_display_path_is_the_path_with_home_as_a_tilde() {
        // Nothing is abbreviated here: shortening is `fit_path`'s, and
        // only as far as a width requires.
        assert_eq!(display_path("/home/jussi/bish/src", "/home/jussi"), "~/bish/src");
        assert_eq!(display_path("/home/jussi", "/home/jussi"), "~");
        assert_eq!(display_path("/usr/local/share", ""), "/usr/local/share");
        assert_eq!(display_path("/home/jussi/.config/bish", "/home/jussi"), "~/.config/bish");
        assert_eq!(display_path("/a/b/c", ""), "/a/b/c");
    }

    // The prompt is built as a raw SGR string, not as cells, so nothing
    // downstream is going to catch this: a directory called
    // `sub<ESC>[2Jdir` cleared the terminal on every prompt redraw --
    // that is, on every keystroke -- until this guard.
    #[test]
    fn a_directory_cannot_name_itself_a_terminal_command() {
        let out = display_path("/tmp/sub\x1b[2Jdir", "");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert_eq!(out, "/tmp/sub\u{FFFD}[2Jdir");
    }
}
