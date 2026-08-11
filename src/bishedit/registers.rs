// Vim's register model: named storage that yank/put (and, later, delete/
// change/macros) read and write. Two deliberate deviations from real vim:
//
// - Registers are pluggable (`RegisterBackend`), not just a HashMap<char,
//   String>. Today only three backends exist (plain in-memory, black-hole,
//   system-clipboard), but a future file-backed register is meant to be a
//   small, well-contained addition against the same trait, not a redesign.
// - The unnamed register ("") is aliased to "+ -- the same relationship
//   real vim has under `set clipboard=unnamedplus`. It isn't a second,
//   independently-tracked slot that happens to mirror "+; `None` (no `"x`
//   prefix), `Some('"')`, and `Some('+')` all resolve to the *same*
//   `ClipboardBackend` instance, so there's nothing to keep in sync.
//
// Numbered registers ("1-"9, populated by delete in real vim) don't exist
// here yet -- there's no delete operator to populate them with. Nothing in
// this module's shape rules this out later; it just isn't built now.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterShape {
    Char,
    Line,
}

#[derive(Debug, Clone)]
pub struct RegisterValue {
    pub text: String,
    pub shape: RegisterShape,
}

impl Default for RegisterValue {
    fn default() -> Self {
        RegisterValue { text: String::new(), shape: RegisterShape::Char }
    }
}

impl RegisterValue {
    // Collapses this value down to something safe to splice into a
    // genuinely single-line buffer (the shell's own command line -- see
    // editor.rs's LineEditor). A Line-shaped value already carries its own
    // trailing '\n' (motion.rs's extract_text/whole_lines both bake it in);
    // drop exactly that one, then flatten any *other* embedded newline
    // (e.g. a multi-line charwise yank pasted from elsewhere) to a space
    // rather than corrupting the single-line display with a literal
    // newline character.
    pub fn flatten_to_single_line(&self) -> String {
        let mut s = self.text.as_str();
        if let RegisterShape::Line = self.shape {
            s = s.strip_suffix('\n').unwrap_or(s);
        }
        s.replace('\n', " ")
    }
}

/// The extension seam for a register's storage. Every register a name can
/// resolve to (named letters, the clipboard-aliased unnamed/"+", the black
/// hole) implements this the same way; a future file-backed register would
/// too.
trait RegisterBackend {
    fn read(&self) -> RegisterValue;
    /// `append`: true for an uppercase register name (`"A`, ...) -- vim's
    /// own rule is text concatenation, and the merged shape is `Line` if
    /// *either* side was `Line` (a linewise register stays "at least a
    /// full line" no matter what gets appended to it).
    fn write(&mut self, value: RegisterValue, append: bool);
}

#[derive(Default)]
struct InMemoryBackend(RegisterValue);

impl RegisterBackend for InMemoryBackend {
    fn read(&self) -> RegisterValue {
        self.0.clone()
    }
    fn write(&mut self, value: RegisterValue, append: bool) {
        if append && !self.0.text.is_empty() {
            let shape = if self.0.shape == RegisterShape::Line || value.shape == RegisterShape::Line {
                RegisterShape::Line
            } else {
                RegisterShape::Char
            };
            self.0.text.push_str(&value.text);
            self.0.shape = shape;
        } else {
            self.0 = value;
        }
    }
}

struct BlackHoleBackend;

impl RegisterBackend for BlackHoleBackend {
    fn read(&self) -> RegisterValue {
        RegisterValue::default()
    }
    fn write(&mut self, _value: RegisterValue, _append: bool) {}
}

