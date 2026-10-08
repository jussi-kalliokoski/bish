// Default interactive prompt: "user@host:path_abbr (branch)<terminator> "
// (no space before the terminator, matching classic `\u@\h:\w\$ ` PS1
// style), where path_abbr abbreviates parent path components to their
// first character, spelling out only the final one (e.g. "~/D/P/bish"),
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
fn ellipsize_end(text: &str, budget: usize) -> String {
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
/// Directories are dropped before any name is cut, and the ones nearest
/// the end are kept: the immediate parents of where you are say more
/// about it than the top of the tree does. An ellipsis stands in for
/// whatever was left out, so the path never reads as a real one it is
/// not. Only when the frame alone -- the root, the gap and the final
/// directory -- will not fit does the final name itself get cut, which
/// is the last thing anyone wants to lose.
///
/// Takes the already-abbreviated display path (see `shorten_path`),
/// whose parents are single characters, so this bites only on a tree
/// deep enough that even those do not fit.
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
    // As many of the middle directories as fit, counting from the end --
    // `keep` of 0 is the root, the gap and the final one.
    for keep in (0..middle.len()).rev() {
        let kept: String = middle[middle.len() - keep..].iter().map(|p| format!("{p}/")).collect();
        let candidate = format!("{base}/\u{2026}/{kept}{last}");
        if width(&candidate) <= budget {
            return candidate;
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
/// Only the final segment carries an ellipsis: the earlier ones are
/// abbreviated the way this prompt's own path abbreviates a parent
/// directory, which is to say silently, and the `/`s are what make that
/// legible.
pub(crate) fn fit_branch(branch: &str, budget: usize) -> String {
    if width(branch) <= budget {
        return branch.to_string();
    }
    let parts: Vec<&str> = branch.split('/').collect();
    if parts.len() < 2 {
        return ellipsize_end(branch, budget);
    }
    let separators = parts.len() - 1;
    // One column per segment is the floor, and under it this shape
    // cannot be drawn at all.
    if separators + parts.len() > budget {
        return ellipsize_end(branch, budget);
    }
    let last = parts.len() - 1;
    let mut share = vec![1usize; parts.len()];
    let mut spare = budget - separators - parts.len();

    // The branch itself first, up to its own length.
    let wanted = width(parts[last]).saturating_sub(1).min(spare);
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
        match i == last {
            // The branch keeps its marker: this is the name being cut.
            true => out.push_str(&ellipsize_end(part, share[i])),
            false => out.extend(part.chars().scan(0usize, |taken, c| {
                *taken += char_width(c);
                (*taken <= share[i]).then_some(c)
            })),
        }
    }
    out
}

fn username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

// `git status`'s branch/dirty segment, or empty outside a repo (or with
// no `git` on $PATH -- `git::head_status`'s own doc comment covers why
// those two cases aren't told apart here).
fn git_segment(cwd: &std::path::Path, budget: Option<usize>) -> String {
    let fitted = |branch: &str, taken: usize| match budget {
        // The parentheses and the dirty marker are part of the segment
        // the budget is about, so the name itself gets what is left of
        // it.
        Some(columns) => fit_branch(branch, columns.saturating_sub(taken)),
        None => branch.to_string(),
    };
    match crate::git::head_status(cwd) {
        Some(status) if status.dirty => format!(" {GIT_DIRTY_COLOR}({}*){RESET}", fitted(&status.branch, 3)),
        Some(status) => format!(" {GIT_CLEAN_COLOR}({}){RESET}", fitted(&status.branch, 2)),
        None => String::new(),
    }
}

fn prefix(shell: &Shell, is_root: bool, cols: usize) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = shorten_path(&shell.cwd.to_string_lossy(), &home);
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
    let git = git_segment(&shell.cwd, budget(shell.bishopt_int("prompt_git_budget"), cols));
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

// pub: repl.rs's tab bar reuses this directly (see its own tab_bar_
// snapshot) so a window's path there always reads exactly like the
// prompt's own -- same abbreviation, same "~" home substitution --
// rather than showing the full, unshortened path.
// The result is display text and only display text (it is abbreviated,
// so it never round-trips back into a real path), which is why the
// control-character guard belongs here rather than on the caller: a
// directory called `sub<ESC>[2Jdir` would otherwise clear the terminal
// on every prompt redraw -- that is, on every keystroke. The prompt is
// built as a raw SGR string rather than as cells, so it does not pass
// through `render_linked`'s own gate.
pub fn shorten_path(cwd: &str, home: &str) -> String {
    crate::term::safe_text(&shorten_path_raw(cwd, home))
}

fn shorten_path_raw(cwd: &str, home: &str) -> String {
    let (base, rest) = if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{home}/"))) {
        ("~".to_string(), cwd[home.len()..].trim_start_matches('/').to_string())
    } else {
        ("/".to_string(), cwd.trim_start_matches('/').to_string())
    };
    if rest.is_empty() {
        return base;
    }
    let mut out = base;
    let parts: Vec<&str> = rest.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if !out.ends_with('/') {
            out.push('/');
        }
        if i + 1 == parts.len() {
            out.push_str(part); // final component: full name
        } else if part.starts_with('.') && part.len() > 1 {
            // keep the leading dot visible for hidden dirs, e.g. ".config" -> ".c"
            out.push('.');
            out.push(part.chars().nth(1).unwrap());
        } else if let Some(c) = part.chars().next() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The path gives up whole directories before it gives up any name,
    // and the ones it keeps are the ones nearest where you are.
    #[test]
    fn a_path_leaves_out_middle_directories_before_cutting_a_name() {
        let deep = shorten_path("/home/jussi/work/clients/acme/backend/services/billing", "/home/jussi");
        assert_eq!(deep, "~/w/c/a/b/s/billing");

        // Room for everything: untouched.
        assert_eq!(fit_path(&deep, 40), deep);

        // Tighter, and the middle goes first -- from the top of the tree
        // down, so the immediate parents survive longest.
        assert_eq!(fit_path(&deep, 18), "~/…/a/b/s/billing");
        assert_eq!(fit_path(&deep, 16), "~/…/b/s/billing");
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
        // rather than the first segment taking it all.
        assert_eq!(fit_branch(branch, 26), "fea/aut/oauth-callback-fix");
        assert_eq!(fit_branch(branch, 22), "f/a/oauth-callback-fix");

        // Then the branch itself starts giving way, and the namespace
        // still keeps a character each: `f/a/` says there were two levels
        // above this where dropping them would say there were none.
        assert_eq!(fit_branch(branch, 16), "f/a/oauth-callb…");
        assert_eq!(fit_branch(branch, 8), "f/a/oau…");

        // Under one column per segment there is no such shape to draw.
        assert_eq!(fit_branch(branch, 4), "fea…");

        // Evenness is about the segments that still want columns: a short
        // one stops taking and the rest get its share.
        assert_eq!(fit_branch("a/muchlonger/tip", 14), "a/muchlong/tip");
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
    fn shorten_path_abbreviates_all_but_the_last_component() {
        assert_eq!(shorten_path("/home/jussi/bish/src", "/home/jussi"), "~/b/src");
        assert_eq!(shorten_path("/home/jussi", "/home/jussi"), "~");
        assert_eq!(shorten_path("/usr/local/share", ""), "/u/l/share");
        assert_eq!(shorten_path("/home/jussi/.config/bish", "/home/jussi"), "~/.c/bish");
    }

    // The prompt is built as a raw SGR string, not as cells, so nothing
    // downstream is going to catch this: a directory called
    // `sub<ESC>[2Jdir` cleared the terminal on every prompt redraw --
    // that is, on every keystroke -- until this guard.
    #[test]
    fn a_directory_cannot_name_itself_a_terminal_command() {
        let out = shorten_path("/tmp/sub\x1b[2Jdir", "");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert_eq!(out, "/t/sub\u{FFFD}[2Jdir");
    }
}
