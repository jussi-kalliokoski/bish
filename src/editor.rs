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
    // Raw NUL (0x00) -- most terminals send this for Ctrl+Space. This
    // codebase's existing convention for "step back a level" (see
    // drive_fg_job's job-detach handling); reused here as the gesture that
    // enters bishedit's normal-mode navigation over the current pane.
    CtrlSpace,
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
    // xterm/tmux send these as "CSI 1 ; 3 <letter>" (modifier 3 = Alt)
    // rather than the plain "CSI <letter>" -- see decode_csi_final.
    AltLeft,
    AltRight,
    AltUp,
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
    // No shell line-editing use yet (unlike the other Ctrl letters above) --
    // added for bishedit's normal-mode Ctrl-Y (scroll one line up).
    CtrlY,
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
        0x00 => Key::CtrlSpace,
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
        0x19 => Key::CtrlY,
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
    // Collect parameter bytes (digits and ';') until the final byte (a
    // letter, or '~') arrives -- covers both the simple forms (no
    // params, e.g. "\x1b[D" for Left) and the modified ones xterm/tmux
    // send for a Ctrl/Alt/Shift-held arrow or Home/End (e.g. "\x1b[1;3D"
    // for Alt+Left), plus the "\x1b[<n>~" family (Delete/Home/End on
    // some terminals) -- see decode_csi_final for how each is told
    // apart.
    let mut params = String::new();
    loop {
        if !term::stdin_ready(ESCAPE_TIMEOUT_MS) {
            return Ok(Key::Unknown);
        }
        let b = match read_byte()? {
            Some(b) => b,
            None => return Ok(Key::Unknown),
        };
        if b.is_ascii_digit() || b == b';' {
            params.push(b as char);
            continue;
        }
        return Ok(decode_csi_final(&params, b));
    }
}

// `params` is whatever digit/';' bytes came between the CSI intro
// ("\x1b[") and `final_byte`, e.g. "" (plain arrow), "3" (an "~"-form
// like Delete), or "1;3" (xterm's "CSI 1 ; modifier <letter>" convention
// for a modified arrow/Home/End -- modifier 3 is Alt; 2/4/5/6/7/8 are
// Shift/Alt+Shift/Ctrl/Ctrl+Shift/Ctrl+Alt/Ctrl+Alt+Shift, none of which
// this editor distinguishes from the plain key, matching its existing
// scope of no Ctrl/Shift-arrow handling).
fn decode_csi_final(params: &str, final_byte: u8) -> Key {
    let parts: Vec<&str> = params.split(';').collect();
    let alt = parts.get(1) == Some(&"3");
    match final_byte {
        b'A' => {
            if alt {
                Key::AltUp
            } else {
                Key::Up
            }
        }
        b'B' => Key::Down,
        b'C' => {
            if alt {
                Key::AltRight
            } else {
                Key::Right
            }
        }
        b'D' => {
            if alt {
                Key::AltLeft
            } else {
                Key::Left
            }
        }
        b'H' => Key::Home,
        b'F' => Key::End,
        b'~' => match parts.first().and_then(|s| s.parse::<u32>().ok()) {
            Some(3) => Key::Delete,
            Some(1) | Some(7) => Key::Home,
            Some(4) | Some(8) => Key::End,
            _ => Key::Unknown,
        },
        _ => Key::Unknown,
    }
}

// How long to wait for a keystroke before giving on_idle another chance
// to run (see read_line's on_idle doc comment) and polling again. Short
// enough that a job running in another window doesn't visibly stall.
const IDLE_POLL_MS: i32 = 15;

