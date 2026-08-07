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

use std::io::{self, Read, Write};

use crate::term;

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

fn read_byte(stdin: &mut io::Stdin) -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    match stdin.read(&mut b) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(b[0])),
        Err(e) => Err(e),
    }
}

// Reads one key event from stdin, which raw mode has already put into
// unbuffered/no-echo/no-ISIG mode (see term::RawGuard) -- so every key,
// including Ctrl-C and Ctrl-Z, arrives here as plain bytes rather than as
// a signal. Handles the escape sequences a normal terminal sends for
// arrow/Home/End/Delete, and decodes multi-byte UTF-8 so non-ASCII input
// doesn't get mangled.
fn read_key(stdin: &mut io::Stdin) -> io::Result<Option<Key>> {
    let b = match read_byte(stdin)? {
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
        0x1b => return Ok(Some(read_escape(stdin)?)),
        0x20..=0x7e => Key::Char(b as char),
        _ if b >= 0xc0 => Key::Char(read_utf8_cont(stdin, b)?),
        _ => Key::Unknown,
    };
    Ok(Some(key))
}

// Escape sequence decoding: bare ESC (nothing follows within the raw
// single-byte reads that keep arriving) is reported as Unknown -- there's
// no notion of "wait briefly for more bytes" here, which is fine since a
// real escape sequence's bytes arrive back-to-back from the terminal.
fn read_escape(stdin: &mut io::Stdin) -> io::Result<Key> {
    let b1 = match read_byte(stdin)? {
        Some(b) => b,
        None => return Ok(Key::Unknown),
    };
    match b1 {
        b'[' | b'O' => {}
        _ => return Ok(Key::Unknown),
    }
    let b2 = match read_byte(stdin)? {
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
            let _ = read_byte(stdin)?;
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

fn read_utf8_cont(stdin: &mut io::Stdin, first: u8) -> io::Result<char> {
    let extra = if first >= 0xf0 {
        3
    } else if first >= 0xe0 {
        2
    } else {
        1
    };
    let mut buf = vec![first];
    for _ in 0..extra {
        if let Some(b) = read_byte(stdin)? {
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
pub fn read_line(prompt: &str) -> io::Result<ReadOutcome> {
    let mut guard = Some(term::RawGuard::enable(0)?);
    let mut ed = LineEditor::new();
    let mut stdin = io::stdin();

    print!("{}", prompt);
    io::stdout().flush()?;

    loop {
        let key = match read_key(&mut stdin)? {
            Some(k) => k,
            None => {
                drop(guard.take());
                return Ok(ReadOutcome::Eof);
            }
        };
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
            // History (Up/Down) isn't implemented yet -- decoded so the
            // key doesn't get misinterpreted as literal escape bytes, but
            // currently a no-op.
            Key::Up | Key::Down | Key::Unknown => {}
        }
        redraw(prompt, &ed)?;
    }
}
