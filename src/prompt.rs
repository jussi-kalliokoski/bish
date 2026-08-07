// Default interactive prompt, styled after fish's: the cwd with parent
// components abbreviated to their first character and the final component
// spelled out in full (e.g. "~/D/P/bish"), colored, with the prompt glyph
// reflecting the last command's exit status. No git-branch segment yet --
// there's no git integration in bish at all so far.

use crate::exec::Shell;

unsafe extern "C" {
    fn geteuid() -> u32;
}

const RESET: &str = "\x1b[0m";
const PATH_COLOR: &str = "\x1b[1;36m"; // bold cyan
const ROOT_PATH_COLOR: &str = "\x1b[1;31m"; // bold red, a deliberate warning color
const OK_COLOR: &str = "\x1b[1;32m"; // bold green
const ERR_COLOR: &str = "\x1b[1;31m"; // bold red

pub fn render(shell: &Shell) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let display = shorten_path(&shell.cwd.to_string_lossy(), &home);

    let is_root = unsafe { geteuid() } == 0;
    let path_color = if is_root { ROOT_PATH_COLOR } else { PATH_COLOR };
    let glyph_color = if shell.last_status == 0 { OK_COLOR } else { ERR_COLOR };
    let glyph = if is_root { "#" } else { "\u{276f}" }; // ❯

    format!("{path_color}{display}{RESET} {glyph_color}{glyph}{RESET} ")
}

// Continuation-line prompt for an unfinished multi-line construct (open
// if/for/while/quote/paren) -- kept plain and dim rather than repeating
// the full cwd prompt.
pub fn continuation() -> String {
    "\x1b[2m…\x1b[0m ".to_string()
}

fn shorten_path(cwd: &str, home: &str) -> String {
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