// Blocks until a key is available, but never for longer than
// IDLE_POLL_MS at a stretch -- calls on_idle and loops back to poll again
// in between. Once a byte is actually ready, hands off to the ordinary
// (genuinely blocking) read_key/read_byte/read_escape machinery
// unchanged: a real key event's remaining bytes (an escape sequence's
// tail, a UTF-8 continuation byte) always arrive close enough behind the
// first that there's nothing to gain by polling those sub-reads too.
pub(crate) fn read_key_idle(on_idle: &mut dyn FnMut()) -> io::Result<Option<Key>> {
    while !term::stdin_ready(IDLE_POLL_MS) {
        on_idle();
    }
    read_key()
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

// `col_origin` is the real terminal column (0-indexed) this line's own
// column 0 actually sits at -- 0 for the ordinary case (a bare `\r`
// already lands there), non-zero when this read_line call is running
// inside a compositor pane that doesn't start at the terminal's left
// edge (see read_line's own doc comment). A bare `\r` always means
// "real column 0," which is wrong the moment a pane's own left edge
// isn't the terminal's; `\x1b[nG` (Cursor Horizontal Absolute) moves to
// column n of whatever row the cursor is already on instead, so it's
// used here even for col_origin == 0 (identical effect to `\r` in that
// case, so this is a no-op change for every non-paned caller).
// `width`: how many columns of this row belong to this line (the
// pane's own cols, or the real terminal's width for the ordinary
// non-paned case). Erasing is done by overwriting exactly this many
// columns with spaces rather than `\x1b[K` (erase to end of the
// *physical* terminal line): the bare terminal-wide form is wrong the
// moment another pane shares this same real row further to the right
// -- it would wipe that pane's content too. For the ordinary
// non-paned/full-width case this is behaviorally identical to the old
// `\x1b[K`.
//
// `width` also bounds how much of prompt+buffer actually gets printed:
// once that combined text would exceed it, the real terminal's own
// autowrap (never disabled here) would wrap the overflow to column 1 of
// the *next* row -- fine for the non-paned case (nothing else is there
// to collide with), but that row belongs to some other pane the moment
// this one doesn't span the whole terminal width, so the overflowing
// characters would render into a neighboring pane's own content. Once
// the buffer alone is too long to fit in what's left after the prompt,
// this horizontally scrolls just the buffer text (prompt always stays
// fully visible) so the cursor's own position is always shown, the same
// "long line stays on one row" behavior GNU readline itself falls back
// to in a narrow terminal -- recomputed fresh on every redraw from the
// cursor's current position, not a persisted scroll offset.
fn redraw(prompt: &str, ed: &LineEditor, col_origin: usize, width: usize) -> io::Result<()> {
    let mut out = String::new();
    out.push_str(&format!("\x1b[{}G", col_origin + 1));
    out.push_str(&" ".repeat(width));
    out.push_str(&format!("\x1b[{}G", col_origin + 1));

    let prompt_len = visible_len(prompt);
    if prompt_len >= width {
        // The prompt alone doesn't fit this pane's width -- show as
        // much of it as does; there's no room left for any buffer text
        // to show anyway. A real edge case (needs a pane narrower than
        // the prompt itself, typically a 4+ way split), not worth more
        // than graceful degradation.
        out.push_str(&truncate_visible(prompt, width));
        out.push_str("\x1b[0m");
        print!("{}", out);
        return io::stdout().flush();
    }

    out.push_str(prompt);
    let remaining = width - prompt_len;
    if ed.buf.len() <= remaining {
        let text: String = ed.buf.iter().collect();
        out.push_str(&text);
        let back = ed.buf.len() - ed.cursor;
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    } else {
        // Right-align the visible window on the cursor whenever it
        // would otherwise fall outside what's currently shown --
        // clamped so the window never scrolls past showing the
        // buffer's own tail once the cursor's at or past the end
        // (typing forward, by far the common case).
        let window_start = ed.cursor.saturating_sub(remaining - 1).min(ed.buf.len() - remaining);
        let window_end = (window_start + remaining).min(ed.buf.len());
        let window: String = ed.buf[window_start..window_end].iter().collect();
        out.push_str(&window);
        let back = (window_end - window_start) - (ed.cursor - window_start);
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    }

    print!("{}", out);
    io::stdout().flush()
}

// How many terminal columns `s` actually occupies once drawn, not
// counting invisible escape bytes -- `s` is always one of this crate's
// own prompt strings, which only ever embed `\x1b[...m` SGR (color)
// codes, so that's the only escape form this needs to recognize.
fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        len += 1;
    }
    len
}

