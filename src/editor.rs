// A from-scratch line editor for interactive input: decodes raw terminal
// bytes into key events, maintains an editable buffer with a cursor, and
// redraws itself after each edit. Deliberately scoped to single-line
// editing for now (each physical line the REPL reads is its own editing
// session; repl.rs still owns stitching continuation lines together into
// a multi-line program) -- but the buffer/cursor/dispatch machinery here
// is written to be reusable, since the plan is to grow this into an
// in-place prompt/buffer editor later (e.g. invoking $EDITOR on the
// current input), not just a one-off REPL helper.
//
// Known simplification: redraw doesn't account for terminal width, so a
// line that wraps past the terminal's column count will render oddly.
// Fixing that needs a TIOCGWINSZ ioctl and wrap-aware redraw math -- left
// for later, same spirit as the other documented gaps in this codebase.

use std::io::{self, Write};

use crate::history::History;
use crate::term;

unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Escape,
    CtrlA,
    CtrlB,
    CtrlC,
    CtrlD,
    CtrlE,
    CtrlF,
    CtrlK,
    CtrlL,
    CtrlU,
    CtrlW,
    CtrlZ,
    Unknown,
}

// How long to wait for further bytes after a lone Esc before deciding it
// really is a standalone Esc keypress rather than the start of a sequence.
// Real escape sequences arrive as a fast burst from the terminal; a human
// pressing Esc alone won't have a next byte ready this soon.
const ESCAPE_TIMEOUT_MS: i32 = 30;

// Reads directly from fd 0 via a raw syscall rather than std::io::Stdin --
// Stdin wraps its own internal BufReader, which can pull extra bytes out of
// the kernel in a single underlying read and hand them out later purely
// from userspace. term::stdin_ready's poll() only sees the kernel's view,
// so mixing it with a buffered reader would make the poll check lie: bytes
// already sitting in Stdin's buffer wouldn't show as "ready" even though
// they're available for the very next read. Going straight to the syscall
// keeps this consistent with what's actually been consumed.
fn read_byte() -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    loop {
        let n = unsafe { read(0, b.as_mut_ptr(), 1) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(if n == 0 { None } else { Some(b[0]) });
    }
}

// Reads one key event from stdin, which raw mode has already put into
// unbuffered/no-echo/no-ISIG mode (see term::RawGuard) -- so every key,
// including Ctrl-C and Ctrl-Z, arrives here as plain bytes rather than as
// a signal. Handles the escape sequences a normal terminal sends for
// arrow/Home/End/Delete, and decodes multi-byte UTF-8 so non-ASCII input
// doesn't get mangled.
fn read_key() -> io::Result<Option<Key>> {
    let b = match read_byte()? {
        Some(b) => b,
        None => return Ok(None),
    };
    let key = match b {
        0x01 => Key::CtrlA,
        0x02 => Key::CtrlB,
        0x03 => Key::CtrlC,
        0x04 => Key::CtrlD,
        0x05 => Key::CtrlE,
        0x06 => Key::CtrlF,
        0x0b => Key::CtrlK,
        0x0c => Key::CtrlL,
        0x15 => Key::CtrlU,
        0x17 => Key::CtrlW,
        0x1a => Key::CtrlZ,
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x1b => return Ok(Some(read_escape()?)),
        0x20..=0x7e => Key::Char(b as char),
        _ if b >= 0xc0 => Key::Char(read_utf8_cont(b)?),
        _ => Key::Unknown,
    };
    Ok(Some(key))
}

// Escape sequence decoding. A bare Esc keypress has nothing following it;
// poll with a short timeout at each step to tell that apart from an actual
// sequence (whose bytes arrive back-to-back) instead of blocking forever.
fn read_escape() -> io::Result<Key> {
    if !term::stdin_ready(ESCAPE_TIMEOUT_MS) {
        return Ok(Key::Escape);
    }
    let b1 = match read_byte()? {
        Some(b) => b,
        None => return Ok(Key::Escape),
    };
    match b1 {
        b'[' | b'O' => {}
        _ => return Ok(Key::Unknown),
    }
    if !term::stdin_ready(ESCAPE_TIMEOUT_MS) {
        return Ok(Key::Unknown);
    }
    let b2 = match read_byte()? {
        Some(b) => b,
        None => return Ok(Key::Unknown),
    };
    Ok(match b2 {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'1' | b'3' | b'4' | b'7' | b'8' => {
            // CSI <n> ~ forms (e.g. "\x1b[3~" for Delete). Consume the
            // trailing '~' and map the digit we already have.
            if term::stdin_ready(ESCAPE_TIMEOUT_MS) {
                let _ = read_byte()?;
            }
            match b2 {
                b'3' => Key::Delete,
                b'1' | b'7' => Key::Home,
                b'4' | b'8' => Key::End,
                _ => Key::Unknown,
            }
        }
        _ => Key::Unknown,
    })
}

fn read_utf8_cont(first: u8) -> io::Result<char> {
    let extra = if first >= 0xf0 {
        3
    } else if first >= 0xe0 {
        2
    } else {
        1
    };
    let mut buf = vec![first];
    for _ in 0..extra {
        if let Some(b) = read_byte()? {
            buf.push(b);
        }
    }
    Ok(std::str::from_utf8(&buf).ok().and_then(|s| s.chars().next()).unwrap_or('\u{FFFD}'))
}

