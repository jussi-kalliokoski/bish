// The default interactive prompt: "user@host:path (branch)$ ", in the
// style of PS1 `\u@\h:\w\$ `; `#` for root. The path is shown whole
// until the width says otherwise, then whole directories are left out,
// and only then are the survivors abbreviated to their first letter and
// an ellipsis (`~/…/D…/bish`), since a bare `D` would read as a real
// directory. The `(branch)` segment (`git::head_status`) appears only in
// a git repository, green when clean, yellow with `*` when not. Command
// mode has its own minimal prompt (`command_mode_prompt`).

use crate::bishedit::unicode_width::char_width;
use crate::exec::{self, Shell};

const RESET: &str = "\x1b[0m";
const USER_HOST_COLOR: &str = "\x1b[1;32m"; // bold green
const ROOT_USER_HOST_COLOR: &str = "\x1b[1;31m"; // bold red, a deliberate warning color
const PATH_COLOR: &str = "\x1b[1;36m"; // bold cyan
const ROOT_PATH_COLOR: &str = "\x1b[1;31m"; // bold red
const OK_COLOR: &str = "\x1b[1;32m"; // bold green
const ERR_COLOR: &str = "\x1b[1;31m"; // bold red
// Unlike both terminator colours, so command mode reads as another mode,
// not a recoloured prompt.
const CMD_MODE_COLOR: &str = "\x1b[1;35m"; // bold magenta
const GIT_CLEAN_COLOR: &str = "\x1b[0;32m"; // plain green -- deliberately dimmer than the bold user@host/path segments, secondary info
const GIT_DIRTY_COLOR: &str = "\x1b[0;33m"; // plain yellow

/// What a component may take of the line, in columns, from its
/// percentage bishopt: the share of this terminal matters, so a column
/// count would be too tight in a pane and too loose in a window. The
/// bounds are independent maxima, not a partition. `0` is no limit, as
/// for `wrap_column`.
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

/// `text` cut to `budget` columns, with an ellipsis where cut. The
/// ellipsis takes a column, so a budget of 1 is the ellipsis alone.
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

/// The path, fitted to `budget`, losing the cheapest thing first:
///
/// 1. nothing, when it fits;
/// 2. directories left out, keeping as many of the nearest as fit in
///    full, with an ellipsis for those gone: the nearest parents say more
///    than the top of the tree;
/// 3. when not one full name fits, the survivors abbreviated to their
///    first letter: `b…/s…/billing` shows two levels where leaving them
///    out shows none;
/// 4. the final directory's name cut, last of all.
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

    // Full names first, as many as fit; abbreviating only when none fits.
    // `…/services/billing` says where you are; `…/c…/a…/b…/s…/billing` only
    // how deep.
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
    // Strictly less: the final name needs a column, even if only for the
    // ellipsis.
    match width(&frame) < budget {
        true => format!("{frame}{}", ellipsize_end(last, budget - width(&frame))),
        // Narrower than the frame: plain truncation, rather than a shape
        // claiming parts it can't show.
        false => ellipsize_end(display, budget),
    }
}

/// A branch name fitted by shortening its `/` segments. The last is the
/// branch, served first; the rest, its namespace, share what's left
/// evenly, each keeping at least a character. A cut segment always shows
/// an ellipsis, the last included, since a silently cut segment claims a
/// name it isn't; so a cut needs two columns, hence the floor of two.
pub(crate) fn fit_branch(branch: &str, budget: usize) -> String {
    if width(branch) <= budget {
        return branch.to_string();
    }
    let parts: Vec<&str> = branch.split('/').collect();
    if parts.len() < 2 {
        return ellipsize_end(branch, budget);
    }
    let separators = parts.len() - 1;
    // The floor per segment: one column if the name fits in one, otherwise
    // two, a character and the ellipsis.
    let floor: Vec<usize> = parts.iter().map(|p| width(p).min(2)).collect();
    let floors: usize = floor.iter().sum();
    if separators + floors > budget {
        return ellipsize_end(branch, budget);
    }
    let last = parts.len() - 1;
    let mut share = floor.clone();
    let mut spare = budget - separators - floors;

    // The branch first, up to its length.
    let wanted = width(parts[last]).saturating_sub(share[last]).min(spare);
    share[last] += wanted;
    spare -= wanted;

    // Then the namespace, a column at a time among the segments still
    // wanting one, so the share is even.
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
        // Every segment, the namespace too, shows when it was cut.
        out.push_str(&ellipsize_end(part, share[i]));
    }
    out
}

fn username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