// Like visible_len, but returns the prefix of `s` whose *visible*
// portion is at most `max_visible` columns, preserving any embedded SGR
// codes encountered along the way (they don't count against the
// budget) rather than risking a mid-escape-sequence cut.
fn truncate_visible(s: &str, max_visible: usize) -> String {
    let mut out = String::new();
    let mut visible = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().unwrap());
            for c2 in chars.by_ref() {
                out.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
            continue;
        }
        if visible >= max_visible {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out
}

pub enum ReadOutcome {
    Line(String),
    Eof,
    Interrupted,
    // Alt+Left/Right/Up at an empty buffer -- see DirNav's own doc
    // comment. Never fires with anything typed (see read_line's own
    // handling), so there's no "what happens to the buffer" question.
    DirNav(DirNav),
    // Ctrl+Space at an empty buffer -- same "don't discard in-progress
    // typing" gating as DirNav above. Enters bishedit's normal-mode
    // navigation over the current pane's own rendered content (repl.rs's
    // run_normal_mode_navigation) -- the *only* way into command mode now
    // (via normal mode's own ':', matching real vim) since the old direct
    // ':'-at-the-shell-prompt shortcut was retired: it existed only to let
    // command mode be reached before normal mode did, and became
    // redundant clutter (a whole arm/materialize/commit/re-arm dance
    // living inside ordinary shell-mode typing) the moment normal mode
    // could get there properly. `i` in normal mode returns to a fresh
    // read_line call, same as this one would have continued into anyway.
    NormalMode,
}

// Directory-history navigation (browser-style back/forward, plus "up to
// parent"), triggered by Alt+Left/Right/Up. repl.rs owns the actual
// per-session history stack and cd logic -- this just reports which
// direction was requested, the same "editor.rs only decodes keys, the
// caller decides what they mean" split as NormalMode/Eof/Interrupted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DirNav {
    Back,
    Forward,
    Up,
}

