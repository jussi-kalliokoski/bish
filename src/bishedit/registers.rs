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
// Numbered registers: "0" holds the most recent yank, "1"-"9" a ring of
// the most recent deletes/changes (newest at "1", each older one shifted
// up, "9" falling off the end) -- populated via `record_yank`/
// `record_delete` rather than plain `write`, and only when no explicit
// `"x` prefix was given (matching vim: `"ayw` only ever touches "a, never
// "0). Simplified relative to real vim in one way: every delete/change
// shifts the ring here, not just ones vim itself would call "big enough"
// -- real vim instead routes a small (single-line, charwise) delete to
// a separate "- register and leaves the "1-"9 ring untouched for those.
// Skipped as an unlikely-to-matter nuance for a shell's own editor.
//
// `.`/`%`/`:` are read-only (vim's own last-inserted-text/current-
// filename/last-Ex-command registers) -- set directly by the specific
// code paths that produce each (see `set_last_insert`/`set_last_filename`/
// `set_last_ex_command`'s own doc comments), never through the ordinary
// `write` path (`ReadOnlyBackend::write` is a no-op, matching vim's own
// "can't be assigned to" rule for these three).

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

// `.`/`%`/`:`'s own storage -- reads back whatever was last set directly
// via `Registers::set_last_insert`/`set_last_filename`/`set_last_ex_command`;
// `write` (the ordinary `"x`-prefixed operator path) is a deliberate
// no-op, since none of these three can be assigned to in real vim either.
#[derive(Default)]
struct ReadOnlyBackend(RegisterValue);

impl RegisterBackend for ReadOnlyBackend {
    fn read(&self) -> RegisterValue {
        self.0.clone()
    }
    fn write(&mut self, _value: RegisterValue, _append: bool) {}
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
    numbered: HashMap<char, InMemoryBackend>,
    unnamed: ClipboardBackend,
    black_hole: BlackHoleBackend,
    last_insert: ReadOnlyBackend,
    last_filename: ReadOnlyBackend,
    last_ex_command: ReadOnlyBackend,
}