// A copy/paste command pair for one clipboard tool. `paste_extra_args`
// exists solely for `wl-paste -n` (see `detect_clipboard_tool`'s own doc
// comment on why it's needed there and nowhere else).
struct ClipboardTool {
    copy: (&'static str, &'static [&'static str]),
    paste: (&'static str, &'static [&'static str]),
}

// Tries each candidate's *both* binaries (a tool that can copy but not
// paste, or vice versa, is useless here) via a plain PATH scan -- no `which`
// subprocess, no probe-spawn of the real tool (which would mean actually
// touching stdin/stdout and risking a hang or a stray clipboard write
// before this shell has even started). Order doesn't hardcode a single
// target OS: probing whichever binaries actually exist on PATH is robust to
// e.g. wl-clipboard under WSLg or xclip installed via XQuartz on macOS,
// which a `cfg(target_os)` gate would miss.
fn detect_clipboard_tool() -> Option<ClipboardTool> {
    let candidates: [ClipboardTool; 4] = [
        ClipboardTool { copy: ("pbcopy", &[]), paste: ("pbpaste", &[]) },
        ClipboardTool { copy: ("xclip", &["-selection", "clipboard"]), paste: ("xclip", &["-selection", "clipboard", "-o"]) },
        ClipboardTool { copy: ("xsel", &["--clipboard", "--input"]), paste: ("xsel", &["--clipboard", "--output"]) },
        // wl-paste appends a trailing newline by default even when the
        // clipboard content didn't have one -- `-n` suppresses that so
        // ClipboardBackend::read's own Char-vs-Line heuristic (does the
        // text end in '\n'?) reflects the clipboard's real content instead
        // of an artifact of the tool itself.
        ClipboardTool { copy: ("wl-copy", &[]), paste: ("wl-paste", &["-n"]) },
    ];
    candidates.into_iter().find(|c| command_exists(c.copy.0) && command_exists(c.paste.0))
}

fn command_exists(name: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(name)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

// Backs the unnamed/"+ register. `tool: None` (no supported clipboard
// utility found on PATH) degrades silently to `fallback`, an ordinary
// in-memory slot -- the register still works, it just isn't backed by the
// real system clipboard. No error, no warning: a shell without a clipboard
// tool installed is an entirely normal environment (this project's own dev
// container is one), not a misconfiguration worth surfacing.
struct ClipboardBackend {
    tool: Option<ClipboardTool>,
    fallback: RegisterValue,
}

impl ClipboardBackend {
    fn new() -> Self {
        ClipboardBackend { tool: detect_clipboard_tool(), fallback: RegisterValue::default() }
    }
}

impl RegisterBackend for ClipboardBackend {
    fn read(&self) -> RegisterValue {
        let Some(tool) = &self.tool else {
            return self.fallback.clone();
        };
        let (cmd, args) = tool.paste;
        let output = Command::new(cmd).args(args).stdin(Stdio::null()).output();
        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                let shape = if text.ends_with('\n') { RegisterShape::Line } else { RegisterShape::Char };
                RegisterValue { text, shape }
            }
            // Tool vanished from PATH, or errored (e.g. no display server
            // to talk to) -- same graceful degrade as never having found
            // it, rather than losing whatever was last yanked.
            _ => self.fallback.clone(),
        }
    }

    fn write(&mut self, value: RegisterValue, append: bool) {
        let merged = if append && !self.fallback.text.is_empty() {
            // The clipboard itself has no append primitive -- read it back
            // first (so "A appending onto a real system clipboard sees
            // whatever's actually there, not just this backend's own
            // possibly-stale fallback) and merge the same way InMemoryBackend
            // does.
            let mut current = self.read();
            let shape = if current.shape == RegisterShape::Line || value.shape == RegisterShape::Line {
                RegisterShape::Line
            } else {
                RegisterShape::Char
            };
            current.text.push_str(&value.text);
            current.shape = shape;
            current
        } else {
            value
        };
        self.fallback = merged.clone();
        if let Some(tool) = &self.tool {
            let (cmd, args) = tool.copy;
            if let Ok(mut child) = Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
                if let Some(stdin) = child.stdin.take() {
                    let mut stdin = stdin;
                    let _ = stdin.write_all(merged.text.as_bytes());
                }
                let _ = child.wait();
            }
        }
    }
}

/// The whole-shell register table -- one instance, shared globally across
/// every window/pane/session (matching both vim, where registers are
/// global to the editor instance rather than per-buffer, and tmux, where
/// paste buffers are global to the server rather than per-pane).
pub struct Registers {
    named: HashMap<char, InMemoryBackend>,
    unnamed: ClipboardBackend,
    black_hole: BlackHoleBackend,
}

impl Registers {
    pub fn new() -> Self {
        Registers { named: HashMap::new(), unnamed: ClipboardBackend::new(), black_hole: BlackHoleBackend }
    }

    /// `name` is exactly what a `"x` prefix typed, case and all (`None` for
    /// no prefix at all). Uppercase `'A'..='Z'` always resolves to the same
    /// storage as its lowercase form -- the case only ever matters to
    /// `write`'s `append` behavior, decided by the caller before calling
    /// this (see `resolve`'s own doc comment).
    fn resolve(&mut self, name: Option<char>) -> (&mut dyn RegisterBackend, bool) {
        match name {
            None | Some('"') | Some('+') => (&mut self.unnamed, false),
            Some('_') => (&mut self.black_hole, false),
            Some(c) if c.is_ascii_uppercase() => {
                (self.named.entry(c.to_ascii_lowercase()).or_default(), true)
            }
            Some(c) if c.is_ascii_lowercase() => (self.named.entry(c).or_default(), false),
            // An unrecognized register name (shouldn't reach here --
            // vimkeys.rs only ever admits a-z/A-Z/+/"/_ into
            // `pending_register` in the first place) falls back to the
            // unnamed register rather than panicking.
            Some(_) => (&mut self.unnamed, false),
        }
    }

    pub fn read(&mut self, name: Option<char>) -> RegisterValue {
        self.resolve(name).0.read()
    }

    pub fn write(&mut self, name: Option<char>, value: RegisterValue) {
        let (backend, append) = self.resolve(name);
        backend.write(value, append);
    }
}

impl Default for Registers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Registers {
    // Forces the unnamed/"+ register to its in-memory fallback rather than
    // whatever `detect_clipboard_tool` finds on the host running the test
    // suite -- otherwise these tests would (a) actually read/write the
    // real system clipboard as a side effect of `cargo test`, clobbering
    // whatever the developer had copied, and (b) collide with each other,
    // since unlike every other backend the real clipboard is genuinely
    // global to the OS, not scoped to one `Registers` instance.
    fn new_for_test() -> Self {
        Registers {
            named: HashMap::new(),
            unnamed: ClipboardBackend { tool: None, fallback: RegisterValue::default() },
            black_hole: BlackHoleBackend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn char_val(s: &str) -> RegisterValue {
        RegisterValue { text: s.to_string(), shape: RegisterShape::Char }
    }
    fn line_val(s: &str) -> RegisterValue {
        RegisterValue { text: s.to_string(), shape: RegisterShape::Line }
    }

    #[test]
    fn named_register_round_trips() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('a'), char_val("hello"));
        let v = regs.read(Some('a'));
        assert_eq!(v.text, "hello");
        assert_eq!(v.shape, RegisterShape::Char);
    }

    #[test]
    fn uppercase_appends_to_the_same_lowercase_register() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('a'), char_val("foo"));
        regs.write(Some('A'), char_val("bar"));
        let v = regs.read(Some('a'));
        assert_eq!(v.text, "foobar");
        assert_eq!(v.shape, RegisterShape::Char);
        // reading via the uppercase name hits the same storage
        assert_eq!(regs.read(Some('A')).text, "foobar");
    }