struct LineEditor {
    buf: Vec<char>,
    cursor: usize,
}

impl LineEditor {
    fn new() -> Self {
        LineEditor { buf: Vec::new(), cursor: 0 }
    }

    fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
    }

    fn delete_forward(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += 1;
        }
    }

    fn kill_to_start(&mut self) {
        self.buf.drain(0..self.cursor);
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        self.buf.truncate(self.cursor);
    }

    // Matches the terminal driver's classic Ctrl-W (ERASEWORD): skip any
    // whitespace immediately before the cursor, then delete back to the
    // next whitespace boundary (or start of line).
    fn kill_word_backward(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        self.buf.drain(i..self.cursor);
        self.cursor = i;
    }

    fn as_string(&self) -> String {
        self.buf.iter().collect()
    }

    // Replaces the whole buffer (history recall), cursor at the end.
    fn set_text(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cursor = self.buf.len();
    }
}

fn redraw(prompt: &str, ed: &LineEditor) -> io::Result<()> {
    let text: String = ed.buf.iter().collect();
    let mut out = String::new();
    out.push('\r');
    out.push_str("\x1b[K"); // clear from column 0 to end of line
    out.push_str(prompt);
    out.push_str(&text);
    let back = ed.buf.len() - ed.cursor;
    if back > 0 {
        out.push_str(&format!("\x1b[{}D", back));
    }
    print!("{}", out);
    io::stdout().flush()
}

pub enum ReadOutcome {
    Line(String),
    Eof,
    Interrupted,
}

// Reads one line of input interactively: prompt is printed, raw mode is
// engaged only for the duration of this call (restored on every return
// path via RawGuard's Drop), so a foreground command run in between calls
// sees an entirely normal terminal.
pub fn read_line(prompt: &str, history: &History) -> io::Result<ReadOutcome> {
    let mut guard = Some(term::RawGuard::enable(0)?);
    let mut ed = LineEditor::new();

    // Fish-style history browsing: Up/Down search backward/forward through
    // history for entries starting with whatever was typed *before*
    // browsing started (that original text is `prefix`, restored on Esc
    // or on Down-ing past the newest match). Any other key -- moving the
    // cursor, editing, submitting -- silently "locks in" the currently
    // shown entry as ordinary buffer text and ends the browse, matching
    // fish: the suggestion just becomes your input from that point on.
    let mut browse: Option<(String, usize)> = None;

    print!("{}", prompt);
    io::stdout().flush()?;

    loop {
        let key = match read_key()? {
            Some(k) => k,
            None => {
                drop(guard.take());
                return Ok(ReadOutcome::Eof);
            }
        };
        if !matches!(key, Key::Up | Key::Down | Key::Escape) {
            browse = None;
        }
        match key {
            Key::Enter => {
                drop(guard.take());
                print!("\r\n");
                io::stdout().flush()?;
                return Ok(ReadOutcome::Line(ed.as_string()));
            }
            Key::CtrlC => {
                drop(guard.take());
                print!("^C\r\n");
                io::stdout().flush()?;
                return Ok(ReadOutcome::Interrupted);
            }
            Key::CtrlD => {
                if ed.buf.is_empty() {
                    drop(guard.take());
                    print!("\r\n");
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Eof);
                }
                ed.delete_forward();
            }
            Key::CtrlZ => {
                // Restore normal terminal state before stopping, so
                // whatever's watching this process (or using the tty
                // while we're stopped) sees a sane tty, then re-enable
                // raw mode and redraw once we're resumed.
                drop(guard.take());
                term::suspend_self();
                guard = Some(term::RawGuard::enable(0)?);
            }
            Key::Backspace => ed.backspace(),
            Key::Delete => ed.delete_forward(),
            Key::Left | Key::CtrlB => ed.move_left(),
            Key::Right | Key::CtrlF => ed.move_right(),
            Key::Home | Key::CtrlA => ed.cursor = 0,
            Key::End | Key::CtrlE => ed.cursor = ed.buf.len(),
            Key::CtrlK => ed.kill_to_end(),
            Key::CtrlU => ed.kill_to_start(),
            Key::CtrlW => ed.kill_word_backward(),
            Key::CtrlL => print!("\x1b[H\x1b[2J"),
            Key::Char(c) => ed.insert(c),
            Key::Up => {
                let prefix = browse.as_ref().map(|(p, _)| p.clone()).unwrap_or_else(|| ed.as_string());
                let from = browse.as_ref().map(|(_, i)| *i);
                if let Some((idx, entry)) = history.search_backward(&prefix, from) {
                    ed.set_text(entry);
                    browse = Some((prefix, idx));
                }
            }
            Key::Down => {
                if let Some((prefix, idx)) = browse.take() {
                    match history.search_forward(&prefix, idx) {
                        Some((new_idx, entry)) => {
                            ed.set_text(entry);
                            browse = Some((prefix, new_idx));
                        }
                        None => ed.set_text(&prefix),
                    }
                }
            }
            Key::Escape => {
                if let Some((prefix, _)) = browse.take() {
                    ed.set_text(&prefix);
                }
            }
            Key::Unknown => {}
        }
        redraw(prompt, &ed)?;
    }
}