// `git status`'s branch and dirty segment, or empty outside a repository
// or without git (`git::head_status`).
fn git_segment(cwd: &std::path::Path, budget: Option<usize>, owned_default: &str) -> String {
    let fitted = |branch: &str, taken: usize| match budget {
        // The parentheses and dirty marker count against the budget; the name
        // gets the rest.
        Some(columns) => fit_branch(branch, columns.saturating_sub(taken)),
        None => branch.to_string(),
    };
    // The `git` capability: in an untrusted directory bish reads the branch
    // from `.git/HEAD` and runs no git in the worktree (`git::head_status`),
    // so a repository's config can't make a prompt draw run code.
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
    // Each component against its own bound, losing the least of what it's
    // for: a path drops directories, a branch shortens its namespace, and
    // `user@host`, two short names, is simply cut.
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

/// The prompt fitted to a line `cols` wide: a pane isn't the terminal,
/// and a narrow split takes more off the prompt.
pub fn render(shell: &Shell, cols: usize) -> String {
    let is_root = crate::platform::effective_user() == 0;
    let glyph_color = if shell.last_status == 0 { OK_COLOR } else { ERR_COLOR };
    let glyph = if is_root { "#" } else { "$" };
    format!("{}{glyph_color}{glyph}{RESET} ", prefix(shell, is_root, cols))
}

// Command mode's prompt: a bare colon, like vim's, not a variant of the
// "user@host:path$ " that would suggest any command can be typed, when
// command mode runs only builtins (`restrict_to_builtins`).
pub fn command_mode_prompt() -> String {
    format!("{CMD_MODE_COLOR}:{RESET} ")
}

// The continuation prompt for an unfinished construct (an open `if`,
// loop, quote or paren), plain and dim.
pub fn continuation() -> String {
    "\x1b[2m…\x1b[0m ".to_string()
}

// The path as a prompt shows it: `$HOME` as `~`, nothing else
// (shortening is `fit_path`'s). Also the tab bar's and `window ls`'s, so
// they match. Display text only, so the control-character guard is
// here: the prompt is a raw SGR string, not cells, and a directory named
// `sub<ESC>[2Jdir` would clear the screen on every keystroke.
pub fn display_path(cwd: &str, home: &str) -> String {
    crate::term::safe_text(&display_path_raw(cwd, home))
}

/// One parent directory cut to its first letter and an ellipsis.
/// `~/w/c/billing` would claim directories named `w` and `c`;
/// `~/w…/c…/billing` says what was left out. A one-character name is shown
/// as is. A hidden directory keeps its dot (`.config` is `.c…`).
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

    #[test]
    fn a_path_leaves_out_middle_directories_before_cutting_a_name() {
        let deep = display_path("/home/jussi/work/clients/acme/backend/services/billing", "/home/jussi");
        assert_eq!(deep, "~/work/clients/acme/backend/services/billing");

        // Room for everything: untouched.
        assert_eq!(fit_path(&deep, 60), deep);

        // Tighter: whole directories go from the top down, the nearest
        // parents last; `…` stands for those left out.
        assert_eq!(fit_path(&deep, 40), "~/…/acme/backend/services/billing");
        assert_eq!(fit_path(&deep, 30), "~/…/backend/services/billing");
        assert_eq!(fit_path(&deep, 20), "~/…/services/billing");

        // Then the survivors are abbreviated, keeping more levels than
        // dropping them would.
        assert_eq!(fit_path(&deep, 19), "~/…/b…/s…/billing");
        assert_eq!(fit_path(&deep, 15), "~/…/s…/billing");
        assert_eq!(fit_path(&deep, 12), "~/…/billing");

        // Only when the frame is all that fits is the final name cut.
        assert_eq!(fit_path(&deep, 10), "~/…/billi…");
        assert_eq!(fit_path(&deep, 6), "~/…/b…");

        // Narrower than the frame: "there was more" rather than a false
        // shape.
        assert_eq!(fit_path(&deep, 3), "~/…");

        // Nothing between the root and the name to leave out.
        assert_eq!(fit_path("~/averylongdirectoryname", 10), "~/averylo…");
    }

    // The branch is the last segment; its namespace the rest.
    #[test]
    fn a_branch_keeps_its_last_segment_and_shortens_the_namespace_evenly() {
        let branch = "feature/auth/oauth-callback-fix";
        assert_eq!(fit_branch(branch, 40), branch);

        // The last segment first, whole while there's room, the namespace
        // sharing the rest evenly, with a mark on every cut segment. `auth`
        // fits its four columns, so it has no mark.
        assert_eq!(fit_branch(branch, 28), "fea…/auth/oauth-callback-fix");
        assert_eq!(fit_branch(branch, 26), "fe…/au…/oauth-callback-fix");

        // Then the branch gives way, and the namespace keeps a character
        // and a mark each.
        assert_eq!(fit_branch(branch, 20), "f…/a…/oauth-callbac…");
        assert_eq!(fit_branch(branch, 12), "f…/a…/oauth…");

        // Under two columns for a name that must be cut, the whole is cut as
        // one.
        assert_eq!(fit_branch(branch, 7), "featur…");

        // Evenness is among segments still wanting columns: a short one stops
        // and the rest share its part. `a` fits one column whole.
        assert_eq!(fit_branch("a/muchlonger/tip", 14), "a/muchlon…/tip");
    }

    // Both fitters, at every width: never wider than the budget, never
    // empty. A range, since an off-by-one hides between chosen examples.
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

    // A budget is a share of the line; `0` is off.
    #[test]
    fn a_budget_is_a_percentage_of_the_line_and_zero_is_no_limit() {
        assert_eq!(budget(0, 200), None);
        assert_eq!(budget(25, 80), Some(20));
        assert_eq!(budget(40, 80), Some(32));
        // Never zero columns: a component that's there is worth a column.
        assert_eq!(budget(1, 10), Some(1));
    }

    #[test]
    fn a_display_path_is_the_path_with_home_as_a_tilde() {
        // No abbreviation here; that's `fit_path`'s.
        assert_eq!(display_path("/home/jussi/bish/src", "/home/jussi"), "~/bish/src");
        assert_eq!(display_path("/home/jussi", "/home/jussi"), "~");
        assert_eq!(display_path("/usr/local/share", ""), "/usr/local/share");
        assert_eq!(display_path("/home/jussi/.config/bish", "/home/jussi"), "~/.config/bish");
        assert_eq!(display_path("/a/b/c", ""), "/a/b/c");
    }

    // The prompt is a raw SGR string, so nothing downstream catches a
    // directory named `sub<ESC>[2Jdir`.
    #[test]
    fn a_directory_cannot_name_itself_a_terminal_command() {
        let out = display_path("/tmp/sub\x1b[2Jdir", "");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert_eq!(out, "/tmp/sub\u{FFFD}[2Jdir");
    }
}