// Reads one line of input interactively: prompt is printed, raw mode is
// engaged only for the duration of this call (restored on every return
// path via RawGuard's Drop), so a foreground command run in between calls
// sees an entirely normal terminal.
// `history` is the calling session's own History value (see its doc
// comment): a persistent, per-session chain, so Up/Down here naturally
// only ever sees that session's own commands plus whatever it inherited
// at fork time -- no separate boundary/cutoff parameter needed, the
// scope is inherent to which chain was passed in.
// `esc_cancels`: whether Esc, when there's no active history browse to
// unwind instead, cancels the whole read (same ReadOutcome::Interrupted
// as Ctrl-C, just without the "^C" echo). Off for the normal shell
// prompt (Esc there stays a no-op outside of history browsing, matching
// this crate's existing behavior); on for command mode, which -- like a
// vim ':' command line -- should be cancelable by either Ctrl-C or Esc
// regardless of what's been typed.
// `on_idle`: called whenever this is about to block waiting for the next
// keystroke and none has arrived yet within one poll tick -- lets
// repl.rs's main loop (M10c) service other windows' backgrounded fg jobs
// (draining their pty output into their own grids, reaping them if
// they've exited) while this window sits idle at its own prompt, instead
// of a job elsewhere silently stalling until this read finally returns.
// A plain `|| {}` is a correct, if inert, choice for callers with nothing
// to service (e.g. command mode's own nested read_line -- see that call
// site's comment on why this is scoped out there for now).
// `col_origin`/`width`: the real terminal column this prompt's own
// column 0 actually sits at, and how many columns of that row belong
// to it -- see redraw's own doc comment. col_origin 0 and width equal
// to the real terminal's width for every non-paned caller (the plain
// terminal, or a promoted-but-unsplit window, both of which start at
// the real column 0 and own the whole row anyway); the current focused
// pane's own rect.col/rect.cols once a window is split (see repl.rs's
// pane_rect). The caller is responsible for having already positioned
// the real cursor correctly (row included) before calling this --
// read_line only ever needs to return to *this same* row's own start
// column while redrawing during typing, never to reposition the row
// itself (single-line editing, see this function's own scope note).
pub fn read_line(
    prompt: &str,
    history: &History,
    esc_cancels: bool,
    col_origin: usize,
    width: usize,
    mut on_idle: impl FnMut(),
) -> io::Result<ReadOutcome> {
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

    // Always goes through the same clear-and-draw redraw(), even with
    // nothing typed yet: the terminal cursor at this point may already be
    // sitting right after a compositor-frozen idle prompt for this exact
    // pane (see repl.rs's freeze_idle_prompt) -- a bare, non-clearing
    // print here would append a second copy right next to it instead of
    // redrawing over it. Behaviorally identical to the old bare print for
    // every non-paned caller (col_origin 0, width the whole terminal).
    redraw(prompt, &ed, col_origin, width)?;

    loop {
        let key = match read_key_idle(&mut on_idle)? {
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
            Key::Backspace => {
                // esc_cancels is only ever set for a command-mode
                // read_line call (see its own doc comment) -- matches
                // real vim: Backspace on an empty Ex command line drops
                // back out of it (here, to normal mode -- see repl.rs's
                // run_normal_mode_navigation, the only caller that sets
                // esc_cancels true now). Only when the buffer is truly
                // empty, not just "cursor at 0" -- with text still to the
                // right (e.g. after Home) Backspace has nothing to its
                // left to delete either way, so it's a plain no-op,
                // falling through to ed.backspace() below.
                if esc_cancels && ed.buf.is_empty() {
                    drop(guard.take());
                    print!("\r\n");
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Interrupted);
                }
                ed.backspace();
            }
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
                    ed.set_text(&entry);
                    browse = Some((prefix, idx));
                }
            }
            Key::Down => {
                if let Some((prefix, idx)) = browse.take() {
                    match history.search_forward(&prefix, idx) {
                        Some((new_idx, entry)) => {
                            ed.set_text(&entry);
                            browse = Some((prefix, new_idx));
                        }
                        None => ed.set_text(&prefix),
                    }
                }
            }
            Key::Escape => match browse.take() {
                // First priority, regardless of esc_cancels: if a history
                // browse is active, Esc just cancels *that* and restores
                // what was typed before it started -- matching readline/
                // vim conventions where Esc unwinds the innermost special
                // state before it ever means "abandon everything."
                Some((prefix, _)) => ed.set_text(&prefix),
                // Nothing to unwind: for callers that opted in (command
                // mode -- see esc_cancels' doc comment), this is "cancel
                // the prompt," same outcome as Ctrl-C.
                None if esc_cancels => {
                    drop(guard.take());
                    print!("\r\n");
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Interrupted);
                }
                None => {}
            },
            // Only at an empty buffer -- same reasoning as ':' arming
            // command mode: don't let a keypress the user might not
            // have meant to hit right now silently discard something
            // they were in the middle of typing. Directory navigation
            // is a "instead of typing a command" action, not something
            // that makes sense mid-line anyway.
            Key::AltLeft if ed.buf.is_empty() => {
                drop(guard.take());
                print!("\r\n");
                io::stdout().flush()?;
                return Ok(ReadOutcome::DirNav(DirNav::Back));
            }
            Key::AltRight if ed.buf.is_empty() => {
                drop(guard.take());
                print!("\r\n");
                io::stdout().flush()?;
                return Ok(ReadOutcome::DirNav(DirNav::Forward));
            }
            Key::AltUp if ed.buf.is_empty() => {
                drop(guard.take());
                print!("\r\n");
                io::stdout().flush()?;
                return Ok(ReadOutcome::DirNav(DirNav::Up));
            }
            // No trailing "\r\n" here, unlike DirNav above: entering
            // normal mode redraws the pane's own rectangle in place (see
            // repl.rs's run_normal_mode_navigation), not a fresh line at
            // the real terminal's current cursor position.
            Key::CtrlSpace if ed.buf.is_empty() => {
                drop(guard.take());
                return Ok(ReadOutcome::NormalMode);
            }
            Key::AltLeft | Key::AltRight | Key::AltUp | Key::CtrlSpace | Key::CtrlY | Key::Unknown => {}
        }
        redraw(prompt, &ed, col_origin, width)?;
    }
}
