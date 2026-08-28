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

use crate::exec::{self, Shell};

unsafe extern "C" {
    fn geteuid() -> u32;
}

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

fn username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

// `git status`'s branch/dirty segment, or empty outside a repo (or with
// no `git` on $PATH -- `git::head_status`'s own doc comment covers why
// those two cases aren't told apart here).
fn git_segment(cwd: &std::path::Path) -> String {
    match crate::git::head_status(cwd) {
        Some(status) if status.dirty => format!(" {GIT_DIRTY_COLOR}({}*){RESET}", status.branch),
        Some(status) => format!(" {GIT_CLEAN_COLOR}({}){RESET}", status.branch),
        None => String::new(),
    }
}

fn prefix(shell: &Shell, is_root: bool) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = shorten_path(&shell.cwd.to_string_lossy(), &home);
    let uh_color = if is_root { ROOT_USER_HOST_COLOR } else { USER_HOST_COLOR };
    let path_color = if is_root { ROOT_PATH_COLOR } else { PATH_COLOR };
    format!("{uh_color}{}@{}{RESET}:{path_color}{display}{RESET}{}", username(), exec::get_hostname(), git_segment(&shell.cwd))
}

pub fn render(shell: &Shell) -> String {
    let is_root = unsafe { geteuid() } == 0;
    let glyph_color = if shell.last_status == 0 { OK_COLOR } else { ERR_COLOR };
    let glyph = if is_root { "#" } else { "$" };
    format!("{}{glyph_color}{glyph}{RESET} ", prefix(shell, is_root))
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
pub fn shorten_path(cwd: &str, home: &str) -> String {
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