    #[test]
    fn append_promotes_shape_to_line_if_either_side_is_line() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('a'), char_val("foo"));
        regs.write(Some('A'), line_val("bar\n"));
        assert_eq!(regs.read(Some('a')).shape, RegisterShape::Line);

        let mut regs2 = Registers::new_for_test();
        regs2.write(Some('b'), line_val("foo\n"));
        regs2.write(Some('B'), char_val("bar"));
        assert_eq!(regs2.read(Some('b')).shape, RegisterShape::Line);
    }

    #[test]
    fn append_onto_empty_register_is_a_plain_write() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('A'), char_val("first"));
        assert_eq!(regs.read(Some('a')).text, "first");
    }

    #[test]
    fn unnamed_double_quote_and_plus_share_the_same_storage() {
        let mut regs = Registers::new_for_test();
        regs.write(None, char_val("no-prefix"));
        assert_eq!(regs.read(Some('"')).text, "no-prefix");
        assert_eq!(regs.read(Some('+')).text, "no-prefix");

        regs.write(Some('"'), char_val("via-quote"));
        assert_eq!(regs.read(None).text, "via-quote");
        assert_eq!(regs.read(Some('+')).text, "via-quote");

        regs.write(Some('+'), char_val("via-plus"));
        assert_eq!(regs.read(None).text, "via-plus");
        assert_eq!(regs.read(Some('"')).text, "via-plus");
    }

    #[test]
    fn black_hole_never_stores_anything() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('_'), char_val("gone"));
        let v = regs.read(Some('_'));
        assert_eq!(v.text, "");
        // doesn't leak into the unnamed register either
        assert_eq!(regs.read(None).text, "");
    }

    #[test]
    fn distinct_named_registers_stay_independent() {
        let mut regs = Registers::new_for_test();
        regs.write(Some('a'), char_val("a-value"));
        regs.write(Some('b'), char_val("b-value"));
        assert_eq!(regs.read(Some('a')).text, "a-value");
        assert_eq!(regs.read(Some('b')).text, "b-value");
    }

    #[test]
    fn reading_an_unwritten_named_register_is_empty() {
        let mut regs = Registers::new_for_test();
        let v = regs.read(Some('z'));
        assert_eq!(v.text, "");
        assert_eq!(v.shape, RegisterShape::Char);
    }

    #[test]
    fn flatten_char_shape_replaces_all_embedded_newlines() {
        let v = char_val("foo\nbar\nbaz");
        assert_eq!(v.flatten_to_single_line(), "foo bar baz");
    }

    #[test]
    fn flatten_line_shape_drops_one_trailing_newline_then_flattens_rest() {
        let v = line_val("foo\nbar\n");
        assert_eq!(v.flatten_to_single_line(), "foo bar");
    }

    #[test]
    fn flatten_line_shape_without_a_trailing_newline_still_flattens() {
        let v = line_val("foo\nbar");
        assert_eq!(v.flatten_to_single_line(), "foo bar");
    }

    #[test]
    fn command_exists_finds_a_real_binary_and_rejects_a_fake_one() {
        assert!(command_exists("ls"));
        assert!(!command_exists("this-command-does-not-exist-anywhere-xyz"));
    }
}
