// Default interactive prompt: "user@host:path_abbr<terminator> " (no
// space before the terminator, matching classic `\u@\h:\w\$ ` PS1
// style), where path_abbr abbreviates parent path components to their
// first character, spelling out only the final one (e.g. "~/D/P/bish"),
// and the terminator glyph is "$" for a normal user, "#" for root, or
// ":" while editor.rs's read_line has a virtual, not-yet-materialized
// command-mode colon armed (see render_command_armed and editor.rs's
// read_line for the reversible-entry mechanic this exists to support --
// plan.md's "Future improvements" note on making command-mode entry a
// virtual character instead of literal inserted text). No git-branch
// segment yet -- there's no git integration in bish at all so far.

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

fn username() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "user".to_string())
}

// Everything before the terminator glyph -- shared by render and
// render_command_armed so the two variants editor.rs alternates between
// while a colon is armed never visually diverge except in that one
// glyph.
fn prefix(shell: &Shell, is_root: bool) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = shorten_path(&shell.cwd.to_string_lossy(), &home);
    let uh_color = if is_root { ROOT_USER_HOST_COLOR } else { USER_HOST_COLOR };
    let path_color = if is_root { ROOT_PATH_COLOR } else { PATH_COLOR };
    format!("{uh_color}{}@{}{RESET}:{path_color}{display}{RESET}", username(), exec::get_hostname())
}

pub fn render(shell: &Shell) -> String {
    let is_root = unsafe { geteuid() } == 0;
    let glyph_color = if shell.last_status == 0 { OK_COLOR } else { ERR_COLOR };
    let glyph = if is_root { "#" } else { "$" };
    format!("{}{glyph_color}{glyph}{RESET} ", prefix(shell, is_root))
}

// Shown in place of render()'s output while a virtual command-mode colon
// is armed (see editor.rs's read_line): same prefix, only the terminator
// glyph and its color change, so entering command mode reads as "the
// prompt itself changed," not as if a character had been typed.
pub fn render_command_armed(shell: &Shell) -> String {
    let is_root = unsafe { geteuid() } == 0;
    format!("{}{CMD_MODE_COLOR}:{RESET} ", prefix(shell, is_root))
}

// Continuation-line prompt for an unfinished multi-line construct (open
// if/for/while/quote/paren) -- kept plain and dim rather than repeating
// the full cwd prompt.
pub fn continuation() -> String {
    "\x1b[2m…\x1b[0m ".to_string()
}

// Armed variant of continuation(), for the same reason render_command_
// armed exists alongside render().
pub fn continuation_armed() -> String {
    format!("{CMD_MODE_COLOR}:{RESET} ")
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