impl Registers {
    pub fn new() -> Self {
        Registers {
            named: HashMap::new(),
            numbered: HashMap::new(),
            unnamed: ClipboardBackend::new(),
            black_hole: BlackHoleBackend,
            last_insert: ReadOnlyBackend::default(),
            last_filename: ReadOnlyBackend::default(),
            last_ex_command: ReadOnlyBackend::default(),
        }
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
            Some(c) if c.is_ascii_digit() => (self.numbered.entry(c).or_default(), false),
            Some('.') => (&mut self.last_insert, false),
            Some('%') => (&mut self.last_filename, false),
            Some(':') => (&mut self.last_ex_command, false),
            // An unrecognized register name (shouldn't reach here --
            // vimkeys.rs only ever admits a-z/A-Z/0-9/+/"/_/./%/: into
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

    /// `y{motion}`/`yy`/Visual `y`'s own register write: exactly `write`,
    /// plus -- only when `name` is `None` (no explicit `"x` prefix, same
    /// gate vim itself uses) -- also updates "0 to this same value.
    pub fn record_yank(&mut self, name: Option<char>, value: RegisterValue) {
        if name.is_none() {
            self.numbered.insert('0', InMemoryBackend(value.clone()));
        }
        self.write(name, value);
    }

    /// `d{motion}`/`dd`/`x`/Visual `d`/`c{motion}`/`cc`'s own register
    /// write: exactly `write`, plus -- only when `name` is `None` -- shifts
    /// the "1-"9 ring up by one (discarding whatever was in "9") and
    /// writes this value into the now-empty "1.
    pub fn record_delete(&mut self, name: Option<char>, value: RegisterValue) {
        if name.is_none() {
            for i in (1..9).rev() {
                let from = char::from_digit(i, 10).unwrap();
                let to = char::from_digit(i + 1, 10).unwrap();
                let shifted = self.numbered.get(&from).map(|b| b.read()).unwrap_or_default();
                self.numbered.insert(to, InMemoryBackend(shifted));
            }
            self.numbered.insert('1', InMemoryBackend(value.clone()));
        }
        self.write(name, value);
    }

    /// `"."`: vim's own record of the text most recently typed during an
    /// Insert-mode (or Replace-mode) session -- set by whatever consumer
    /// owns a real typing loop at each of that loop's own exit points
    /// (today: only `fileeditor.rs`'s `run_insert_mode`; the shell
    /// prompt's own "Insert mode" is really just its ordinary core typing
    /// loop, which nothing here hooks into -- see `KeyOutcome::
    /// EnterReplace`'s own doc comment for the same "too central a loop to
    /// touch for this" reasoning).
    pub fn set_last_insert(&mut self, text: String) {
        self.last_insert.0 = RegisterValue { text, shape: RegisterShape::Char };
    }

    /// `"%"`: vim's own record of the current file's name -- set whenever
    /// a file editor session's own path becomes known or changes (opening
    /// a named file, or `:w`/`:wq`/`:x` naming a previously-unnamed one).
    pub fn set_last_filename(&mut self, name: String) {
        self.last_filename.0 = RegisterValue { text: name, shape: RegisterShape::Char };
    }

    /// `":"`: vim's own record of the last Ex command line entered -- set
    /// whenever one is read, successful or not (matching vim: a failed
    /// `:nonsense` still becomes the new `":`).
    pub fn set_last_ex_command(&mut self, cmd: String) {
        self.last_ex_command.0 = RegisterValue { text: cmd, shape: RegisterShape::Char };
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
    // global to the OS, not scoped to one `Registers` instance. `pub(crate)`
    // rather than private: editor.rs's own test module needs this too (for
    // its delete/change operator tests), not just this module's own tests.
    pub(crate) fn new_for_test() -> Self {
        Registers {
            named: HashMap::new(),
            numbered: HashMap::new(),
            unnamed: ClipboardBackend { tool: None, fallback: RegisterValue::default() },
            black_hole: BlackHoleBackend,
            last_insert: ReadOnlyBackend::default(),
            last_filename: ReadOnlyBackend::default(),
            last_ex_command: ReadOnlyBackend::default(),
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
    fn record_yank_populates_register_0_only_without_an_explicit_prefix() {
        let mut regs = Registers::new_for_test();
        regs.record_yank(None, char_val("yanked"));
        assert_eq!(regs.read(Some('0')).text, "yanked");

        regs.record_yank(Some('a'), char_val("into-a"));
        assert_eq!(regs.read(Some('a')).text, "into-a");
        // an explicit register target never touches "0
        assert_eq!(regs.read(Some('0')).text, "yanked");
    }

    #[test]
    fn record_delete_shifts_the_numbered_ring_without_an_explicit_prefix() {
        let mut regs = Registers::new_for_test();
        regs.record_delete(None, char_val("first"));
        assert_eq!(regs.read(Some('1')).text, "first");

        regs.record_delete(None, char_val("second"));
        assert_eq!(regs.read(Some('1')).text, "second");
        assert_eq!(regs.read(Some('2')).text, "first");

        regs.record_delete(None, char_val("third"));
        assert_eq!(regs.read(Some('1')).text, "third");
        assert_eq!(regs.read(Some('2')).text, "second");
        assert_eq!(regs.read(Some('3')).text, "first");
    }

    #[test]
    fn record_delete_with_an_explicit_register_never_touches_the_ring() {
        let mut regs = Registers::new_for_test();
        regs.record_delete(None, char_val("in-the-ring"));
        regs.record_delete(Some('a'), char_val("into-a"));
        assert_eq!(regs.read(Some('a')).text, "into-a");
        assert_eq!(regs.read(Some('1')).text, "in-the-ring");
    }

    #[test]
    fn record_delete_drops_the_oldest_entry_past_9() {
        let mut regs = Registers::new_for_test();
        for i in 1..=10 {
            regs.record_delete(None, char_val(&format!("del{i}")));
        }
        // "1 is the most recent, "9 is the oldest still kept; "del1" (the
        // very first delete) has fallen off the end.
        assert_eq!(regs.read(Some('1')).text, "del10");
        assert_eq!(regs.read(Some('9')).text, "del2");
    }

    #[test]
    fn read_only_registers_round_trip_what_was_set() {
        let mut regs = Registers::new_for_test();
        regs.set_last_insert("typed text".to_string());
        assert_eq!(regs.read(Some('.')).text, "typed text");
        regs.set_last_filename("/tmp/example.txt".to_string());
        assert_eq!(regs.read(Some('%')).text, "/tmp/example.txt");
        regs.set_last_ex_command("wq".to_string());
        assert_eq!(regs.read(Some(':')).text, "wq");
    }

    #[test]
    fn read_only_registers_reject_ordinary_writes() {
        let mut regs = Registers::new_for_test();
        regs.set_last_insert("original".to_string());
        regs.write(Some('.'), char_val("should not stick"));
        assert_eq!(regs.read(Some('.')).text, "original");
    }

    #[test]
    fn command_exists_finds_a_real_binary_and_rejects_a_fake_one() {
        assert!(command_exists("ls"));
        assert!(!command_exists("this-command-does-not-exist-anywhere-xyz"));
    }
}
