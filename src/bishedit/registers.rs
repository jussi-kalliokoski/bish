// vim's register model: named storage that yank, put, delete and change
// read and write. Macros live in `vimkeys::VimKeys` instead: a recorded
// `editor::Key` sequence (arrows, Ctrl chords, mouse events) doesn't fit
// this module's text-only `RegisterValue`.
//
// Where this differs from vim:
//
// - Storage is pluggable (`RegisterBackend`): in-memory, black hole and
//   system clipboard.
// - The unnamed register is the `"+` register, as under vim's
//   `set clipboard=unnamedplus`: `None`, `Some('"')` and `Some('+')` all
//   resolve to the same `ClipboardBackend`.
// - Every delete or change shifts the `"1`-`"9` ring. vim sends small
//   (single-line, charwise) deletes to `"-` instead.
//
// `"0` holds the latest yank, and `"1`-`"9` the latest deletes, newest at
// `"1`. Both are written by `record_yank`/`record_delete`, and only without
// an explicit `"x` prefix: `"ayw` touches only `"a`, as in vim.
//
// `.`, `%` and `:` are read-only. Each is set by the code path that
// produces it (`set_last_insert`, `set_last_filename`,
// `set_last_ex_command`); a `write` to one does nothing.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterShape {
    Char,
    Line,
    /// A rectangle, as `Ctrl-V` selects and yanks it. Put back, each line
    /// goes at the same column on consecutive lines instead of end to end.
    Block,
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
    // This value as one line, for the shell's command line. A Line value ends
    // in `\n` (motion.rs bakes it in), which is dropped; any other newline,
    // from a multi-line charwise yank, becomes a space.
    pub fn flatten_to_single_line(&self) -> String {
        let mut s = self.text.as_str();
        if let RegisterShape::Line = self.shape {
            s = s.strip_suffix('\n').unwrap_or(s);
        }
        s.replace('\n', " ")
    }
}

/// A register's storage. Every name resolves to one of these.
trait RegisterBackend {
    fn read(&self) -> RegisterValue;
    /// `append` is set for an uppercase name (`"A`): the text is
    /// concatenated, and the result is `Line` if either side was, as in
    /// vim.
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
            let shape =
                if self.0.shape == RegisterShape::Line || value.shape == RegisterShape::Line { RegisterShape::Line } else { RegisterShape::Char };
            self.0.text.push_str(&value.text);
            self.0.shape = shape;
        } else {
            self.0 = value;
        }
    }
}

// Storage for `.`, `%` and `:`, set through `Registers`'s `set_last_*`.
// `write` does nothing: vim doesn't let them be assigned either.
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

// A copy and paste command pair for one clipboard tool. `paste_extra_args`
// is for `wl-paste -n` (see `detect_clipboard_tool`).
struct ClipboardTool {
    copy: (&'static str, &'static [&'static str]),
    paste: (&'static str, &'static [&'static str]),
}

// Each candidate needs both binaries on PATH; a tool that can only copy or
// only paste is useless. Found by scanning PATH, without running anything,
// which could hang or write the clipboard before the shell has started.
// Probing what's installed rather than gating on `target_os` also finds
// wl-clipboard under WSLg or xclip under XQuartz.
fn detect_clipboard_tool() -> Option<ClipboardTool> {
    // Never during a test run: anything that builds a real `Registers` (an
    // `App`, a `HexSession`) would use the developer's actual clipboard. And
    // `wl-copy` daemonizes holding the stdout it inherited, so
    // `cargo test | anything` would never see EOF.
    if cfg!(test) {
        return None;
    }
    // ...nor for a bish that something else drives, which `cfg!(test)`
    // can't see: the vimdiff corpus runs a real `bish tool edit` in a pty,
    // and half its cases start with `dd`. The spawner sets this.
    if std::env::var_os("BISH_NO_CLIPBOARD").is_some() {
        return None;
    }
    let candidates: [ClipboardTool; 4] = [
        ClipboardTool { copy: ("pbcopy", &[]), paste: ("pbpaste", &[]) },
        ClipboardTool { copy: ("xclip", &["-selection", "clipboard"]), paste: ("xclip", &["-selection", "clipboard", "-o"]) },
        ClipboardTool { copy: ("xsel", &["--clipboard", "--input"]), paste: ("xsel", &["--clipboard", "--output"]) },
        // wl-paste adds a trailing newline by default; `-n` stops it, so
        // `ClipboardBackend::read`'s Char-or-Line guess (does the text end in
        // `\n`?) sees the real content.
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

// Backs the unnamed and `"+` registers. With no clipboard tool on PATH it
// quietly uses `fallback`, an in-memory slot: a system without one is
// normal, not a misconfiguration.
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
                // A clipboard holds text without a shape, so the shape is
                // guessed from a trailing newline, which reads a blockwise
                // yank back as charwise. When the text is exactly what was
                // last put there, the recorded shape is used instead:
                // nothing else touched the clipboard. That's the only way a
                // block survives the system clipboard.
                if text == self.fallback.text {
                    return self.fallback.clone();
                }
                let shape = if text.ends_with('\n') { RegisterShape::Line } else { RegisterShape::Char };
                RegisterValue { text, shape }
            }
            // The tool vanished or failed (no display server, say): fall
            // back as if it was never found.
            _ => self.fallback.clone(),
        }
    }

    fn write(&mut self, value: RegisterValue, append: bool) {
        let merged = if append && !self.fallback.text.is_empty() {
            // The clipboard can't append, so read it first (`"A` should see
            // what's really there, not a stale fallback) and merge like
            // InMemoryBackend.
            let mut current = self.read();
            let shape =
                if current.shape == RegisterShape::Line || value.shape == RegisterShape::Line { RegisterShape::Line } else { RegisterShape::Char };
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
        write_osc52(&merged.text);
    }
}

// OSC 52 (`\x1b]52;c;<base64>\x07`) asks the terminal to set the
// clipboard, which works over SSH, where the local tools above would write
// the remote machine's. Sent in addition to a local tool, so both cases
// work without guessing which applies. Write-only: most terminals disable
// OSC 52's read query, since it would let any program read the clipboard.
// Not sent when stdout isn't a terminal.
fn write_osc52(text: &str) {
    if !stdout_is_tty() {
        return;
    }
    print!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let _ = std::io::stdout().flush();
}

fn stdout_is_tty() -> bool {
    crate::platform::is_terminal(1)
}

const BASE64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

// Standard alphabet, `=`-padded. Encode only, since OSC 52 is write-only
// here.
fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        out.push(BASE64_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(BASE64_ALPHABET[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(if let Some(b1) = b1 { BASE64_ALPHABET[(((b1 & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char } else { '=' });
        out.push(if let Some(b2) = b2 { BASE64_ALPHABET[(b2 & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// The register table, one for the whole shell, shared by every window,
/// pane and session, as vim's registers are global to the editor.
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

    /// `name` as the `"x` prefix typed it (`None` without one). Uppercase
    /// names share their lowercase storage; the case only decides `append`,
    /// which the caller handles.
    fn resolve(&mut self, name: Option<char>) -> (&mut dyn RegisterBackend, bool) {
        match name {
            None | Some('"') | Some('+') => (&mut self.unnamed, false),
            Some('_') => (&mut self.black_hole, false),
            Some(c) if c.is_ascii_uppercase() => (self.named.entry(c.to_ascii_lowercase()).or_default(), true),
            Some(c) if c.is_ascii_lowercase() => (self.named.entry(c).or_default(), false),
            Some(c) if c.is_ascii_digit() => (self.numbered.entry(c).or_default(), false),
            Some('.') => (&mut self.last_insert, false),
            Some('%') => (&mut self.last_filename, false),
            Some(':') => (&mut self.last_ex_command, false),
            // vimkeys only admits valid names, but an unknown one falls
            // back to the unnamed register rather than panicking.
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

    /// The register write for `y`, `yy` and Visual `y`: `write`, and without
    /// an explicit `"x` prefix also `"0`, as in vim.
    pub fn record_yank(&mut self, name: Option<char>, value: RegisterValue) {
        if name.is_none() {
            self.numbered.insert('0', InMemoryBackend(value.clone()));
        }
        self.write(name, value);
    }

    /// The register write for `d`, `dd`, `x`, `c` and their Visual forms:
    /// `write`, and without a prefix also shifting the `"1`-`"9` ring and
    /// putting this in `"1`.
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

    /// `".`: the text last typed in Insert or Replace mode, set at the
    /// exits of a typing loop. Only `fileeditor.rs`'s `run_insert_mode`
    /// sets it; the shell prompt's typing loop doesn't (see
    /// `KeyOutcome::EnterReplace`).
    pub fn set_last_insert(&mut self, text: String) {
        self.last_insert.0 = RegisterValue { text, shape: RegisterShape::Char };
    }

    /// `"%`: the current file's name, set when a file editor's path becomes
    /// known or changes (opening a file, or `:w` naming an unnamed one).
    pub fn set_last_filename(&mut self, name: String) {
        self.last_filename.0 = RegisterValue { text: name, shape: RegisterShape::Char };
    }

    /// `":`: the last Ex command line, set whether or not it succeeded, as in
    /// vim.
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
    // Uses the in-memory fallback for the unnamed register, so tests neither
    // touch the real clipboard nor collide with each other through it: it's
    // global to the OS. `pub(crate)` for editor.rs's tests too.
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
        // The uppercase name reads the same storage.
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
        // Nor does it reach the unnamed register.
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
        // An explicit register never touches `"0`.
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
        // `"1` is the newest and `"9` the oldest kept; `del1` fell off.
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

    // Vacuous without a clipboard tool. With `wl-copy` installed it keeps
    // the developer's clipboard intact and leaves no daemon behind.
    #[test]
    fn a_test_run_never_reaches_the_real_clipboard() {
        assert!(detect_clipboard_tool().is_none(), "a test found a clipboard tool on PATH and would have used it");
        let registers = Registers::new();
        assert!(registers.unnamed.tool.is_none(), "...and so would an ordinary Registers built inside a test");
    }

    #[test]
    fn command_exists_finds_a_real_binary_and_rejects_a_fake_one() {
        assert!(command_exists("ls"));
        assert!(!command_exists("this-command-does-not-exist-anywhere-xyz"));
    }

    // RFC 4648's test vectors, covering all three padding cases.
    #[test]
    fn base64_encode_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
