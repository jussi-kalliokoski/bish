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

use std::collections::HashMap;
use std::io::{self, Write};

use crate::bishedit::completion::{self, CompletionCandidate, CompletionProvider, CompletionRequest};
use crate::bishedit::highlight::{self, BashHighlighter, Highlighter, HighlightContext, StyledSpan};
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::snippet::{self, Abbr, LiveSnippet, Snippet};
use crate::bishedit::suggestion::{SuggestionProvider, SuggestionRequest};
use crate::bishedit::undo::UndoTree;
use crate::bishedit::unicode_width::char_width;
use crate::bishedit::Buffer;
use crate::bishedit::vimkeys::{self, KeyOutcome, Op, VimKeys};
use crate::history::History;
// Same cross-module use fileeditor.rs already makes of these two -- see
// run_line_normal_mode's own doc comment on its `term_rows` param.
use crate::repl::{erase_global_status_row, render_global_status_row};
use crate::term;
use crate::vt100;

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
    // "CSI 5 ~"/"CSI 6 ~" -- see decode_csi_final. Not decoded before this
    // existed at all (fell through to Unknown), which is why neither ever
    // did anything in any mode (Normal or Insert) of bishedit's own file
    // editor, or the plain shell prompt -- see run_insert_mode/vimkeys.rs's
    // own Motion::PageUp/PageDown handling for where these actually go now.
    PageUp,
    PageDown,
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
    // Completion cycling -- see read_line's own completion handling.
    // BackTab is xterm's CBT sequence ("CSI Z", shift-tab); CtrlN/CtrlP
    // are the same forward/backward pair emacs-style line editors use.
    Tab,
    BackTab,
    CtrlN,
    CtrlP,
    // Vim's insert-mode "do exactly one normal command, then return to
    // insert" -- see run_one_shot_normal_command's own doc comment.
    CtrlO,
    CtrlU,
    CtrlW,
    // Vim's insert-mode "paste this register here" -- see read_line's own
    // Key::CtrlR arm.
    CtrlR,
    // No shell line-editing use yet (unlike the other Ctrl letters above) --
    // added for bishedit's normal-mode Ctrl-Y (scroll one line up).
    CtrlY,
    CtrlZ,
    // No shell line-editing use either -- added for bishedit's own
    // Normal-mode number decrement (`Ctrl-X`, `Ctrl-A`'s own mirror).
    CtrlX,
    // An SGR (mode 1006) mouse report -- see `read_sgr_mouse`'s own doc
    // comment for the wire format. Decoded here purely so a real mouse
    // event arriving at any of this codebase's several `read_key`-based
    // loops (the shell prompt's own typing loop, bishedit's normal-mode
    // navigation, the `e` file editor) is safely consumed as one unit
    // rather than leaking its raw digit/`;`/`M` bytes through as
    // individual bogus keystrokes (what happened before this variant
    // existed -- `read_escape`'s param loop had no notion of the `<`
    // marker byte this sequence starts with). None of those loops act on
    // it yet -- it lands in whatever catch-all bucket already exists
    // there for an unbound key, same as `Unknown`. The one place mouse
    // input is actually *used* is `repl.rs`'s `drive_fg_job`, which never
    // goes through `Key` at all (it forwards raw stdin bytes straight to
    // a foreground job's pty, mouse sequences included) -- see its own
    // doc comment on why real-terminal mouse reporting is enabled only
    // while that loop owns the terminal.
    Mouse(MouseEvent),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    // The raw Cb parameter from the SGR sequence: encodes which button
    // (bits 0-1, plus bit 6 for buttons 4-7), whether this is motion
    // while a button is held (bit 5), and Shift/Meta/Ctrl modifiers (bits
    // 2-4) -- xterm's own ctlseqs.txt has the full bit layout. Left
    // un-decoded into separate fields since nothing here reads it yet;
    // whatever eventually does (click-to-focus-pane, in a later pass)
    // can add real accessors then instead of guessing at a shape now.
    pub button: u16,
    // 1-indexed column/row, matching the terminal's own coordinate
    // convention (row 1 = top).
    pub col: u16,
    pub row: u16,
    // `true` for a press/drag (final byte `M`), `false` for a release
    // (final byte `m`).
    pub pressed: bool,
}

impl MouseEvent {
    // A genuine left-button press -- not a drag (bit 5, set while a
    // button is held during motion), not a wheel event (bit 6), not some
    // other button (bits 0-1 != 0), and not a release (`pressed ==
    // false`, final byte 'm'). The only gesture this codebase treats as
    // "a click" so far -- see this method's own call sites for why
    // everything else it decodes is deliberately left alone for now.
    pub fn is_left_click(&self) -> bool {
        self.pressed && self.button & 0x60 == 0 && self.button & 0x03 == 0
    }

    // xterm's SGR mouse protocol encodes a wheel notch as button 4 (up) or
    // 5 (down) -- bit 6 set (the same bit that also marks buttons 6/7,
    // which this codebase has no use for and doesn't distinguish), plus
    // bits 0-1 holding 0 or 1. Modifier bits (2-4) are masked out, same as
    // is_left_click above -- a Shift/Ctrl/Alt-held wheel notch still
    // scrolls. A wheel notch is always reported as a bare press (no
    // matching release), but `pressed` is still checked here anyway --
    // cheap insurance against a terminal that behaves differently, same
    // spirit as is_left_click's own explicit check.
    pub fn is_scroll_up(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x40
    }

    pub fn is_scroll_down(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x41
    }
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
        0x09 => Key::Tab,
        0x0b => Key::CtrlK,
        0x0c => Key::CtrlL,
        0x0e => Key::CtrlN,
        0x0f => Key::CtrlO,
        0x10 => Key::CtrlP,
        0x12 => Key::CtrlR,
        0x15 => Key::CtrlU,
        0x17 => Key::CtrlW,
        0x18 => Key::CtrlX,
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
    // An SGR mouse report ("\x1b[<Cb;Cx;CyM/m") starts with a '<' marker
    // right after the CSI intro -- unlike every other sequence this
    // decoder handles, whose first param byte (if any) is always a digit
    // or ';' (see the loop just below). Peeking for it here, before that
    // loop even starts, keeps this the only place that needs to know
    // about it.
    if b1 == b'[' {
        if !term::stdin_ready(ESCAPE_TIMEOUT_MS) {
            return Ok(Key::Unknown);
        }
        let b2 = match read_byte()? {
            Some(b) => b,
            None => return Ok(Key::Unknown),
        };
        if b2 == b'<' {
            return read_sgr_mouse();
        }
        // Not a mouse report -- fall into the ordinary param-collection
        // loop below with `b2` as its first byte, exactly as if it had
        // been read there in the first place.
        return read_csi_params(String::from(b2 as char));
    }
    read_csi_params(String::new())
}

// Collects parameter bytes (digits and ';') until the final byte (a
// letter, or '~') arrives -- covers both the simple forms (no params,
// e.g. "\x1b[D" for Left) and the modified ones xterm/tmux send for a
// Ctrl/Alt/Shift-held arrow or Home/End (e.g. "\x1b[1;3D" for Alt+Left),
// plus the "\x1b[<n>~" family (Delete/Home/End on some terminals) -- see
// decode_csi_final for how each is told apart. `seed` is whatever byte
// `read_escape` already consumed while peeking for a mouse report's `<`
// marker (empty for CSI-O sequences, which never need that peek).
fn read_csi_params(seed: String) -> io::Result<Key> {
    let mut params = seed;
    if params.chars().next().is_some_and(|c| !c.is_ascii_digit() && c != ';') {
        // The peeked byte was itself the final byte (a paramless
        // sequence like "\x1b[H" for Home) -- nothing left to collect.
        let final_byte = params.pop().unwrap() as u8;
        return Ok(decode_csi_final(&params, final_byte));
    }
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

// "\x1b[<" already consumed. Collects "Cb;Cx;Cy" then a final 'M' (press
// or drag) or 'm' (release), matching xterm's SGR (mode 1006) mouse
// protocol -- the only mouse encoding this decoder understands (the
// legacy X10/UTF-8 encodings some terminals still default to are out of
// scope: they can't represent coordinates past column/row 223, and
// nothing in this codebase enables mouse reporting on the real terminal
// without also requesting SGR mode -- see drive_fg_job's own doc
// comment).
fn read_sgr_mouse() -> io::Result<Key> {
    let mut params = String::new();
    loop {
        if !term::stdin_ready(ESCAPE_TIMEOUT_MS) {
            return Ok(Key::Unknown);
        }
        let b = match read_byte()? {
            Some(b) => b,
            None => return Ok(Key::Unknown),
        };
        match b {
            b'M' | b'm' => return Ok(decode_sgr_mouse_final(&params, b)),
            b if b.is_ascii_digit() || b == b';' => params.push(b as char),
            _ => return Ok(Key::Unknown),
        }
    }
}

// `params` is the "Cb;Cx;Cy" collected between "\x1b[<" and `final_byte`
// (always 'M' or 'm' -- `read_sgr_mouse`'s own only two call sites for
// this). Split out as a pure function, mirroring decode_csi_final's own
// split from read_escape, so the actual parsing has unit test coverage
// without needing real stdin bytes.
pub(crate) fn decode_sgr_mouse_final(params: &str, final_byte: u8) -> Key {
    let parts: Vec<&str> = params.split(';').collect();
    let button = parts.first().and_then(|s| s.parse::<u16>().ok());
    let col = parts.get(1).and_then(|s| s.parse::<u16>().ok());
    let row = parts.get(2).and_then(|s| s.parse::<u16>().ok());
    match (button, col, row) {
        (Some(button), Some(col), Some(row)) => Key::Mouse(MouseEvent { button, col, row, pressed: final_byte == b'M' }),
        _ => Key::Unknown,
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
            Some(5) => Key::PageUp,
            Some(6) => Key::PageDown,
            _ => Key::Unknown,
        },
        b'Z' => Key::BackTab,
        _ => Key::Unknown,
    }
}

// How long to wait for a keystroke before giving on_idle another chance
// to run (see read_line's on_idle doc comment) and polling again. Short
// enough that a job running in another window doesn't visibly stall.
// pub(crate): repl.rs's run_normal_mode_navigation needs this same
// interval for its own pre-wait loop (see that function's own comment
// on why it can't just reuse read_key_idle's on_idle closure directly).
pub(crate) const IDLE_POLL_MS: i32 = 15;

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

    // Replaces buf[start..end] with `text`, cursor landing right after the
    // inserted text. The one primitive both completion cycling and
    // Ctrl-E's discard-and-revert share: a completion candidate is
    // spliced in as real buffer content (not a separate dimmed overlay),
    // which is what lets "keep typing" accept it for free -- the buffer
    // is already correct, nothing special has to happen on acceptance.
    fn splice_word(&mut self, start: usize, end: usize, text: &str) {
        let new_chars: Vec<char> = text.chars().collect();
        let new_len = new_chars.len();
        self.buf.splice(start..end, new_chars);
        self.cursor = start + new_len;
    }
}

// Tracks an in-progress multi-candidate completion: `word_start..cursor`
// (the buffer's *current* cursor, always kept in sync by splice_word) is
// the currently-spliced candidate's own span, `original` is what was
// there before completion started (restored verbatim by Ctrl-E), and
// `selected` indexes into `candidates` for cycling. Its mere existence
// means "more than one candidate, ghost text is currently spliced into
// the real buffer" -- a single-candidate auto-fill never creates one
// (see trigger_completion), matching the request's own "just fill it
// without showing the menu" spec.
struct CompletionState {
    word_start: usize,
    original: String,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

// Runs the provider fresh (no caching, matching this feature's blanket
// "recomputed every trigger" scope) and either fills the one candidate
// directly (0 or 1 results never need a menu/cycling state at all) or
// splices in the first (Tab/Ctrl-N) or last (Shift-Tab/Ctrl-P) of several
// and returns tracking state for cycling.
fn trigger_completion(ed: &mut LineEditor, provider: &dyn CompletionProvider, backward: bool) -> Option<CompletionState> {
    let line = ed.as_string();
    let result = provider.complete(CompletionRequest { line: &line, cursor: ed.cursor });
    match result.candidates.len() {
        0 => None,
        1 => {
            ed.splice_word(result.word_start, ed.cursor, &result.candidates[0].display);
            None
        }
        n => {
            let original: String = ed.buf[result.word_start..ed.cursor].iter().collect();
            let selected = if backward { n - 1 } else { 0 };
            let text = result.candidates[selected].display.clone();
            ed.splice_word(result.word_start, ed.cursor, &text);
            Some(CompletionState { word_start: result.word_start, original, candidates: result.candidates, selected })
        }
    }
}

// Removes the currently-spliced candidate's own range (word_start..cursor,
// which splice_word always keeps in sync) and splices in the next/previous
// one, wrapping. `state.candidates` itself never changes across a cycle --
// only which one is currently shown.
fn cycle_completion(ed: &mut LineEditor, state: &mut CompletionState, backward: bool) {
    let n = state.candidates.len();
    state.selected = if backward { (state.selected + n - 1) % n } else { (state.selected + 1) % n };
    let text = state.candidates[state.selected].display.clone();
    ed.splice_word(state.word_start, ed.cursor, &text);
}

// Recomputed fresh on every redraw rather than tracked across
// keystrokes -- same no-caching reasoning as the highlighter and the
// completion sources (fuzzy/PATH/manpages), and, more importantly, a
// real correctness trap avoided: by the time any match arm runs, the
// completion reset rule above it may have already cleared `completion`,
// so recomputing a suggestion *inside* an arm could produce one in
// exactly the keystroke where it must not exist (suggestions aren't
// applicable while the completion menu is active). Computing once,
// before dispatch, and treating the result purely as "what's currently
// rendered" sidesteps that -- the value read by Right/Ctrl-Y's own arms
// is always exactly what was just drawn.
//
// Gate, all of which must hold: a provider was given; no completion is
// active (including through the single-candidate auto-fill path, which
// leaves `completion` None once it's spliced in -- a suggestion can
// legitimately appear right after an auto-fill completes, correctly
// indistinguishable from ordinary typing at that point); no history
// browse is active (Up/Down recalls a *complete* entry -- painting a
// ghost that extends it into a *different* command would read as noise
// while scanning, and the moment browse resets on any other key the
// ghost returns on its own); cursor at the buffer's end (suggestions
// are end-of-line only, see the request's own scope); buffer non-empty.
fn compute_suggestion(ed: &LineEditor, provider: Option<&dyn SuggestionProvider>, completion_active: bool, browsing: bool) -> Option<String> {
    let provider = provider?;
    if completion_active || browsing || ed.buf.is_empty() || ed.cursor != ed.buf.len() {
        return None;
    }
    let line = ed.as_string();
    let suggestion = provider.suggest(SuggestionRequest { line: &line, cursor: ed.cursor })?;
    Some(suggestion.text.chars().skip(ed.buf.len()).collect())
}

// Turns the ghost tail currently being rendered into real buffer text.
// Only ever reachable with the cursor already at the buffer's end --
// the sole condition under which a suggestion is computed at all -- so
// this is a plain append with the cursor landing right after it,
// expressed through splice_word (the same primitive completion
// acceptance already goes through) rather than opening a second
// insertion path.
fn accept_suggestion(ed: &mut LineEditor, tail: &str) {
    let end = ed.buf.len();
    ed.splice_word(end, end, tail);
}

// The line editor's own half of a live snippet: everything about
// *editing* one lives in bishedit::snippet::LiveSnippet, shared with the
// file editor, and all that's left here is the two-line buffer adapter
// plus how it's drawn. `line` is always 0 -- this buffer is one line.
impl snippet::SnippetHost for LineEditor {
    fn replace_in_line(&mut self, _line: usize, start: usize, end: usize, text: &str) {
        self.buf.splice(start..end, text.chars());
    }

    fn place_cursor(&mut self, _line: usize, col: usize) {
        self.cursor = col;
    }
}

// The highlight layer that makes a tentative snippet read as one: the
// placeholder being typed into is reverse-video (the same "this is the
// selected one" mark the completion menu already uses), the rest are
// underlined. Both are drawn over whatever the syntax highlighter made
// of the text underneath, since a half-finished command line is
// routinely not valid syntax yet.
fn snippet_layer(live: &LiveSnippet) -> Vec<StyledSpan> {
    live.holes()
        .into_iter()
        .map(|(start, end, active)| StyledSpan {
            start,
            end,
            fg: vt100::Color::Default,
            attrs: if active {
                vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() }
            } else {
                vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() }
            },
        })
        .collect()
}

// Fish-style `abbr` expansion: called right as the keystroke that would
// end the word at the cursor arrives (Space) or would submit the whole
// line (Enter) -- see `Shell::abbrs`'s own doc comment for the storage
// half of this feature. Reuses completion.rs's own word-role walker
// (`find_word_start`/`classify_word_role`) rather than inventing a
// second "what word is this" notion: command position only, matching
// fish's own default (`--position command`) -- an abbreviation typed as
// an *argument* is left alone, same as a real command name would be.
// `true` if an expansion happened (the caller's cue that the buffer
// changed and, for Enter specifically, that this keystroke shouldn't
// also submit the line -- fish's own "first Enter expands, second one
// runs" behavior).
//
// An expansion carrying `%s` placeholders splices in as a live snippet
// instead of as finished text, reported through `snippet` rather than
// through the return value -- which stays a plain `bool` so the two
// trigger sites can keep using it as a match guard (`Key::Enter if
// expand_abbr_at_cursor(..)`), the shape that gives Enter its
// expand-first-submit-second behavior for free.
fn expand_abbr_at_cursor(ed: &mut LineEditor, abbrs: &[Abbr], snippet: &mut Option<LiveSnippet>) -> bool {
    if abbrs.is_empty() {
        return false;
    }
    let word_start = completion::find_word_start(&ed.buf, ed.cursor);
    let prefix_text: String = ed.buf[..word_start].iter().collect();
    if !matches!(completion::classify_word_role(&prefix_text), completion::CmdRole::Command) {
        return false;
    }
    let word: String = ed.buf[word_start..ed.cursor].iter().collect();
    let Some(abbr) = abbrs.iter().find(|a| a.name == word) else {
        return false;
    };
    match Snippet::parse(&abbr.expansion, &abbr.order) {
        Some(snip) => *snippet = Some(LiveSnippet::start(snip, 0, word_start, word, ed)),
        None => ed.splice_word(word_start, ed.cursor, &abbr.expansion),
    }
    true
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
// `search_matches`: (start, end) char-column ranges within `ed.buf` to
// show as reverse-video matches. Empty for every caller except
// run_line_normal_mode (the only context where a vim search, and thus
// something worth highlighting, can be in progress) -- turned into
// compose_redraw's own generic `overlay` layer here rather than being a
// second, parallel mechanism beside it.
fn redraw(prompt: &str, ed: &LineEditor, col_origin: usize, width: usize, ctx: HighlightContext, search_matches: &[(usize, usize)]) -> io::Result<()> {
    let overlay: Vec<StyledSpan> = search_matches
        .iter()
        .map(|&(start, end)| StyledSpan { start, end, fg: vt100::Color::Default, attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() } })
        .collect();
    print!("{}", compose_redraw(prompt, ed, "", col_origin, width, ctx, &overlay));
    io::stdout().flush()
}

// The escape-coded string redraw() prints, split out so
// redraw_with_completion_row can fold it into one larger string of its
// own rather than issuing a separate print!/flush for it -- two
// separate writes left a real, visible window between them where the
// terminal had already rendered the intermediate "moved down, column
// reset" state from the first write before the second one corrected it,
// showing up as the cursor briefly jumping to the start of the row on
// every keystroke. One write, one flush, matches this function's own
// original design intent (build the whole string first, print it once).
//
// `ghost`: a suggestion's full, untruncated dimmed tail, rendered right
// after the real buffer -- empty for every caller except read_line's
// own suggestion support, and only ever non-empty when
// `ed.cursor == ed.buf.len()` (the caller's own gate; not re-asserted
// here). Truncated to fit *here*, not by the caller -- see the
// `ghost_room` computation below.
//
// `overlay`: whatever the caller wants marked up on top of the line --
// a vim search's own matches in reverse video (`hlsearch`, see
// `redraw`), or a live `abbr` snippet's placeholders (see
// `snippet_layer`). Composed as the *last* layer (see
// highlight::compose's own doc comment on later layers winning), so it
// visually wins over both syntax highlighting and a suggestion's ghost
// tail wherever they overlap -- matching vim, where `hlsearch` sits on
// top of everything else. Empty for every caller with nothing to mark.
fn compose_redraw(prompt: &str, ed: &LineEditor, ghost: &str, col_origin: usize, width: usize, ctx: HighlightContext, overlay: &[StyledSpan]) -> String {
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
        return out;
    }

    out.push_str(prompt);
    let remaining = width - prompt_len;

    // Highlight the whole buffer up front, then slice the *resolved*
    // cells for whichever window ends up visible below -- a span that
    // starts before the horizontally-scrolled window and extends into it
    // still renders correctly from column 0 with no separate clipping
    // logic, since slicing happens after per-char style is already
    // resolved (same as how the un-highlighted code already sliced
    // Vec<char>). Recomputed fresh on every redraw, same as the rest of
    // this function -- no caching, matching this feature's own stated
    // out-of-scope list (a single command line's worth of recomputation
    // per keystroke is cheap enough that it isn't worth it).
    let buf_text: String = ed.buf.iter().collect();
    let styled: Vec<StyledSpan> = BashHighlighter
        .highlight(&buf_text, ctx)
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, ctx.color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();

    // `combined` is the real buffer plus as much of the ghost tail as
    // fits in whatever's left after the buffer -- truncated here
    // (rather than trusting the caller to have already done it) since
    // `remaining` is only known once the prompt's own width is
    // accounted for, right here. Zero room (an already-overlong buffer)
    // silently drops the whole ghost rather than wrapping it into a
    // neighboring pane's row -- a deliberate v1 simplification, not a
    // caller contract that could be violated. Styled as two layers over
    // one char slice -- highlight::compose already supports this (a
    // later layer wins per-cell), so the ghost needs no new
    // presentation machinery of its own. With ghost == "" (or zero
    // room) this is byte-for-byte the old ed.buf-only behavior:
    // combined == ed.buf, the ghost layer is empty, nothing changes.
    let ghost_room = remaining.saturating_sub(ed.buf.len());
    let mut combined = ed.buf.clone();
    combined.extend(ghost.chars().take(ghost_room));
    let ghost_layer = if combined.len() == ed.buf.len() {
        Vec::new()
    } else {
        // Same (fg, attrs) pair default_style(HighlightKind::Comment)
        // already uses -- this codebase's existing "greyed out" convention.
        vec![StyledSpan {
            start: ed.buf.len(),
            end: combined.len(),
            fg: vt100::Color::Indexed(8),
            attrs: vt100::CellAttrs { dim: true, ..vt100::CellAttrs::default() },
        }]
    };
    // `overlay` last, so whatever the caller is marking up -- a vim
    // search's own matches, a live `abbr` snippet's placeholders -- wins
    // over the syntax highlighting underneath it, which for a
    // half-finished command line is routinely not even valid syntax yet.
    let cells = highlight::compose(&combined, &[&styled, &ghost_layer, overlay]);

    if combined.len() <= remaining {
        out.push_str(&highlight::render_styled(&cells));
        // Walks the real cursor back past the ghost tail (present or
        // not) to land right after the real buffer, at ed.cursor's own
        // position -- cursor-wise, the ghost doesn't exist. With no
        // ghost this is exactly the old `ed.buf.len() - ed.cursor`.
        let back = combined.len() - ed.cursor;
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    } else {
        // Right-align the visible window on the cursor whenever it
        // would otherwise fall outside what's currently shown --
        // clamped so the window never scrolls past showing the
        // buffer's own tail once the cursor's at or past the end
        // (typing forward, by far the common case). Unreachable with a
        // non-empty ghost in practice (the caller never truncates a
        // ghost to fit only to have combined still overflow), but the
        // math here is unaffected either way since it only ever
        // references ed.buf/ed.cursor, never `combined`.
        let window_start = ed.cursor.saturating_sub(remaining - 1).min(ed.buf.len() - remaining);
        let window_end = (window_start + remaining).min(ed.buf.len());
        out.push_str(&highlight::render_styled(&cells[window_start..window_end]));
        let back = (window_end - window_start) - (ed.cursor - window_start);
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    }

    out
}

// Lays out a completion menu row: every candidate's own `display` joined
// by spaces, a bold StyledSpan per matched character (from each
// candidate's own `matched_positions`), and the selected candidate's
// whole span tracked separately so it can both get a reverse-video
// StyledSpan and anchor the width-clamping window below. Building the
// bold layer and the reverse span as two separate compose() layers
// (rather than merging attrs by hand) means the selected candidate's
// bold matched-character highlighting gets overwritten by its own
// reverse-video span where they overlap -- a minor, accepted
// simplification, since the selected candidate already stands out via
// reverse video on its own.
struct CompletionRow {
    chars: Vec<char>,
    bold: Vec<StyledSpan>,
    selected: Vec<StyledSpan>,
    selected_range: std::ops::Range<usize>,
}

fn build_completion_row(state: &CompletionState) -> CompletionRow {
    let mut text = String::new();
    let mut bold = Vec::new();
    let mut selected_range = 0..0;
    for (i, candidate) in state.candidates.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start = text.chars().count();
        for &pos in &candidate.matched_positions {
            bold.push(StyledSpan {
                start: start + pos,
                end: start + pos + 1,
                fg: vt100::Color::Default,
                attrs: vt100::CellAttrs { bold: true, ..vt100::CellAttrs::default() },
            });
        }
        text.push_str(&candidate.display);
        let end = text.chars().count();
        if i == state.selected {
            selected_range = start..end;
        }
    }
    let selected = vec![StyledSpan {
        start: selected_range.start,
        end: selected_range.end,
        fg: vt100::Color::Default,
        attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() },
    }];
    CompletionRow { chars: text.chars().collect(), bold, selected, selected_range }
}

// Wraps redraw() with exactly one extra row for the completion menu,
// positioned via relative cursor movement (down 1, then back up 1)
// rather than absolute-row tracking or save/restore-cursor: save/
// restore captures an *absolute* coordinate, which goes stale the
// moment a scroll happens between save and restore -- exactly the
// "prompt on the terminal's last row" case this needs to survive.
//
// IMPORTANT, and previously wrong in this comment: relative movement
// does NOT net to zero rows moved regardless of what scrolled in
// between. Verified directly (bypassing this codebase entirely, driving
// a plain terminal with raw escape sequences and tmux's own cursor-
// position introspection as the oracle): when the cursor is genuinely
// on the terminal's true bottom margin, `\n` scrolls the whole screen up
// by one row but leaves the cursor's own absolute row index unchanged
// (it stays on the now-blank bottom row) -- so the content that used to
// be "the current row" is now one row higher than the cursor. The
// following `\x1b[1A` then moves up by exactly one row unconditionally,
// landing exactly on that shifted content and overwriting it -- a real,
// deterministic, single-invocation loss of one row, not a save/restore-
// style staleness issue. This is why `menu_was_shown` below exists:
// since this function used to run on *every* redraw (see its own
// removed claim above), the very first time typing/output reached the
// terminal's true last row would permanently pull the prompt up by one
// row, and it would never be touched again -- exactly the "the
// terminal's true last row is never used" bug this was found while
// fixing. Gating the whole block to only run when a completion menu is
// actually relevant (`completion.is_some() || menu_was_shown`) doesn't
// make a single dance invocation any safer in isolation, but it makes
// the dance rare enough in practice (only during active completion use,
// not on every keystroke) that this residual risk is an accepted,
// documented gap rather than a routine failure. The row is still always
// erased once more after a completion ends (even though `completion` is
// already None by then), clearing a stale row left over from the
// previous call -- `menu_was_shown` is what keeps that one extra redraw
// gated in too. `\x1b[1A` restores the *row* but not the column
// redraw() already left the real cursor at -- the second compose_redraw
// call is the deliberately-simple way to fix that, rather than
// duplicating redraw()'s own column math here.
//
// The "move down" step is a literal `\n`, not `\x1b[1B` (Cursor Down):
// CUD is defined to *clamp* at the terminal's bottom margin rather than
// scroll, unlike a real linefeed, which was silently corrupting the
// *previous* command's own output row (found via interactive testing)
// before this used `\n` instead.
//
// Everything is assembled into one string and written with a single
// print!/flush, not two separate ones -- an earlier version called
// redraw() (its own print!+flush) for the base row, then did a second,
// separate print!+flush for the rest. That left a real, brief window
// after the first flush where the terminal had already rendered the
// intermediate "moved down a row, column reset to col_origin" state
// before the second flush corrected it -- visible as the cursor jumping
// to the start of the row on every single keystroke (found via the
// user's own interactive testing). One write settles directly on the
// final state with nothing intermediate ever rendered.
// The completion menu row's own styled content (candidate names, bold
// matched positions, reverse-video selection), with no positioning of
// its own -- shared by both redraw_with_completion_row's relative
// (plain-mode) and absolute (grid-mode) paths below, which only differ
// in *how* they get the real cursor down to this row and back.
fn render_menu_row_content(state: &CompletionState, width: usize) -> String {
    let row = build_completion_row(state);
    let cells = highlight::compose(&row.chars, &[&row.bold, &row.selected]);
    let visible: &[vt100::Cell] = if row.chars.len() <= width {
        &cells
    } else {
        // Right-anchors the window on the selected candidate's own end,
        // same shape as redraw()'s own cursor-visibility clamp for an
        // overlong buffer -- a long candidate list stays truncated/
        // scrolled to `width` rather than wrapping into a neighboring
        // pane's row.
        let window_start = row.selected_range.end.saturating_sub(width).min(row.chars.len() - width);
        let window_end = (window_start + width).min(row.chars.len());
        &cells[window_start..window_end]
    };
    highlight::render_styled(visible)
}

fn redraw_with_completion_row(
    prompt: &str,
    ed: &LineEditor,
    ghost: &str,
    completion: Option<&CompletionState>,
    // Whether a completion menu was showing on the *previous* redraw --
    // the dance below still has to run once more after a completion ends
    // (Tab-accepted, Ctrl-E-discarded, or "locked in" by an unrelated
    // key) purely to erase that stale row, even though `completion` is
    // already None by then. See this function's own doc comment for why
    // it must NOT run on every other redraw too.
    menu_was_shown: bool,
    menu_capable: bool,
    // `Some((prompt_row, pane_bottom_row))` in grid/promoted mode (an
    // unsplit window only -- see read_line's own doc comment): both
    // absolute, 0-indexed real terminal rows, computed once by repl.rs
    // before this whole read_line call starts (from the session's own
    // vt100::Screen cursor, which tracks correctly independent of the
    // real-terminal scrolling ambiguity relative movement has -- see
    // this function's own doc comment) and constant for its duration,
    // the same way `col_origin`/`width` already are. `pane_bottom_row`
    // is the pane's own last row, never the tab bar's -- when there's no
    // room below `prompt_row` to fit within it, the menu is skipped
    // entirely rather than risking a spill. `None` in plain mode (no
    // fixed rect to know a row against -- relative movement handles it)
    // and for command mode's colon-line (no meaningful pane context).
    row_origin: Option<(usize, usize)>,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    // A live `abbr` snippet's own placeholder marking, empty the rest of
    // the time -- see `snippet_layer`.
    overlay: &[StyledSpan],
) -> io::Result<()> {
    let mut out = compose_redraw(prompt, ed, ghost, col_origin, width, ctx, overlay);
    if menu_capable && (completion.is_some() || menu_was_shown) {
        match row_origin {
            // Grid/promoted mode: absolute positioning only, never
            // relative movement -- a stray real scroll here would desync
            // the real terminal from the session's own virtual grid,
            // corrupting the tab bar or a neighboring pane in the
            // meantime (the same mechanism bug the Ctrl-C fix elsewhere
            // in this codebase addresses for a different trigger).
            // `\x1b[{row};{col}H` jumps are unambiguous regardless of
            // terminal scroll state, so there's no dance needed at all.
            Some((prompt_row, pane_bottom_row)) => {
                let menu_row = prompt_row + 1;
                if menu_row <= pane_bottom_row {
                    out.push_str(&format!("\x1b[{};{}H", menu_row + 1, col_origin + 1));
                    out.push_str(&" ".repeat(width));
                    out.push_str(&format!("\x1b[{};{}H", menu_row + 1, col_origin + 1));
                    if let Some(state) = completion {
                        out.push_str(&render_menu_row_content(state, width));
                    }
                    // Absolute jump back, not \x1b[1A -- fixes both row
                    // and column in one unambiguous move.
                    out.push_str(&format!("\x1b[{};{}H", prompt_row + 1, col_origin + 1));
                    out.push_str(&compose_redraw(prompt, ed, ghost, col_origin, width, ctx, overlay));
                }
                // else: no room below the prompt within this pane --
                // gracefully skip showing the menu this redraw, same
                // degradation plain mode already has when `menu_capable`
                // is false.
            }
            // Plain mode: the relative-movement dance.
            None => {
                out.push('\n');
                out.push_str(&format!("\x1b[{}G", col_origin + 1));
                out.push_str(&" ".repeat(width));
                out.push_str(&format!("\x1b[{}G", col_origin + 1));
                if let Some(state) = completion {
                    out.push_str(&render_menu_row_content(state, width));
                }
                out.push_str("\x1b[1A");
                out.push_str(&compose_redraw(prompt, ed, ghost, col_origin, width, ctx, overlay));
            }
        }
    }

    print!("{}", out);
    io::stdout().flush()
}

// How many terminal columns `s` actually occupies once drawn, not
// counting invisible escape bytes -- `s` is always one of this crate's
// own prompt strings, which only ever embed `\x1b[...m` SGR (color)
// codes, so that's the only escape form this needs to recognize. Each
// visible char counts for its own real display width (bishedit::
// unicode_width::char_width), not a flat 1 -- the cwd embedded in the
// default prompt is real, user-controlled path text, wide CJK
// characters/emoji included, not just ASCII.
// pub(crate): repl.rs's own freeze-with-text helper (Ctrl+Space with
// in-progress text) reuses this to know how many *visible* columns a
// colored prompt occupies, so it can position the frozen row's
// ScreenBuffer cursor at the right column rather than guessing.
//
// Grapheme-cluster-aware for the *visible* text (a ZWJ emoji sequence
// counts once, not once per codepoint -- see bishedit::unicode_width::
// str_width's own doc comment for the exact same fix and why): SGR
// escapes are stripped first into a plain char buffer, then walked
// cluster by cluster via bishedit::grapheme::next_boundary, since that
// function needs an indexable `&[char]` slice (grapheme boundaries
// GB11/GB12/13 look several codepoints backward) that a single-escape-
// lookahead streaming scan can't provide directly.
pub(crate) fn visible_len(s: &str) -> usize {
    let mut visible_chars: Vec<char> = Vec::with_capacity(s.len());
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
        visible_chars.push(c);
    }
    let mut len = 0;
    let mut i = 0;
    while i < visible_chars.len() {
        len += char_width(visible_chars[i]);
        i = crate::bishedit::grapheme::next_boundary(&visible_chars, i);
    }
    len
}

// Like visible_len, but returns the prefix of `s` whose *visible*
// portion is at most `max_visible` columns, preserving any embedded SGR
// codes encountered along the way (they don't count against the
// budget) rather than risking a mid-escape-sequence cut.
//
// Unlike visible_len, *not* grapheme-cluster-aware yet: this streams
// char-by-char with only one-char lookahead (for the escape-sequence
// check), so a truncation point could in principle land mid-cluster --
// e.g. cutting a ZWJ emoji sequence in half. A real, deliberately
// deferred gap (this function would need the same escapes-stripped-
// into-a-Vec<char>-then-walked-in-clusters restructuring visible_len
// just got, plus stitching the original escapes back into the
// truncated result at the right points): this only ever triggers when
// a pane is too narrow even for the prompt alone to fit -- see this
// function's own next comment -- rare enough, and degrading to "one
// glyph looks broken in an already-degenerate layout" rather than a
// crash or stuck cursor, that it's not worth the complexity here yet.
// A wide char
// that would only *partially* fit (`max_visible` has exactly 1 column
// left, the next char is 2 columns wide) is dropped whole rather than
// split -- there's no such thing as half a CJK glyph, and this only
// ever feeds into padding/truncation math that tolerates landing one
// column short of the budget, never one over it.
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
        let w = char_width(c);
        if visible + w > max_visible {
            break;
        }
        out.push(c);
        visible += w;
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
    // Ctrl+Space -- unconditional now, regardless of what's been typed
    // (previously gated to an empty buffer only, the same "don't discard
    // in-progress typing" reasoning DirNav above still uses). Enters
    // bishedit's normal-mode navigation over the current pane's own
    // rendered content (repl.rs's run_normal_mode_navigation) -- the
    // *only* way into command mode now (via normal mode's own ':',
    // matching real vim) since the old direct ':'-at-the-shell-prompt
    // shortcut was retired. `text`/`cursor` are `ed.as_string()`/
    // `ed.cursor` at the moment Ctrl+Space was pressed, so the caller can
    // both show what's already been typed in its pane view and hand it
    // back (via `read_line`'s own `initial` parameter) to whatever
    // `read_line` call eventually resumes editing -- nothing is silently
    // discarded now that this isn't empty-buffer-only anymore. `i`/`a`/
    // `I`/`A`/`s`/`S`/`C` in normal mode all act relative to this
    // *original* cursor (not wherever normal mode's own navigation cursor
    // wanders off to -- see run_normal_mode_navigation's own doc comment).
    NormalMode { text: String, cursor: usize },
    // A qualifying left click (see MouseEvent::is_left_click) anywhere
    // during ordinary typing. Bubbled up the same way NormalMode is --
    // read_line has no idea `windows`/panes exist at all, only its own
    // pane's col_origin/width, so hit-testing which tab/pane the click
    // actually landed on is entirely the caller's job (repl.rs's own
    // hit_test_click). `text`/`cursor` are `ed.as_string()`/`ed.cursor`
    // at the moment of the click, same reasoning as NormalMode's own
    // fields: whatever was typed so far must not be silently discarded,
    // even though (unlike NormalMode) resuming into the *same* buffer
    // isn't generally what happens next -- see this outcome's own repl.rs
    // handler for why a genuine focus change instead freezes this text
    // into the losing pane's own grid.
    Mouse { event: MouseEvent, text: String, cursor: usize },
    // Ctrl-L, but only when the caller opted in via `ctrl_l_reports` (see
    // that parameter's own doc comment) -- command mode's own toggle for
    // showing its whole command+output transcript. Whatever was typed so
    // far is discarded, same simplification DirNav/NormalMode already
    // make for their own callers with nothing meaningful to preserve
    // across the jump.
    CtrlL,
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
// `ctrl_l_reports`: whether Ctrl-L is reported as `ReadOutcome::CtrlL`
// instead of this crate's usual "clear the real screen" handling. Off
// for the normal shell prompt (Ctrl-L there keeps meaning "clear
// screen," the readline/bash convention); on for command mode, which
// gives Ctrl-L its own meaning (toggling the command+output transcript
// view -- see repl.rs's run_command_mode).
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
// `initial`: text + cursor to preload the buffer with instead of starting
// empty -- used to resume editing after a Ctrl+Space excursion into
// normal-mode navigation (see `ReadOutcome::NormalMode`'s own doc
// comment) without losing whatever had already been typed. `None` for
// every ordinary call (a fresh prompt has nothing to preload).
pub fn read_line(
    prompt: &str,
    history: &History,
    esc_cancels: bool,
    ctrl_l_reports: bool,
    initial: Option<(String, usize)>,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    // `None` for callers with no meaningful shell context to complete
    // against (command mode's own colon-line -- same clean layering
    // HighlightContext::default() already established there). `menu_capable`
    // is unused until a later stage wires up the actual menu row; carried
    // as a parameter now so both repl.rs call sites only need to change
    // their argument list once.
    completion_provider: Option<&dyn CompletionProvider>,
    // Same "`None` for no meaningful shell context" pattern as
    // `completion_provider` just above -- command mode's colon-line
    // passes `None` here too.
    suggestion_provider: Option<&dyn SuggestionProvider>,
    menu_capable: bool,
    // See redraw_with_completion_row's own doc comment on this same
    // parameter -- threaded straight through, read_line itself never
    // interprets it beyond passing it along to every redraw call.
    row_origin: Option<(usize, usize)>,
    // The whole-shell register table (see bishedit::registers::Registers'
    // own doc comment) -- one instance, shared globally, threaded through
    // here for Ctrl-R (below) and passed further into run_line_normal_mode/
    // run_one_shot_normal_command for y/p in Ctrl-E's/Ctrl-O's own vim
    // Normal mode.
    registers: &mut Registers,
    // Fish-style abbreviations, snapshotted fresh per prompt by the
    // caller -- same "owned snapshot, not a live borrow" pattern as
    // `completion_provider`'s own `cwd`/`known_functions` (see
    // `Shell::abbrs`'s own doc comment). Empty for callers with no
    // meaningful shell context (command mode's colon-line, matching
    // `completion_provider`/`suggestion_provider`'s own `None` there) --
    // `expand_abbr_at_cursor` is a no-op on an empty slice either way.
    abbrs: &[Abbr],
    // `(term_rows, term_cols)`, `Some` when Ctrl-E's own line-local
    // Normal mode (run_line_normal_mode, below) should draw its live
    // `/`/`?` search input at the terminal's shared global status row
    // (`repl::render_global_status_row`/`command_mode_row` -- the same
    // row `:` command mode and Ctrl+Space's own Normal-mode status line
    // already use) instead of substituting it in place of the prompt --
    // both dimensions are needed since that row spans the *terminal's*
    // full width, not this call's own `col_origin`/`width` (which, for a
    // split pane, is narrower). `None`: falls back to the original
    // in-place substitution -- command mode's own colon-line already
    // occupies that exact row with its own prompt (see that call site),
    // so a Ctrl-E search started while composing a `:` command keeps the
    // old behavior rather than fighting over it. A plain snapshot, not a
    // live reference: `on_idle` below can itself resize the terminal
    // (SIGWINCH), and a value this stale by at most one resize is a fine
    // trade against needing `on_idle` and this to somehow share `&mut`
    // access to the same term size.
    global_row_size: Option<(usize, usize)>,
    mut on_idle: impl FnMut(),
) -> io::Result<ReadOutcome> {
    let mut guard = Some(term::RawGuard::enable_with_mouse(0)?);
    let mut ed = match initial {
        Some((text, cursor)) => {
            let mut e = LineEditor::new();
            e.buf = text.chars().collect();
            e.cursor = cursor.min(e.buf.len());
            e
        }
        None => LineEditor::new(),
    };

    // `u`/`Ctrl-R`/`g-`/`g+` for this line's own Ctrl-E (`run_line_normal_
    // mode`) and Ctrl-O (`run_one_shot_normal_command`) vim Normal-mode
    // excursions -- shared across *both* (and across however many
    // separate excursions of either kind happen while composing this one
    // line), so a `<C-o>dd` is undoable via a later Ctrl-E `u` just like
    // an edit made from inside Ctrl-E itself. Fresh per `read_line` call
    // (never persisted past submitting/cancelling this one line), unlike
    // `marks`/`selections` below, which stay scoped to one excursion each
    // -- those two are what a *motion* (`` `a ``, a Visual selection)
    // resolves against, meaningful only within the excursion that set
    // them; undo history is meaningful for as long as the line itself
    // exists.
    let mut undo: UndoTree<Vec<char>> = UndoTree::new(ed.buf.clone(), (0, ed.cursor));

    // Fish-style history browsing: Up/Down search backward/forward through
    // history for entries starting with whatever was typed *before*
    // browsing started (that original text is `prefix`, restored on Esc
    // or on Down-ing past the newest match). Any other key -- moving the
    // cursor, editing, submitting -- silently "locks in" the currently
    // shown entry as ordinary buffer text and ends the browse, matching
    // fish: the suggestion just becomes your input from that point on.
    let mut browse: Option<(String, usize)> = None;

    // In-progress completion state -- see CompletionState's own doc
    // comment. `None` means either nothing's being completed, or the last
    // trigger auto-filled a single candidate directly (no tracking
    // needed for that case).
    let mut completion: Option<CompletionState> = None;

    // The `abbr` snippet currently spliced into the line, if any -- see
    // bishedit::snippet::LiveSnippet. Mutually exclusive with `completion` in practice
    // (nothing can start one while the other is live), but nothing
    // depends on that beyond the redraw only ever having one extra layer
    // to draw.
    let mut snippet: Option<LiveSnippet> = None;

    // Whether a completion menu was showing on the immediately-preceding
    // redraw -- see redraw_with_completion_row's own doc comment for why
    // this exists (its down/erase/up dance must run once more after a
    // completion ends purely to erase the stale row, but must NOT run on
    // every other redraw, which is what it used to do and is exactly
    // what made the terminal's true last row permanently unreachable).
    // Starts false: nothing has been drawn yet, so there's no stale row
    // from a previous call to worry about.
    let mut menu_was_shown = false;

    // The ghost tail currently being shown, if any -- see
    // compute_suggestion's own doc comment for why this is a plain
    // recomputed-every-redraw cache, not a persisted state machine.
    let mut suggestion = compute_suggestion(&ed, suggestion_provider, completion.is_some(), browse.is_some());

    // Always goes through the same clear-and-draw redraw(), even with
    // nothing typed yet: the terminal cursor at this point may already be
    // sitting right after a compositor-frozen idle prompt for this exact
    // pane (see repl.rs's freeze_idle_prompt) -- a bare, non-clearing
    // print here would append a second copy right next to it instead of
    // redrawing over it. Behaviorally identical to the old bare print for
    // every non-paned caller (col_origin 0, width the whole terminal).
    redraw_with_completion_row(
        prompt,
        &ed,
        suggestion.as_deref().unwrap_or(""),
        completion.as_ref(),
        menu_was_shown,
        menu_capable,
        row_origin,
        col_origin,
        width,
        ctx,
        &[],
    )?;
    menu_was_shown = completion.is_some();

    // Set only when Ctrl-E's or Ctrl-O's own sub-loop below reports a key
    // it didn't consume itself (Ctrl-C/D/Z -- see run_line_normal_mode/
    // run_one_shot_normal_command's own doc comments) -- reprocessed here
    // through the exact same match arms that would have handled it had it
    // been read directly, rather than duplicating that handling.
    let mut pending_key: Option<Key> = None;

    loop {
        let key = match pending_key.take() {
            Some(k) => k,
            None => match read_key_idle(&mut on_idle)? {
                Some(k) => k,
                None => {
                    drop(guard.take());
                    return Ok(ReadOutcome::Eof);
                }
            },
        };
        if !matches!(key, Key::Up | Key::Down | Key::Escape) {
            browse = None;
        }
        // Mirrors the browse reset immediately above: any key that isn't
        // part of cycling/accepting/discarding a completion "locks in"
        // whatever's currently spliced into the buffer as ordinary text
        // and ends tracking -- the buffer is already correct, so this is
        // the entire implementation of "keep typing accepts" (Char,
        // Backspace, Enter, etc. all fall through to their own existing,
        // untouched arms right after this). Ctrl-Y isn't in the exemption
        // list either, so "accept" falls out for free with no dedicated
        // arm of its own.
        if completion.is_some() && !matches!(key, Key::Tab | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::CtrlE | Key::Up | Key::Down) {
            completion = None;
        }
        // The same rule one tier over, for a live snippet: any key
        // outside its own small vocabulary (below) accepts it as it
        // stands and then means whatever it always meant. So a Left
        // arrow, a Ctrl-A, or a history recall doesn't have to grow a
        // snippet-aware case of its own -- it just finds ordinary text
        // where the placeholders were.
        if snippet.is_some()
            && !matches!(
                key,
                Key::Tab | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::Enter | Key::CtrlY | Key::CtrlE | Key::Backspace | Key::Char(_)
            )
        {
            snippet.take().unwrap().accept(&mut ed);
        }

        match key {
            // --- a live `abbr` snippet owns these eight keys ---------
            // Tab/Shift-Tab and Ctrl-N/Ctrl-P step between placeholders,
            // the same two pairs that cycle a completion menu -- there's
            // never both at once (see the `snippet` local's own comment),
            // so the overload costs nothing.
            Key::Tab | Key::CtrlN if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                state.snip.advance(false);
                state.sync(&mut ed);
            }
            Key::BackTab | Key::CtrlP if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                state.snip.advance(true);
                state.sync(&mut ed);
            }
            // Enter advances like Tab, except on the last placeholder in
            // visit order, where there's nothing left to advance *to* and
            // it accepts instead. Accepting does not also submit: the
            // line is left there to look over, exactly as a plain
            // abbreviation's own first Enter leaves its expansion.
            Key::Enter if snippet.as_ref().is_some_and(|s| !s.snip.at_last()) => {
                let state = snippet.as_mut().unwrap();
                state.snip.advance(false);
                state.sync(&mut ed);
            }
            Key::Enter | Key::CtrlY if snippet.is_some() => {
                snippet.take().unwrap().accept(&mut ed);
            }
            // Ctrl-E discards the whole thing and puts the line back the
            // way it was, abbreviation name and all -- the same key, and
            // the same meaning, a live completion already gives it.
            Key::CtrlE if snippet.is_some() => {
                snippet.take().unwrap().cancel(&mut ed);
            }
            // Backspace only ever eats what was typed into the active
            // placeholder. With nothing left in it, it stops there rather
            // than chewing into the snippet's own literal text -- which
            // is not something the model could put back.
            Key::Backspace if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                if state.snip.backspace() {
                    state.sync(&mut ed);
                }
            }
            // Typing (space included -- a placeholder standing in for
            // `-m "%s"`'s message wants spaces) fills the active
            // placeholder, replacing the `%s` shown there.
            Key::Char(c) if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                state.snip.type_char(c);
                state.sync(&mut ed);
            }

            // Fish's own "first Enter expands, second one runs": if the
            // word right at the cursor is a command-position abbreviation,
            // this Enter only expands it and falls through to the ordinary
            // post-match redraw below instead of submitting -- the same
            // way a real terminal expects to see its own expansion before
            // running it, not run the un-expanded short form. A second,
            // immediate Enter then finds nothing left to expand and
            // submits normally.
            Key::Enter if expand_abbr_at_cursor(&mut ed, abbrs, &mut snippet) => {}
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
                guard = Some(term::RawGuard::enable_with_mouse(0)?);
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
            // Right accepts the currently-shown suggestion, turning its
            // ghost tail into real buffer text. `suggestion` is only
            // ever Some with the cursor already at ed.buf.len(), where
            // the move_right() below would have been a no-op anyway --
            // so this is a strict refinement, never shadowing a real
            // movement. Ctrl-F is deliberately excluded: the request
            // names the right arrow specifically, and Ctrl-F stays a
            // pure motion with no acceptance meaning of its own.
            Key::Right if suggestion.is_some() => accept_suggestion(&mut ed, suggestion.as_deref().unwrap()),
            Key::Left | Key::CtrlB => ed.move_left(),
            Key::Right | Key::CtrlF => ed.move_right(),
            Key::Home | Key::CtrlA => ed.cursor = 0,
            Key::End => ed.cursor = ed.buf.len(),
            Key::CtrlK => ed.kill_to_end(),
            Key::CtrlU => ed.kill_to_start(),
            Key::CtrlW => ed.kill_word_backward(),
            // Vim's insert-mode <C-r>{register}: reads exactly one more
            // key as the register name and splices that register's
            // contents in at the cursor, literally (not as a "ghost" the
            // way a suggestion is -- it's real buffer text immediately).
            // Anything other than a plain char for the register name
            // (Escape, Ctrl-C, ...) is a no-op, matching vim's own "Esc
            // cancels a pending <C-r>" -- the next read_key_idle call
            // simply reprocesses whatever that key would have meant on
            // its own, same as Ctrl-E/Ctrl-O's own propagated-key pattern
            // just below, except here there's nothing worth propagating
            // (an aborted <C-r> is just... aborted).
            Key::CtrlR => {
                if let Some(Key::Char(c)) = read_key_idle(&mut on_idle)? {
                    let text = registers.read(Some(c)).flatten_to_single_line();
                    for ch in text.chars() {
                        ed.insert(ch);
                    }
                }
            }
            Key::CtrlL if ctrl_l_reports => {
                drop(guard.take());
                return Ok(ReadOutcome::CtrlL);
            }
            Key::CtrlL => print!("\x1b[H\x1b[2J"),
            // Space is the other abbr expansion trigger, alongside Enter
            // above -- unlike Enter, a space is never withheld: the
            // expansion (if any) lands first, then the space that
            // triggered it is still inserted right after, matching fish
            // ("gco" + Space becomes "git checkout " -- expansion plus the
            // space that ended it, not one or the other).
            Key::Char(' ') => {
                // A snippet swallows the space that triggered it: the
                // caret is already parked inside the first placeholder,
                // and a space there would be the first thing typed *into*
                // it -- never what "end the abbreviation" meant. A plain
                // expansion still gets it, unchanged.
                expand_abbr_at_cursor(&mut ed, abbrs, &mut snippet);
                if snippet.is_none() {
                    ed.insert(' ');
                }
            }
            Key::Char(c) => ed.insert(c),
            // Once a completion menu is active, Up/Down cycle it instead
            // of history browsing -- the plan's own "or up and down
            // arrows if the completion menu is already open."
            Key::Up if completion.is_some() => cycle_completion(&mut ed, completion.as_mut().unwrap(), true),
            Key::Down if completion.is_some() => cycle_completion(&mut ed, completion.as_mut().unwrap(), false),
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
            //
            // No `\r\n` before returning (used to print one, moving to a
            // fresh line every time) -- the caller (repl.rs) updates the
            // session's cwd and loops back into a brand new `read_line`
            // call with a freshly recomputed prompt string, and that
            // call's own very first paint already goes through the same
            // clear-and-redraw `redraw_with_completion_row` every
            // keystroke uses (see its own call site's doc comment: "the
            // terminal cursor at this point may already be sitting right
            // after a ... prompt for this exact pane"). Printing a
            // newline here defeated that -- it left the cursor one row
            // below where the redraw would land, so instead of updating
            // the prompt in place, every Alt-Left/Right/Up push a new,
            // separate prompt line onto the screen instead.
            Key::AltLeft if ed.buf.is_empty() => {
                drop(guard.take());
                return Ok(ReadOutcome::DirNav(DirNav::Back));
            }
            Key::AltRight if ed.buf.is_empty() => {
                drop(guard.take());
                return Ok(ReadOutcome::DirNav(DirNav::Forward));
            }
            Key::AltUp if ed.buf.is_empty() => {
                drop(guard.take());
                return Ok(ReadOutcome::DirNav(DirNav::Up));
            }
            // No trailing "\r\n" here, unlike DirNav above: entering
            // normal mode redraws the pane's own rectangle in place (see
            // repl.rs's run_normal_mode_navigation), not a fresh line at
            // the real terminal's current cursor position. Unconditional
            // now (used to be empty-buffer-only, matching DirNav above) --
            // see ReadOutcome::NormalMode's own doc comment for why that
            // gating is gone.
            Key::CtrlSpace => {
                drop(guard.take());
                return Ok(ReadOutcome::NormalMode { text: ed.as_string(), cursor: ed.cursor });
            }
            // Ctrl-E: a line-local vim Normal mode, always available
            // (empty buffer or not) -- see run_line_normal_mode's own doc
            // comment. Reassigned from this key's previous "move cursor to
            // end of line" meaning (still reachable via the plain `End`
            // key, just no longer double-bound).
            // While a completion is active, Ctrl-E discards it (reverts
            // to the original word) instead of its usual meaning --
            // overloading it this way is deliberate per the feature
            // request. Otherwise unchanged: vim's line-local Normal mode.
            Key::CtrlE if completion.is_some() => {
                let state = completion.take().unwrap();
                ed.splice_word(state.word_start, ed.cursor, &state.original);
            }
            Key::CtrlE => match run_line_normal_mode(&mut ed, prompt, col_origin, width, ctx, registers, &mut undo, global_row_size, &mut on_idle)? {
                LineNormalExit::ToInsert => {}
                LineNormalExit::Propagate(k) => {
                    pending_key = Some(k);
                    // Neither propagated key (Ctrl-C/D/Z) reads
                    // `suggestion`, so this is inert today -- kept
                    // anyway so "the local always matches what's
                    // actually on screen" holds by construction rather
                    // than by which keys happen to propagate.
                    suggestion = None;
                    continue;
                }
                LineNormalExit::Eof => {
                    drop(guard.take());
                    return Ok(ReadOutcome::Eof);
                }
            },
            // Ctrl-O: vim's insert-mode "do exactly one normal command,
            // then return" -- see run_one_shot_normal_command's own doc
            // comment. Reachable directly from ordinary typing, matching
            // real vim -- no need to already be in Ctrl-E's own mode first.
            Key::CtrlO => {
                if let Some(k) = run_one_shot_normal_command(&mut ed, registers, &mut undo, &mut on_idle)? {
                    pending_key = Some(k);
                    suggestion = None; // see Ctrl-E's own propagate arm
                    continue;
                }
            }
            // Tab/Ctrl-N cycle forward (or trigger a fresh completion if
            // none is active); Shift-Tab/Ctrl-P cycle backward. Ctrl-Y
            // needs no arm of its own -- it's not in the reset
            // exemption list above, so "accept" already happened by the
            // time any match arm would run.
            Key::Tab | Key::CtrlN => match &mut completion {
                Some(state) => cycle_completion(&mut ed, state, false),
                None => completion = completion_provider.and_then(|p| trigger_completion(&mut ed, p, false)),
            },
            Key::BackTab | Key::CtrlP => match &mut completion {
                Some(state) => cycle_completion(&mut ed, state, true),
                None => completion = completion_provider.and_then(|p| trigger_completion(&mut ed, p, true)),
            },
            // Ctrl-Y accepts a suggestion, the same gesture completions
            // already use for accept. Needs no end-of-line reasoning of
            // its own -- `suggestion` is never Some anywhere else. Can't
            // collide with completion's own Ctrl-Y accept (not a match
            // arm at all -- see the reset rule above): a suggestion is
            // only ever computed while completion.is_none(), so at most
            // one of the two is ever live at once.
            Key::CtrlY if suggestion.is_some() => accept_suggestion(&mut ed, suggestion.as_deref().unwrap()),
            // Ctrl-X: no shell line-editing use (see the `Key` variant's
            // own doc comment) -- Normal mode's own `Ctrl-A`/`Ctrl-X`
            // (number increment/decrement) is reached via Ctrl-E's own
            // vim excursion, same as every other Normal-mode-only key.
            // See ReadOutcome::Mouse's own doc comment. Non-qualifying
            // mouse events (drags, releases, wheel, other buttons) fall
            // through to the catch-all just below, exactly as before --
            // safely decoded, silently ignored.
            Key::Mouse(ev) if ev.is_left_click() => {
                drop(guard.take());
                return Ok(ReadOutcome::Mouse { event: ev, text: ed.as_string(), cursor: ed.cursor });
            }
            // PageUp/PageDown: no meaning at the live prompt itself (no
            // scrollback view to page through here -- that's Ctrl+Space's
            // own Normal-mode navigation's job), same as a real bash
            // readline prompt not binding them either.
            Key::AltLeft | Key::AltRight | Key::AltUp | Key::CtrlY | Key::CtrlX | Key::Mouse(_) | Key::PageUp | Key::PageDown | Key::Unknown => {}
        }
        // Recomputed fresh every iteration -- see compute_suggestion's
        // own doc comment. Must happen after the match (so it reflects
        // whatever the just-dispatched key actually did to the buffer/
        // completion/browse state) and before the redraw that shows it.
        suggestion = compute_suggestion(&ed, suggestion_provider, completion.is_some() || snippet.is_some(), browse.is_some());
        let overlay = snippet.as_ref().map(snippet_layer).unwrap_or_default();
        redraw_with_completion_row(
            prompt,
            &ed,
            suggestion.as_deref().unwrap_or(""),
            completion.as_ref(),
            menu_was_shown,
            menu_capable,
            row_origin,
            col_origin,
            width,
            ctx,
            &overlay,
        )?;
        menu_was_shown = completion.is_some();
    }
}

// What Ctrl-E's own line-local Normal mode (run_line_normal_mode) ended
// with.
enum LineNormalExit {
    /// A motion/insert-entry command resolved -- back to ordinary insert
    /// typing, no key left over to reprocess.
    ToInsert,
    /// Ctrl-C/D/Z -- not handled here (this function only knows vim
    /// motions/insert-entry, not "interrupt the whole read"), handed back
    /// to the caller to process exactly as it would have if read directly
    /// at the top of its own loop.
    Propagate(Key),
    /// `read_key_idle` returned `None` (stdin closed) mid-excursion.
    Eof,
}

// What run_line_normal_mode's own redraw call should show for its prompt,
// and which char-columns of `ed.buf` to highlight (reverse video) for the
// active search pattern -- computed together since both derive from the
// same `vk.pending_display()` check, and Ctrl-E has no separate status
// row to show search feedback on the way `normal_mode_status_left`/
// `render_normal_mode_frame` do in repl.rs's own full-pane Normal mode,
// so a search in progress here replaces the prompt text outright instead
// (plain, not reverse-video -- distinguishes it from the ordinary mode
// indicator, matching how the Ctrl+Space status bar already shows it).
//
// "Active" pattern, one rule for both the prompt text and the
// highlighting: the in-progress `/`/`?` text while one is being typed
// (incsearch-style live feedback for free), else the last resolved
// search's own pattern, else nothing to highlight (and the ordinary
// `decorated_prompt` to show). For a word-based last search (`*`/`#`),
// there's no separately-stored pattern text -- `VimKeys::last_search_
// text`'s own doc comment explains why `motion::word_under_cursor` at
// the buffer's current cursor reliably recovers it instead.
// Visual mode's own active (not yet committed via `Z`) selection, if any
// -- `vk`'s own anchor ordered against `lb`'s current cursor into a
// `MotionRange` ready for `extract_text`/rendering/deletion. Mirrors
// repl.rs's own `active_visual_range` exactly (see its doc comment for
// why `Char` maps to `Inclusive`, `Line` to `Linewise`) -- duplicated
// rather than shared since the two drive different `Buffer` impls
// (`ScreenBuffer` there, `LineBuffer` here) and it's a handful of lines
// either way.
fn active_visual_range_line(vk: &VimKeys, lb: &LineBuffer) -> Option<motion::MotionRange> {
    let (shape, anchor) = vk.visual_anchor()?;
    let cursor = lb.cursor();
    let motion_shape = if shape == RegisterShape::Line { motion::MotionShape::Linewise } else { motion::MotionShape::Inclusive };
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some(motion::MotionRange { shape: motion_shape, from, to })
}

// The (start, end) char-column range `range` covers -- always the whole
// single line this buffer ever has, so unlike repl.rs's own
// `selection_columns_in_line` (which has to handle a range spanning
// several real lines) this never needs a `None`/per-line case: `Linewise`
// is the whole buffer (same flattening `yy`/`dd`/`cc` already use for a
// single-line buffer), `Inclusive` is just `to.1 + 1` clamped to `len`.
fn selection_columns_line(range: &motion::MotionRange, len: usize) -> (usize, usize) {
    if range.shape == motion::MotionShape::Linewise {
        return (0, len);
    }
    (range.from.1, (range.to.1 + 1).min(len))
}

// Returns the live `/`/`?` pattern while one's being typed (`None`
// otherwise -- a resolved/no search yields no bar text, only inline
// match highlighting) alongside the match ranges to highlight on the
// command line itself either way. Used to be a single dual-purpose
// "what to show on the prompt row" string, back when a search always
// substituted in place of the prompt; now split apart, since
// run_line_normal_mode only does that as a fallback when it has nowhere
// global to draw the pattern into instead (see that function's own doc
// comment on its `term_rows` param).
fn normal_mode_prompt_and_matches(
    ed: &mut LineEditor,
    marks: &mut HashMap<char, (usize, usize)>,
    selections: &mut Vec<motion::MotionRange>,
    vk: &VimKeys,
) -> (Option<String>, Vec<(usize, usize)>) {
    let pending = vk.pending_display();
    if let Some(rest) = pending.strip_prefix('/').or_else(|| pending.strip_prefix('?')) {
        let lb = LineBuffer { ed, marks, selections };
        let mut matches = if rest.is_empty() { Vec::new() } else { motion::find_matches_in_line(&lb, 0, rest) };
        push_selection_matches(&lb, vk, &mut matches);
        return (Some(pending.to_string()), matches);
    }
    let lb = LineBuffer { ed, marks, selections };
    let pattern = if vk.last_search_is_word() {
        motion::word_under_cursor(&lb, lb.cursor())
    } else {
        let text = vk.last_search_text();
        if text.is_empty() { None } else { Some(text.to_string()) }
    };
    let mut matches = match pattern {
        Some(p) => motion::find_matches_in_line(&lb, 0, &p),
        None => Vec::new(),
    };
    push_selection_matches(&lb, vk, &mut matches);
    (None, matches)
}

// Extends `matches` (search-match column ranges, rendered reverse-video
// -- see `redraw`'s own `search_matches` doc comment) with every
// committed selection plus the active one, if any -- Visual mode's own
// highlighting piggybacks on exactly the same rendering, matching
// repl.rs's own `render_normal_mode_frame`.
fn push_selection_matches(lb: &LineBuffer, vk: &VimKeys, matches: &mut Vec<(usize, usize)>) {
    let len = lb.ed.buf.len();
    for range in lb.selections.iter().chain(active_visual_range_line(vk, lb).iter()) {
        matches.push(selection_columns_line(range, len));
    }
}

// Draws (`Some`) or erases (`None`) run_line_normal_mode's own live
// search-pattern display at the shared global status row -- a no-op
// when `global_row_size` itself is `None` (nothing global to draw
// into, see that function's own doc comment on the param). Wrapped in
// cursor save/restore (`\x1b7`/`\x1b8`): this writes to a row far from
// wherever the just-completed `redraw` call left the real cursor, and
// must leave it exactly where `redraw` put it, not where this leaves
// off.
fn draw_global_search_bar(global_row_size: Option<(usize, usize)>, bar_text: Option<&str>) {
    let Some((term_rows, term_cols)) = global_row_size else { return };
    let row = match bar_text {
        Some(text) => render_global_status_row(&pad_to_width(text, term_cols), term_rows),
        None => erase_global_status_row(term_rows),
    };
    print!("\x1b7{}\x1b8", row);
    let _ = io::stdout().flush();
}

// Space-pads or truncates `text` to exactly `cols` display columns --
// `render_global_status_row`'s own doc comment requires this of its
// caller. Same small, duplicated-rather-than-shared shape as repl.rs's
// own `normal_mode_status_text`/fileeditor.rs's own `status_text`, the
// two other places that build this exact row's contents.
fn pad_to_width(text: &str, cols: usize) -> String {
    let len = text.chars().count();
    match len.cmp(&cols) {
        std::cmp::Ordering::Less => format!("{}{}", text, " ".repeat(cols - len)),
        std::cmp::Ordering::Greater => text.chars().take(cols).collect(),
        std::cmp::Ordering::Equal => text.to_string(),
    }
}

// Ctrl-E: a lightweight, line-local vim Normal mode over the buffer
// currently being typed -- no promotion, no pane/scrollback, works
// identically whether the terminal is split or not (unlike repl.rs's
// full-pane Ctrl+Space mode). Fully vim-authentic: motions and
// insert-entry commands (`i`/`a`/`I`/`A`/`s`/`S`/`C`) both act on the
// *live*, currently-navigated cursor, since this is a tight single-line
// loop with immediate rendering -- there's no "look around elsewhere,
// resume later" excursion the way Ctrl+Space's full-pane mode has (see
// that mode's own doc comment in repl.rs for why *it* instead resolves
// insert-entry against a frozen original cursor). Ctrl-E itself is *not*
// special-cased here as a "toggle back to insert" -- only entering this
// mode from ordinary typing changed meaning; once inside, Ctrl-E keeps
// its real vim Normal-mode meaning (`Motion::ScrollLineDown`, a no-op on
// a single line but still fed through `vk.feed` like any other key,
// unchanged). Real vim only ever returns to Insert via `i`/`a`/`I`/`A`/
// `s`/`S`/`C` (or an explicit Escape-equivalent), never via Ctrl-E, so
// this doesn't invent a binding vim itself doesn't have.
#[allow(clippy::too_many_arguments)]
fn run_line_normal_mode(
    ed: &mut LineEditor,
    prompt: &str,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    registers: &mut Registers,
    undo: &mut UndoTree<Vec<char>>,
    // `(term_rows, term_cols)`: `Some` draws a `/`/`?` pattern being
    // typed at the terminal's shared global status row
    // (`repl::render_global_status_row`, `repl::erase_global_status_row`
    // to blank it back out -- the same row `:` command mode and Ctrl+
    // Space's own Normal-mode status line already use) instead of
    // substituting it in place of this excursion's own reverse-video
    // prompt. Threaded straight through from `read_line`'s own
    // identically-named param -- see its doc comment for why both
    // dimensions are needed (that row is the *terminal's* full width,
    // not this call's own `width` above) and why this is a plain
    // snapshot, not a live reference. `None` falls back to the original
    // in-place substitution.
    global_row_size: Option<(usize, usize)>,
    on_idle: &mut dyn FnMut(),
) -> io::Result<LineNormalExit> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
    // Visual mode's own committed selections -- see `LineBuffer::
    // selections`' own doc comment. Fresh and empty for this excursion,
    // same as `marks` just above.
    let mut selections: Vec<motion::MotionRange> = Vec::new();
    // Checkpoints whatever was typed via *ordinary* typing since the last
    // checkpoint (read_line's own per-character loop never calls this
    // itself) as its own group, before this excursion's first command can
    // add one of its own on top -- without this, `undo`'s own `current`
    // node can be stale by an arbitrary amount of ordinary typing, and if
    // this excursion's first edit happens to land back on that stale
    // content by coincidence, `checkpoint` would (correctly, by its own
    // rules) treat it as a no-op and silently lose both the typed text
    // *and* the edit as separate undo-able steps.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    // Reverse-video prompt: the mode indicator. Deliberately not a
    // terminal cursor-shape change (DECSCUSR) -- that's global terminal
    // state with no clean way to restore whatever the user's own
    // terminal had configured before this feature ever touched it, so a
    // forced "steady bar" on the way back out would itself be an
    // unwanted, sticky change to something this feature has no business
    // touching.
    let decorated_prompt = format!("\x1b[7m{}\x1b[0m", prompt);
    let (bar_text, matches) = normal_mode_prompt_and_matches(ed, &mut marks, &mut selections, &vk);
    let shown = if global_row_size.is_some() { decorated_prompt.clone() } else { bar_text.clone().unwrap_or_else(|| decorated_prompt.clone()) };
    redraw(&shown, ed, col_origin, width, ctx, &matches)?;
    draw_global_search_bar(global_row_size, bar_text.as_deref());
    let exit = loop {
        let key = match read_key_idle(on_idle)? {
            Some(k) => k,
            None => break LineNormalExit::Eof,
        };
        match key {
            // A search actively being typed keeps Ctrl-C for itself
            // (see vimkeys.rs's own feed_search doc comment on its
            // Ctrl-C arm) -- only intercepted here, before vk.feed ever
            // sees it, when no search is in progress.
            Key::CtrlC | Key::CtrlD | Key::CtrlZ if !vk.is_search_pending() => break LineNormalExit::Propagate(key),
            // Visual mode's own `Z`/`y`/`d`/`c`/`p`/`P`/Escape -- intercepted
            // here, ahead of `vk.feed`, for the same reason repl.rs's own
            // identical arms are (see its own doc comment): "is there a
            // selection to act on" is `LineBuffer`-owned state vimkeys.rs
            // deliberately never sees. `vk.is_idle()` guards all of them:
            // mid a sub-prefix (`f`, a count, ...) these keys keep their
            // ordinary meaning instead.
            //
            // `Z` (single, not `ZZ` -- unlike repl.rs's own Normal mode,
            // nothing here gives `Z` any other meaning to preserve):
            // commits the active selection and returns to Normal mode,
            // keeping every selection so far highlighted so another
            // `v`/`V` can start the next one.
            Key::Char('Z') if vk.is_idle() && vk.is_visual() => {
                let lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let end_cursor = lb.cursor();
                vk.end_visual(end_cursor);
            }
            // `y`: yanks every committed selection plus the active one
            // (if any) as one concatenated register value, then clears
            // both -- reachable either mid an active `v`/`V` selection,
            // or from plain Normal mode right after a `Z` with nothing
            // new started (the `!selections.is_empty()` half of the
            // guard). Falls through to vimkeys.rs's own ordinary
            // `y`/`yy`/`y{motion}` handling whenever neither is true.
            Key::Char('y') if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let register = vk.take_pending_register();
                let end_cursor = lb.cursor();
                yank_selections_line(&lb, registers, register);
                lb.selections.clear();
                vk.end_visual(end_cursor);
            }
            // `d`: deletes every committed selection plus the active one
            // (writing the concatenated deleted text to a register
            // first, same as any other delete), then clears both. Same
            // reachability as `y` just above; otherwise falls through to
            // ordinary `d`/`dd`/`d{motion}` operator-arming.
            Key::Char('d') if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let register = vk.take_pending_register();
                let end_cursor = lb.cursor();
                delete_selections(&mut lb, registers, register);
                lb.selections.clear();
                vk.end_visual(end_cursor);
            }
            // `c`: like `d` (same deletion, same "delete always yanks"
            // register write, reusing `delete_selections` outright), but
            // also enters insert mode afterward -- vim's own visual `c`.
            // Only breaks to insert if something was actually deleted,
            // mirroring `Op::Change`'s own single-motion arm below; in
            // practice this guard is never false here, since reaching
            // this arm at all already means either an active selection
            // (always yields a range) or a non-empty committed set.
            Key::Char('c') if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let register = vk.take_pending_register();
                let end_cursor = lb.cursor();
                let deleted = delete_selections(&mut lb, registers, register);
                lb.selections.clear();
                vk.end_visual(end_cursor);
                if deleted {
                    break LineNormalExit::ToInsert;
                }
            }
            // `p`/`P`: replaces every committed selection plus the
            // active one with the register's content (see
            // `put_over_selections`' own doc comment on why this
            // broadcasts to every selection rather than mimicking real
            // vim's single-selection swap). Same reachability as `y`/`d`;
            // otherwise falls through to ordinary `p`/`P` put-after/
            // before-cursor.
            Key::Char('p') | Key::Char('P') if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let register = vk.take_pending_register();
                let end_cursor = lb.cursor();
                put_over_selections(&mut lb, registers, register);
                lb.selections.clear();
                vk.end_visual(end_cursor);
            }
            // `S`: vim-surround's own "wrap the selection" -- reads one
            // more raw key directly (the delimiter character), the same
            // way `:` elsewhere in this codebase reads a whole Ex command
            // line; vimkeys.rs never sees this key at all, same reason
            // `y`/`d`/`c`/`p` don't reach it either while Visual is
            // active (see this loop's own module doc comment).
            Key::Char('S') if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let end_cursor = lb.cursor();
                if let Some(Key::Char(ch)) = read_key_idle(on_idle)? {
                    surround_selections(&mut lb, ch);
                }
                lb.selections.clear();
                vk.end_visual(end_cursor);
            }
            // Escape: cancels everything -- the active selection and
            // every previously committed one -- back to a clean Normal
            // mode with nothing yanked/deleted/replaced.
            Key::Escape if vk.is_idle() && (vk.is_visual() || !selections.is_empty()) => {
                let end_cursor = (0, ed.cursor);
                vk.end_visual(end_cursor);
                selections.clear();
            }
            _ => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                match vk.feed(key) {
                    KeyOutcome::Motion(m, count) => {
                        apply_motion_or_reselect(&mut vk, &mut lb, m, count);
                    }
                    KeyOutcome::EnterInsert(cmd) => {
                        let (new_buf, new_cursor) = vimkeys::apply_insert_cmd(&lb.ed.buf, lb.ed.cursor, cmd);
                        lb.ed.buf = new_buf;
                        lb.ed.cursor = new_cursor;
                        break LineNormalExit::ToInsert;
                    }
                    KeyOutcome::Operator(op, motion, count, register) => match op {
                        Op::Yank => yank_motion(&mut lb, registers, motion, count, register),
                        Op::Delete => {
                            delete_motion(&mut lb, registers, motion, count, register);
                        }
                        Op::Change => {
                            let motion = redirect_cw_to_ce(&lb, &motion);
                            if delete_motion(&mut lb, registers, motion, count, register) {
                                break LineNormalExit::ToInsert;
                            }
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                            case_operator_motion(&mut lb, motion, count, case_kind_for_op(op));
                        }
                        // `>{motion}`/`<{motion}`: see indent_line/
                        // outdent_line's own doc comment on why the
                        // motion itself is ignored here.
                        Op::Indent => indent_line(&mut lb),
                        Op::Outdent => outdent_line(&mut lb),
                    },
                    KeyOutcome::OperatorLines(op, count, register) => match op {
                        Op::Yank => yank_lines(&lb, registers, count, register),
                        Op::Delete => delete_lines(&mut lb, registers, count, register),
                        Op::Change => {
                            delete_lines(&mut lb, registers, count, register);
                            break LineNormalExit::ToInsert;
                        }
                        Op::Lowercase | Op::Uppercase | Op::CaseToggle => case_operator_lines(&mut lb, case_kind_for_op(op)),
                        Op::Indent => indent_line(&mut lb),
                        Op::Outdent => outdent_line(&mut lb),
                    },
                    KeyOutcome::Put { before, count, register } => put(&mut lb.ed.buf, &mut lb.ed.cursor, registers, before, count, register),
                    KeyOutcome::DeleteCharForward { count, register } => {
                        let (new_buf, new_cursor, deleted) = vimkeys::apply_delete_forward(&lb.ed.buf, lb.ed.cursor, count.unwrap_or(1).max(1));
                        lb.ed.buf = new_buf;
                        lb.ed.cursor = new_cursor;
                        if !deleted.is_empty() {
                            registers.record_delete(register, RegisterValue { text: deleted, shape: RegisterShape::Char });
                        }
                    }
                    // `v`/`V`: arms Visual mode with the buffer's own
                    // current cursor as the anchor (vimkeys.rs can't read
                    // that itself -- see `EnterVisual`'s own doc
                    // comment). Rendering (the reverse-video highlight,
                    // via `push_selection_matches`) and what `y`/`d`/`p`/
                    // `Z`/Escape do from here on are handled by the
                    // guarded arms above, at the top of this same loop.
                    KeyOutcome::EnterVisual(shape) => {
                        vk.begin_visual(shape, lb.cursor());
                    }
                    KeyOutcome::ReselectVisual => {
                        if let Some((shape, anchor, cursor)) = vk.last_visual() {
                            lb.set_cursor(cursor.0, cursor.1);
                            vk.begin_visual(shape, anchor);
                        }
                    }
                    KeyOutcome::Jump { forward } => {
                        let current = lb.cursor();
                        let target = if forward { vk.jump_forward(current) } else { vk.jump_back(current) };
                        if let Some((row, col)) = target {
                            lb.set_cursor(row, col);
                        }
                    }
                    // <C-w> is still vimkeys' own window-leader prefix
                    // here too, matching real vim's own Normal-mode
                    // Ctrl-W meaning -- intentionally not special-cased
                    // away, even though there's no window/pane state for
                    // it to act on in this context (a harmless no-op).
                    // `Join`/`OpenLine` (`o`/`O`) are no-ops for the same
                    // reason: `LineBuffer` is a single line by
                    // construction (see its own doc comment) -- there's
                    // never a next/previous line to join with or open one
                    // beside.
                    KeyOutcome::AddSurround { target, ch } => add_surround(&mut lb, target, ch),
                    KeyOutcome::DeleteSurround { ch } => delete_surround(&mut lb, ch),
                    KeyOutcome::ChangeSurround { ch, replacement } => change_surround(&mut lb, ch, replacement),
                    KeyOutcome::ReplaceChar { ch, count } => replace_char(&mut lb, ch, count.unwrap_or(1).max(1)),
                    // `R`: degrades to an ordinary `i`-style insert entry
                    // right at the cursor (no repositioning needed --
                    // same as real `R`'s own starting point) rather than
                    // true overtype-as-you-type. A deliberate
                    // simplification: true Replace-mode typing behavior
                    // would need to live inside `read_line`'s own typing
                    // loop (this excursion always resumes there once it
                    // breaks out to `ToInsert`), which is the single
                    // highest-traffic code path in the whole shell -- not
                    // a change to make for this one command. `fileeditor.
                    // rs`'s own `run_insert_mode` has its own dedicated
                    // loop instead, so `R` gets the real thing there.
                    KeyOutcome::EnterReplace => break LineNormalExit::ToInsert,
                    KeyOutcome::ToggleCase { count } => toggle_case(&mut lb, count.unwrap_or(1).max(1)),
                    KeyOutcome::AdjustNumber { delta } => adjust_number(&mut lb, delta),
                    // Guarded the same way repl.rs's own Undo/Redo arms
                    // are -- see KeyOutcome::Undo's own doc comment in
                    // vimkeys.rs for why: real vim's Visual mode binds
                    // bare `u`/`U` to lowercase/uppercase the selection,
                    // not implemented in this codebase today, so `u`
                    // simply does nothing while a selection is active
                    // rather than misfiring as undo.
                    KeyOutcome::Undo(count) if !vk.is_visual() && lb.selections.is_empty() => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let Some(snap) = undo.undo() else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                    }
                    KeyOutcome::Redo(count) if !vk.is_visual() && lb.selections.is_empty() => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let Some(snap) = undo.redo() else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                    }
                    // `g-`/`g+`: unlike `u`/`Ctrl-R`, these can land on a
                    // node in a completely different branch than the one
                    // `undo`'s own `current` was just on -- see
                    // UndoTree::time_travel_back/forward's own doc
                    // comment. Same Visual-mode guard as `u`/`Ctrl-R`
                    // just above, for the same reason.
                    KeyOutcome::UndoSeq { forward, count } if !vk.is_visual() && lb.selections.is_empty() => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let snap = if forward { undo.time_travel_forward() } else { undo.time_travel_back() };
                            let Some(snap) = snap else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                    }
                    KeyOutcome::Undo(_) | KeyOutcome::Redo(_) | KeyOutcome::UndoSeq { .. } => {}
                    KeyOutcome::Window(..) | KeyOutcome::Join { .. } | KeyOutcome::OpenLine { .. } | KeyOutcome::Pending | KeyOutcome::None => {}
                }
            }
        }
        // Commits a new undo-tree node if this key actually changed
        // `ed.buf` -- a no-op otherwise (pure navigation, or an undo/redo
        // that just made `ed.buf` match the node it moved to). Once per
        // top-level key, same "one hook point, reached regardless of
        // which arm ran" shape as repl.rs's own `render_nav_frame` uses
        // for `TextBuffer::checkpoint_undo` -- see that one's own doc
        // comment for why that's what actually defines an undo "group".
        undo.checkpoint(&ed.buf, (0, ed.cursor));
        let (bar_text, matches) = normal_mode_prompt_and_matches(ed, &mut marks, &mut selections, &vk);
        let shown = if global_row_size.is_some() { decorated_prompt.clone() } else { bar_text.clone().unwrap_or_else(|| decorated_prompt.clone()) };
        redraw(&shown, ed, col_origin, width, ctx, &matches)?;
        draw_global_search_bar(global_row_size, bar_text.as_deref());
    };
    // Every `break` above (ToInsert, Propagate, Eof) skips straight past
    // the loop body's own trailing draw_global_search_bar call --
    // without this, whatever the bar was last showing would keep
    // sitting at the global row indefinitely once back in ordinary
    // insert-mode typing, since nothing else calls it again until the
    // next Ctrl-E excursion. Always erase here instead, regardless of
    // which exit path was taken.
    draw_global_search_bar(global_row_size, None);
    Ok(exit)
}

// `KeyOutcome::Motion`'s ordinary handling (`apply_motion`, moving the
// cursor) except for a `Motion::TextObject` reached while in Visual mode
// (`viw`, `va(`, ...): vimkeys.rs can only ever hand back a plain `Motion`
// there (it never touches a `Buffer` itself), but a text object needs to
// move *both* Visual ends -- the anchor to the object's start, the cursor
// to its end -- not just the cursor the way every other motion does.
// Also where `motion::is_jump` motions get recorded for `Ctrl-O`/`Ctrl-I`
// (`vk.push_jump`) and vim's own ``` ``` ```/`''` mark (a plain `Buffer`
// mark keyed by an apostrophe -- see `VimKeys::push_jump`'s and
// `feed_mark`'s own doc comments) -- this is the one place every jump
// motion in every Buffer-owning context already passes through, so
// recording happens here rather than at each of this function's own
// three call sites. Falls through to `apply_motion` for every other case,
// including a `TextObject` reached via an armed operator (handled
// entirely inside `motion::motion_range`, not here) -- operator targets
// aren't jumps; the cursor ends up back at the range's own start either
// way, not somewhere new to navigate back from. Generic over `Buffer` for
// the same reason `yank_motion` just below is -- shared by this file's
// own run_line_normal_mode and by repl.rs's own ScreenBuffer-based Visual
// mode.
pub fn apply_motion_or_reselect(vk: &mut VimKeys, buf: &mut impl crate::bishedit::Buffer, m: motion::Motion, count: Option<usize>) {
    if let (true, motion::Motion::TextObject(kind, around)) = (vk.is_visual(), &m) {
        if let Some(range) = motion::text_object_range(buf, *kind, *around, count) {
            let (shape, _) = vk.visual_anchor().unwrap();
            vk.begin_visual(shape, range.from);
            buf.set_cursor(range.to.0, range.to.1);
        }
        return;
    }
    if motion::is_jump(&m) {
        let pos = buf.cursor();
        vk.push_jump(pos);
        // Applied *before* overwriting the `'` mark, not after: `` ` ` ``/
        // `''` themselves resolve to `Motion::GotoMark('\'')`/`GotoMarkLine
        // ('\'')`, which read that very mark inside `apply_motion` below --
        // writing the new value first would make ``` ``` ``` a no-op
        // (jump to wherever it already just landed, immediately clobbered
        // by its own jump-recording).
        motion::apply_motion(buf, m, count);
        buf.set_mark('\'', pos);
        return;
    }
    motion::apply_motion(buf, m, count);
}

// The `y{motion}`/`yy` glue: generic over any `bishedit::Buffer`, so this
// is shared not just by run_line_normal_mode/run_one_shot_normal_command
// below (both drive a `LineBuffer`) but by repl.rs's own ScreenBuffer-based
// Ctrl+Space normal mode too (yank-only there -- see that call site). `put`
// stays private to this file: it works on a `LineEditor`'s own buffer
// directly (not through `Buffer`), and only the LineBuffer-driven contexts
// ever have something real to put into -- see repl.rs's own doc comment on
// why `KeyOutcome::Put` is a deliberate no-op there.
pub fn yank_motion(buf: &mut impl crate::bishedit::Buffer, registers: &mut Registers, motion: motion::Motion, count: Option<usize>, register: Option<char>) {
    let Some(range) = motion::motion_range(buf, motion, count) else {
        return;
    };
    let text = motion::extract_text(buf, &range);
    let shape = if range.shape == motion::MotionShape::Linewise { RegisterShape::Line } else { RegisterShape::Char };
    registers.record_yank(register, RegisterValue { text, shape });
}

pub fn yank_lines(buf: &impl crate::bishedit::Buffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let text = motion::whole_lines(buf, count.unwrap_or(1).max(1));
    registers.record_yank(register, RegisterValue { text, shape: RegisterShape::Line });
}

fn put(buf: &mut Vec<char>, cursor: &mut usize, registers: &mut Registers, before: bool, count: Option<usize>, register: Option<char>) {
    let value = registers.read(register);
    let text = value.flatten_to_single_line();
    let (new_buf, new_cursor) = vimkeys::apply_put(buf, *cursor, &text, before, count.unwrap_or(1).max(1));
    *buf = new_buf;
    *cursor = new_cursor;
}

// `d{motion}`/`c{motion}`'s shared half: computes the same range
// `yank_motion` would (writing it to a register the same way), then
// additionally removes that text from the buffer, leaving the cursor at
// the range's own start -- vim's own rule for both (`c{motion}` then
// enters insert at that same position, handled by the caller). Unlike
// `yank_motion`, this isn't generic over `impl Buffer`: mutation only
// makes sense against a real `LineEditor`, and `ScreenBuffer`'s own
// Normal mode never reaches this (see repl.rs's own doc comment on why
// `Operator`/`OperatorLines` there stay yank-only regardless of `Op`).
// Returns whether anything was actually deleted -- `Op::Change` uses this
// to decide whether to enter insert mode at all: a motion target that
// doesn't exist or doesn't move (`motion::motion_range` returning `None`)
// aborts the whole `c{motion}`, the same way it already silently aborts
// a `y{motion}`, rather than dropping into insert mode having changed
// nothing.
fn delete_motion(lb: &mut LineBuffer, registers: &mut Registers, motion: motion::Motion, count: Option<usize>, register: Option<char>) -> bool {
    let Some(range) = motion::motion_range(lb, motion, count) else {
        return false;
    };
    let text = motion::extract_text(lb, &range);
    let shape = if range.shape == motion::MotionShape::Linewise { RegisterShape::Line } else { RegisterShape::Char };
    registers.record_delete(register, RegisterValue { text, shape });
    let (from_col, to_col) = (range.from.1, range.to.1);
    match range.shape {
        // A single-line buffer's own "linewise" is always the whole
        // buffer -- same flattening `whole_lines`/`yank_lines` already
        // established for `yy`.
        motion::MotionShape::Linewise => {
            lb.ed.buf.clear();
            lb.ed.cursor = 0;
        }
        motion::MotionShape::Inclusive => {
            let end = (to_col + 1).min(lb.ed.buf.len());
            lb.ed.buf.drain(from_col..end);
            lb.ed.cursor = from_col.min(lb.ed.buf.len().saturating_sub(1));
        }
        motion::MotionShape::Exclusive => {
            lb.ed.buf.drain(from_col..to_col);
            lb.ed.cursor = from_col.min(lb.ed.buf.len().saturating_sub(1));
        }
    }
    true
}

// `dd`/`cc`'s own whole-line shorthand -- see `delete_motion`'s own doc
// comment on the single-line "linewise means the whole buffer"
// flattening (same one `yank_lines`/`whole_lines` use for `yy`). Unlike
// `delete_motion`, there's no failure case: `count` lines always exist to
// delete (clamped to just "the" line, since there's only ever one), so
// `Op::Change` never needs to check a return value here the way it does
// for `delete_motion` -- `cc` always enters insert.
fn delete_lines(lb: &mut LineBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let text = motion::whole_lines(lb, count.unwrap_or(1).max(1));
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
    lb.ed.buf.clear();
    lb.ed.cursor = 0;
}

// `>{motion}`/`>>`/`<{motion}`/`<<`: same "linewise means the whole
// buffer" flattening `delete_lines`'s own doc comment establishes for
// `dd`/`cc` -- there's only ever one line here, so both the motion form
// and the double-tap shorthand collapse to the same "shift the one
// line" action, and `count`/the motion itself are ignored (vim's own
// indent amount is always exactly one shiftwidth regardless of how many
// lines a count would otherwise select, and there's no second line for
// a motion to extend into). A genuinely empty line is left alone, same
// as fileeditor::indent_rows's own rule.
fn indent_line(lb: &mut LineBuffer) {
    if lb.ed.buf.is_empty() {
        return;
    }
    lb.ed.buf.splice(0..0, std::iter::repeat_n(' ', vimkeys::INDENT_WIDTH));
    lb.ed.cursor = 0;
}

fn outdent_line(lb: &mut LineBuffer) {
    let strip = lb.ed.buf.iter().take(vimkeys::INDENT_WIDTH).take_while(|&&c| c == ' ' || c == '\t').count();
    if strip == 0 {
        return;
    }
    lb.ed.buf.drain(0..strip);
    lb.ed.cursor = 0;
}

// Visual mode's own `y`: every selection in `lb.selections` (already in
// commit order -- `Z` pushes, this reads, the caller clears afterward),
// concatenated with no separator -- mirrors repl.rs's own
// `yank_selections` exactly (see its doc comment for why: a `Linewise`
// part already ends in `\n`, a charwise part butts directly against its
// neighbor).
fn yank_selections_line(lb: &LineBuffer, registers: &mut Registers, register: Option<char>) {
    if lb.selections.is_empty() {
        return;
    }
    let mut text = String::new();
    let mut shape = RegisterShape::Char;
    for range in lb.selections.iter() {
        text.push_str(&motion::extract_text(lb, range));
        if range.shape == motion::MotionShape::Linewise {
            shape = RegisterShape::Line;
        }
    }
    registers.record_yank(register, RegisterValue { text, shape });
}

// Visual mode's own `d`: removes every selection from the buffer,
// writing the concatenated deleted text to a register first (vim's own
// "delete always yanks" rule, same as any other delete operator).
// Returns whether anything was actually selected -- mirrors
// `delete_motion`'s own `bool` result, though this call site never
// checks it for a change-to-insert decision the way that one does (there
// is no Visual `c` yet).
fn delete_selections(lb: &mut LineBuffer, registers: &mut Registers, register: Option<char>) -> bool {
    if lb.selections.is_empty() {
        return false;
    }
    let mut text = String::new();
    let mut shape = RegisterShape::Char;
    for range in lb.selections.iter() {
        text.push_str(&motion::extract_text(lb, range));
        if range.shape == motion::MotionShape::Linewise {
            shape = RegisterShape::Line;
        }
    }
    registers.record_delete(register, RegisterValue { text, shape });

    // A `Linewise` selection (`V`) already covers the whole single-line
    // buffer -- same flattening `yy`/`dd`/`cc` already use -- so it
    // subsumes every other selection in the set outright; no per-range
    // removal needed (or safe: the buffer's about to be empty either
    // way).
    if lb.selections.iter().any(|r| r.shape == motion::MotionShape::Linewise) {
        lb.ed.buf.clear();
        lb.ed.cursor = 0;
        return true;
    }

    // Removed highest start-column first, so removing a rightward
    // selection never shifts a still-pending leftward one's own columns
    // -- `leftmost` is captured from the *original* (pre-removal) ranges
    // for exactly that reason: the leftmost range's own start column
    // never moves no matter what's removed to its right.
    let leftmost = lb.selections.iter().map(|r| r.from.1).min().unwrap_or(0);
    let mut ranges = lb.selections.clone();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.from.1));
    for range in &ranges {
        // Re-clamped to the buffer's *current* length on every iteration
        // (not computed once up front) -- panic-safe even against
        // pathologically overlapping selections, which nothing currently
        // prevents the user from creating.
        let len = lb.ed.buf.len();
        let start = range.from.1.min(len);
        let end = (range.to.1 + 1).min(len).max(start);
        lb.ed.buf.drain(start..end);
    }
    lb.ed.cursor = leftmost.min(lb.ed.buf.len().saturating_sub(1));
    true
}

// Visual mode's own `S{ch}` -- vim-surround's own "wrap the selection"
// command. Wraps every committed selection plus the active one in `ch`'s
// own delimiter pair -- "wrap every one of these", the same multi-
// selection extension `put_over_selections`' own doc comment already
// establishes for `p`. Highest start-column first, so wrapping a
// rightward selection never shifts a still-pending leftward one's own
// columns. Cursor lands on the leftmost selection's own inserted open
// delimiter, mirroring `add_surround`'s single-target convention.
fn surround_selections(lb: &mut LineBuffer, ch: char) {
    if lb.selections.is_empty() {
        return;
    }
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let mut ranges = lb.selections.clone();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.from.1));
    let mut leftmost_open_at = 0;
    for range in &ranges {
        let (open_at, close_at) = motion::surround_insert_points(lb, range);
        let close_col = close_at.1.min(lb.ed.buf.len());
        lb.ed.buf.splice(close_col..close_col, close.chars());
        let open_col = open_at.1.min(lb.ed.buf.len());
        lb.ed.buf.splice(open_col..open_col, open.chars());
        leftmost_open_at = open_col;
    }
    lb.ed.cursor = leftmost_open_at;
}

// Visual mode's own `p`/`P` (both the same in Visual mode, matching real
// vim: there's no "before/after the cursor" when replacing a selected
// range, only "instead of it"): replaces *every* selection with the same
// register content -- a deliberate departure from real vim's own
// single-selection swap (where the replaced text lands back in the
// register): with several selections, broadcasting one register's worth
// of text to each -- "replace every one of these with that" -- is the
// actually useful multi-selection behavior (and keeps the register
// intact for pasting the same replacement again), so the old, replaced-
// away text is simply discarded here rather than overwriting the
// register a caller may still want. Returns whether anything was
// actually selected/replaced.
fn put_over_selections(lb: &mut LineBuffer, registers: &mut Registers, register: Option<char>) -> bool {
    if lb.selections.is_empty() {
        return false;
    }
    let insert_text = registers.read(register).flatten_to_single_line();
    let insert_chars: Vec<char> = insert_text.chars().collect();
    if insert_chars.is_empty() {
        return false;
    }

    if lb.selections.iter().any(|r| r.shape == motion::MotionShape::Linewise) {
        let len = insert_chars.len();
        lb.ed.buf = insert_chars;
        lb.ed.cursor = len.saturating_sub(1);
        return true;
    }

    let mut ranges = lb.selections.clone();
    ranges.sort_by_key(|r| std::cmp::Reverse(r.from.1));
    let mut leftmost_insert_at = 0;
    for range in &ranges {
        let len = lb.ed.buf.len();
        let start = range.from.1.min(len);
        let end = (range.to.1 + 1).min(len).max(start);
        lb.ed.buf.splice(start..end, insert_chars.iter().copied());
        leftmost_insert_at = start;
    }
    // Cursor on the last character of the leftmost replacement -- the
    // same "ends on the last inserted character" rule `apply_put`'s own
    // doc comment already establishes for a single `p`.
    lb.ed.cursor = (leftmost_insert_at + insert_chars.len()).saturating_sub(1).min(lb.ed.buf.len().saturating_sub(1));
    true
}

// `ys{motion}`/`yss`'s own target resolution: a motion resolves exactly
// like any other operator target (`motion::motion_range`, `None` on a
// failed/empty one -- same silent no-op `yank_motion`/`delete_motion`
// already give a failed target); `yss` needs no such lookup at all --
// `LineBuffer` is always exactly one line (see its own doc comment), so
// "the current line" is simply the whole buffer, linewise, every time
// (unlike `TextBuffer`'s own multi-line version of this same target --
// see `fileeditor.rs`'s own `resolve_surround_target` -- `count` has
// nothing further to extend into here).
fn resolve_surround_target(lb: &mut LineBuffer, target: &vimkeys::SurroundTarget) -> Option<motion::MotionRange> {
    match target {
        vimkeys::SurroundTarget::Motion(m, count) => motion::motion_range(lb, m.clone(), *count),
        vimkeys::SurroundTarget::Line(_) => Some(motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) }),
    }
}

// `ys{motion}{ch}`/`yss{ch}`: wraps `target`'s resolved range in `ch`'s
// own delimiter pair. Splices the close delimiter in first, then the
// open one -- inserting at/after `close_at` can never shift `open_at`'s
// own column, so no further adjustment is needed regardless of shape
// (see `motion::surround_insert_points`'s own doc comment). Cursor lands
// on the inserted open delimiter's own first character, matching
// vim-surround.
fn add_surround(lb: &mut LineBuffer, target: vimkeys::SurroundTarget, ch: char) {
    let Some(range) = resolve_surround_target(lb, &target) else {
        return;
    };
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let (open_at, close_at) = motion::surround_insert_points(lb, &range);
    let close_col = close_at.1.min(lb.ed.buf.len());
    lb.ed.buf.splice(close_col..close_col, close.chars());
    let open_col = open_at.1.min(lb.ed.buf.len());
    lb.ed.buf.splice(open_col..open_col, open.chars());
    lb.ed.cursor = open_col;
}

// `ds{ch}`: removes the nearest enclosing pair named by `ch`, plus any
// padding `motion::surround_delete_spans` decides to strip -- close side
// first, so removing it can never shift the open side's own column. A
// no-op if `ch` doesn't name a valid target or no such pair encloses the
// cursor.
fn delete_surround(lb: &mut LineBuffer, ch: char) {
    let Some(kind) = motion::surround_target_kind(ch) else {
        return;
    };
    let Some((open_pos, close_pos)) = motion::surround_pair_positions(lb, kind) else {
        return;
    };
    let (open_range, close_range) = motion::surround_delete_spans(lb, kind, open_pos, close_pos);
    delete_char_range_line(lb, &close_range);
    delete_char_range_line(lb, &open_range);
    lb.ed.cursor = open_range.from.1.min(lb.ed.buf.len().saturating_sub(1));
}

// `cs{ch}{replacement}`: like `delete_surround`, but replaces the found
// pair's own two delimiter characters with `replacement`'s pair instead
// of removing them -- never touches any padding around them (unlike
// `ds`).
fn change_surround(lb: &mut LineBuffer, ch: char, replacement: char) {
    let Some(kind) = motion::surround_target_kind(ch) else {
        return;
    };
    let Some((open_pos, close_pos)) = motion::surround_pair_positions(lb, kind) else {
        return;
    };
    let Some((open, close)) = motion::surround_delims(replacement) else {
        return;
    };
    let close_col = close_pos.1;
    lb.ed.buf.splice(close_col..close_col + 1, close.chars());
    let open_col = open_pos.1;
    lb.ed.buf.splice(open_col..open_col + 1, open.chars());
    lb.ed.cursor = open_col;
}

// `Ctrl-A`/`Ctrl-X`: adjusts the decimal number found at or after the
// cursor by `delta`, replacing it and leaving the cursor on the new
// number's own last digit. A no-op if there's no number on the line
// from the cursor onward.
fn adjust_number(lb: &mut LineBuffer, delta: i64) {
    let Some(m) = motion::find_number(lb, lb.cursor()) else {
        return;
    };
    let replacement = motion::apply_number_delta(&m, delta);
    lb.ed.buf.splice(m.from.1..=m.to.1, replacement.chars());
    lb.ed.cursor = m.from.1 + replacement.chars().count() - 1;
}

fn case_kind_for_op(op: Op) -> motion::CaseKind {
    match op {
        Op::Lowercase => motion::CaseKind::Lower,
        Op::Uppercase => motion::CaseKind::Upper,
        Op::CaseToggle => motion::CaseKind::Toggle,
        Op::Yank | Op::Delete | Op::Change | Op::Indent | Op::Outdent => {
            unreachable!("case_kind_for_op is only ever called for Op::Lowercase/Uppercase/CaseToggle")
        }
    }
}

// `gu{motion}`/`gU{motion}`/`g~{motion}`: transforms the case of every
// character in the motion's resolved range, leaving the cursor at the
// range's own start -- same "operator leaves the cursor at `from`" rule
// `delete_motion`'s own doc comment establishes. A no-op on a failed/
// empty target, same as any other operator.
fn case_operator_motion(lb: &mut LineBuffer, motion: motion::Motion, count: Option<usize>, kind: motion::CaseKind) {
    let Some(range) = motion::motion_range(lb, motion, count) else {
        return;
    };
    let (from_col, to_col) = (range.from.1, range.to.1);
    match range.shape {
        // A single-line buffer's own "linewise" is always the whole
        // buffer -- same flattening `delete_motion`/`yank_lines` already
        // establish for `yy`/`dd`.
        motion::MotionShape::Linewise => case_operator_lines(lb, kind),
        motion::MotionShape::Inclusive => {
            let end = (to_col + 1).min(lb.ed.buf.len());
            for c in lb.ed.buf[from_col..end].iter_mut() {
                *c = motion::case_transform(*c, kind);
            }
            lb.ed.cursor = from_col.min(lb.ed.buf.len().saturating_sub(1));
        }
        motion::MotionShape::Exclusive => {
            let end = to_col.min(lb.ed.buf.len());
            for c in lb.ed.buf[from_col..end].iter_mut() {
                *c = motion::case_transform(*c, kind);
            }
            lb.ed.cursor = from_col.min(lb.ed.buf.len().saturating_sub(1));
        }
    }
}

// `guu`/`gUU`/`g~~`: the same whole-line shorthand `yy`/`dd`/`cc` already
// establish (see `delete_lines`'s own doc comment) -- transforms the
// whole (single-line) buffer. No `count` parameter: unlike `dd`/`yy`,
// there's never more than the one line for a wider count to reach.
fn case_operator_lines(lb: &mut LineBuffer, kind: motion::CaseKind) {
    for c in lb.ed.buf.iter_mut() {
        *c = motion::case_transform(*c, kind);
    }
    lb.ed.cursor = 0;
}

// `~`: toggles the case of `count` characters starting at the cursor,
// then advances the cursor to just past the last one toggled (clamped
// to the buffer's own last character) -- see `KeyOutcome::ToggleCase`'s
// own doc comment. A no-op on an empty buffer.
fn toggle_case(lb: &mut LineBuffer, count: usize) {
    let (_, col) = lb.cursor();
    let len = lb.ed.buf.len();
    if col >= len {
        return;
    }
    let end = (col + count).min(len);
    for c in lb.ed.buf.iter_mut().take(end).skip(col) {
        *c = motion::case_transform(*c, motion::CaseKind::Toggle);
    }
    lb.ed.cursor = end.min(len.saturating_sub(1));
}

// `r{ch}`: replaces `count` characters starting at the cursor with `ch`
// each, staying in Normal mode. Refuses (no-op) if fewer than `count`
// characters remain on the line from the cursor onward -- matches vim:
// `r` never crosses a line break or extends the buffer, unlike `s`
// (substitute, which deletes then inserts and so has no such limit).
fn replace_char(lb: &mut LineBuffer, ch: char, count: usize) {
    let (_, col) = lb.cursor();
    let len = lb.ed.buf.len();
    if count == 0 || col + count > len {
        return;
    }
    for c in lb.ed.buf.iter_mut().skip(col).take(count) {
        *c = ch;
    }
    lb.ed.cursor = col + count - 1;
}

// A single- or two-character-wide (`motion::surround_delete_spans`'s own
// `Inclusive` ranges, always same-line for `LineBuffer`) removal --
// `delete_surround`'s own primitive, kept separate from `delete_motion`
// above since it never writes a register (`ds` doesn't yank, matching
// vim-surround).
fn delete_char_range_line(lb: &mut LineBuffer, range: &motion::MotionRange) {
    let from = range.from.1.min(lb.ed.buf.len());
    let to = (range.to.1 + 1).min(lb.ed.buf.len()).max(from);
    lb.ed.buf.drain(from..to);
}

// vim's own "`cw`/`cW` act like `ce`/`cE`" rule: when the cursor sits on
// a non-blank character, changing "to the next word" would otherwise eat
// the trailing whitespace before that word too (via `WordForward`'s own
// exclusive-to-the-next-word-start semantics), which reads wrong when
// what follows is about to be retyped -- vim redirects it to "to the end
// of the current word" instead, matching what a user actually means.
// `motion::motion_shape`'s own doc comment flagged this as deliberately
// unhandled until a real change operator existed to need it; this is
// that. Only ever called for `Op::Change`, and only changes anything for
// exactly these two motions -- every other motion (and `Op::Delete`,
// which never calls this) is unaffected, matching vim (`dw` still eats
// the trailing whitespace; only `cw` gets the redirect).
fn redirect_cw_to_ce(lb: &LineBuffer, motion: &motion::Motion) -> motion::Motion {
    let on_word_char = matches!(lb.ed.buf.get(lb.ed.cursor), Some(c) if !c.is_whitespace());
    match motion {
        motion::Motion::WordForward if on_word_char => motion::Motion::WordEnd,
        motion::Motion::WordForwardBig if on_word_char => motion::Motion::WordEndBig,
        other => other.clone(),
    }
}

// Ctrl-O (vim's insert-mode "do exactly one normal command, then return
// to insert"): reachable directly from ordinary typing, no need to
// already be in Ctrl-E's own line-local mode first, matching real vim.
// Reads and resolves exactly one VimKeys-recognized command -- a full
// multi-key sequence like `2f)`, not just one keystroke -- by looping
// until `vk.feed` stops returning `Pending`, applies it once, then always
// returns to insert (the cursor ends up wherever that one command left
// it -- e.g. `<C-o>b` really does leave the cursor at the start of the
// previous word once back in insert, exactly like real vim; it isn't
// snapped back). `Some(key)` is only ever returned for Ctrl-C/D/Z,
// handled by the caller the same way run_line_normal_mode's own
// `Propagate` is. No terminal cursor-shape change here either -- same
// reasoning as run_line_normal_mode's own doc comment.
fn run_one_shot_normal_command(ed: &mut LineEditor, registers: &mut Registers, undo: &mut UndoTree<Vec<char>>, on_idle: &mut dyn FnMut()) -> io::Result<Option<Key>> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
    // Never actually driven in this loop (see EnterVisual's own arm
    // below) -- exists only because `LineBuffer` always needs one to
    // construct, same reasoning as `marks`.
    let mut selections: Vec<motion::MotionRange> = Vec::new();
    // Same "checkpoint whatever ordinary typing did since the last
    // checkpoint, before this excursion's own edit can add one on top"
    // reasoning as run_line_normal_mode's own doc comment.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    let result = loop {
        let key = match read_key_idle(on_idle)? {
            Some(k) => k,
            // EOF mid-command: nothing resolved to apply -- the outer
            // loop's own next read_key_idle call will see the same EOF
            // and handle it properly: not lossy, just deferred by one
            // iteration.
            None => break None,
        };
        match key {
            // Same reasoning as run_line_normal_mode's own conditional
            // interception: a search actively being typed keeps Ctrl-C
            // for itself (vimkeys.rs's feed_search cancels just the
            // search), so it's only intercepted here when no search is
            // in progress.
            Key::CtrlC | Key::CtrlD | Key::CtrlZ if !vk.is_search_pending() => break Some(key),
            _ => {
                let mut lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                match vk.feed(key) {
                    KeyOutcome::Motion(m, count) => {
                        motion::apply_motion(&mut lb, m, count);
                        break None;
                    }
                    KeyOutcome::EnterInsert(cmd) => {
                        let (new_buf, new_cursor) = vimkeys::apply_insert_cmd(&lb.ed.buf, lb.ed.cursor, cmd);
                        lb.ed.buf = new_buf;
                        lb.ed.cursor = new_cursor;
                        break None;
                    }
                    // Ctrl-O already unconditionally returns to insert
                    // after any resolved outcome, so `Op::Change` here
                    // needs no "did it actually delete anything" check
                    // the way run_line_normal_mode's own arm does --
                    // `<C-o>cw` returns to insert either way, matching
                    // Ctrl-O's own "do exactly one command, then resume
                    // typing" contract regardless of what that command
                    // did.
                    KeyOutcome::Operator(op, motion, count, register) => {
                        match op {
                            Op::Yank => yank_motion(&mut lb, registers, motion, count, register),
                            Op::Delete => {
                                delete_motion(&mut lb, registers, motion, count, register);
                            }
                            Op::Change => {
                                let motion = redirect_cw_to_ce(&lb, &motion);
                                delete_motion(&mut lb, registers, motion, count, register);
                            }
                            Op::Lowercase | Op::Uppercase | Op::CaseToggle => {
                                case_operator_motion(&mut lb, motion, count, case_kind_for_op(op));
                            }
                            Op::Indent => indent_line(&mut lb),
                            Op::Outdent => outdent_line(&mut lb),
                        }
                        break None;
                    }
                    KeyOutcome::OperatorLines(op, count, register) => {
                        match op {
                            Op::Yank => yank_lines(&lb, registers, count, register),
                            Op::Delete | Op::Change => delete_lines(&mut lb, registers, count, register),
                            Op::Lowercase | Op::Uppercase | Op::CaseToggle => case_operator_lines(&mut lb, case_kind_for_op(op)),
                            Op::Indent => indent_line(&mut lb),
                            Op::Outdent => outdent_line(&mut lb),
                        }
                        break None;
                    }
                    KeyOutcome::Put { before, count, register } => {
                        put(&mut lb.ed.buf, &mut lb.ed.cursor, registers, before, count, register);
                        break None;
                    }
                    KeyOutcome::DeleteCharForward { count, register } => {
                        let (new_buf, new_cursor, deleted) = vimkeys::apply_delete_forward(&lb.ed.buf, lb.ed.cursor, count.unwrap_or(1).max(1));
                        lb.ed.buf = new_buf;
                        lb.ed.cursor = new_cursor;
                        if !deleted.is_empty() {
                            registers.record_delete(register, RegisterValue { text: deleted, shape: RegisterShape::Char });
                        }
                        break None;
                    }
                    KeyOutcome::AddSurround { target, ch } => {
                        add_surround(&mut lb, target, ch);
                        break None;
                    }
                    KeyOutcome::DeleteSurround { ch } => {
                        delete_surround(&mut lb, ch);
                        break None;
                    }
                    KeyOutcome::ChangeSurround { ch, replacement } => {
                        change_surround(&mut lb, ch, replacement);
                        break None;
                    }
                    KeyOutcome::ReplaceChar { ch, count } => {
                        replace_char(&mut lb, ch, count.unwrap_or(1).max(1));
                        break None;
                    }
                    // `R`: same degrade-to-`i` simplification as
                    // run_line_normal_mode's own arm (see its doc
                    // comment) -- Ctrl-O always returns to insert
                    // afterward anyway, matching its own "do exactly one
                    // command, then resume typing" contract regardless.
                    KeyOutcome::EnterReplace => break None,
                    KeyOutcome::ToggleCase { count } => {
                        toggle_case(&mut lb, count.unwrap_or(1).max(1));
                        break None;
                    }
                    KeyOutcome::AdjustNumber { delta } => {
                        adjust_number(&mut lb, delta);
                        break None;
                    }
                    // EnterVisual: a no-op here too, same reasoning as
                    // run_line_normal_mode's own arm just above -- Ctrl-O
                    // is "do exactly one command, then resume typing"
                    // anyway, which Visual mode's whole point (extending a
                    // selection over several subsequent keys) doesn't fit.
                    // `Join`/`OpenLine` are no-ops here for the same
                    // single-line reason `run_line_normal_mode`'s own arm
                    // documents.
                    KeyOutcome::Window(..)
                    | KeyOutcome::EnterVisual(_)
                    | KeyOutcome::ReselectVisual
                    | KeyOutcome::Join { .. }
                    | KeyOutcome::OpenLine { .. }
                    | KeyOutcome::Jump { .. }
                    | KeyOutcome::None => break None,
                    // `<C-o>u`/`<C-o>Ctrl-R`/`<C-o>g-`/`<C-o>g+`: real vim
                    // treats these as ordinary one-shot Normal commands
                    // too. No Visual-mode guard needed here the way
                    // run_line_normal_mode's own arms have one --
                    // `EnterVisual` is a no-op just above, so `vk.is_
                    // visual()` can never be true by the time any of
                    // these could fire.
                    KeyOutcome::Undo(count) => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let Some(snap) = undo.undo() else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                        break None;
                    }
                    KeyOutcome::Redo(count) => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let Some(snap) = undo.redo() else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                        break None;
                    }
                    KeyOutcome::UndoSeq { forward, count } => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let snap = if forward { undo.time_travel_forward() } else { undo.time_travel_back() };
                            let Some(snap) = snap else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                        break None;
                    }
                    KeyOutcome::Pending => continue,
                }
            }
        }
    };
    // Checkpoints once, right before returning -- this function always
    // applies exactly one command then returns, so there's no per-
    // iteration redraw hook to piggyback on the way run_line_normal_
    // mode's own loop has; this is the equivalent single point reached
    // regardless of which arm actually ran.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    Ok(result)
}

// Adapts `LineEditor`'s single line as a `bishedit::Buffer` -- just
// enough for `motion::apply_motion` to navigate it (`line_count` always
// 1, viewport_* degenerate -- no vertical scrolling in one line).
// Mutation (insert-entry's own deletes) deliberately bypasses this trait
// (it has none yet -- see bishedit's own module doc comment) and works
// directly on `ed.buf`/`ed.cursor` via `vimkeys::apply_insert_cmd`
// instead. `marks` is a fresh, empty map each time a `LineBuffer` is
// constructed (once per Ctrl-E excursion or Ctrl-O one-shot) -- not
// persisted across separate excursions, a fine simplification for a
// short-lived command line.
struct LineBuffer<'a> {
    ed: &'a mut LineEditor,
    marks: &'a mut HashMap<char, (usize, usize)>,
    // Visual mode's own committed selections -- see repl.rs's own
    // `ScreenBuffer::selections` doc comment for the shared design (a
    // plain `Vec<motion::MotionRange>`, `Z` pushes, `y`/`d`/`p` read and
    // clear). Fresh, empty each `LineBuffer` construction here too, same
    // as `marks` just above -- Visual mode doesn't persist across
    // separate Ctrl-E excursions.
    selections: &'a mut Vec<motion::MotionRange>,
}

impl<'a> crate::bishedit::Buffer for LineBuffer<'a> {
    fn line_count(&self) -> usize {
        1
    }

    fn line_len(&self, _line: usize) -> usize {
        self.ed.buf.len()
    }

    fn char_at(&self, _line: usize, col: usize) -> Option<char> {
        self.ed.buf.get(col).copied()
    }

    fn cursor(&self) -> (usize, usize) {
        (0, self.ed.cursor)
    }

    fn set_cursor(&mut self, _line: usize, col: usize) {
        self.ed.cursor = col.min(self.ed.buf.len());
    }

    fn viewport_top(&self) -> usize {
        0
    }

    fn set_viewport_top(&mut self, _line: usize) {}

    fn viewport_height(&self) -> usize {
        1
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        self.marks.insert(name, pos);
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        self.marks.get(&name).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // decode_csi_final is a pure function, unlike read_key/read_escape
    // (which read real bytes off stdin) -- this editor's first test
    // module, added alongside completion's Tab/Shift-Tab decoding since
    // that's the one piece of this stage testable without a real
    // terminal.
    #[test]
    fn decode_csi_final_shift_tab_is_back_tab() {
        assert_eq!(decode_csi_final("", b'Z'), Key::BackTab);
    }

    #[test]
    fn decode_csi_final_plain_arrows_still_decode_correctly() {
        assert_eq!(decode_csi_final("", b'A'), Key::Up);
        assert_eq!(decode_csi_final("", b'B'), Key::Down);
        assert_eq!(decode_csi_final("", b'C'), Key::Right);
        assert_eq!(decode_csi_final("", b'D'), Key::Left);
    }

    #[test]
    fn visible_len_counts_display_width_not_char_count() {
        assert_eq!(visible_len("abc"), 3);
        // Embedded SGR codes never count.
        assert_eq!(visible_len("\x1b[1;32mabc\x1b[0m"), 3);
        // A wide CJK char counts for 2, not 1.
        assert_eq!(visible_len("a中b"), 4);
    }

    #[test]
    fn visible_len_is_grapheme_cluster_aware() {
        // A ZWJ family emoji sequence (7 codepoints) counts as its own
        // one-cluster width (2), not the sum of every codepoint (8) --
        // see bishedit::unicode_width::str_width's own identical fix.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(visible_len(family), 2);
        // Still correct with real SGR codes wrapped around it, and
        // plain text on either side.
        assert_eq!(visible_len(&format!("a\x1b[1;32m{family}\x1b[0mb")), 1 + 2 + 1);
    }

    #[test]
    fn truncate_visible_drops_a_wide_char_that_would_only_partially_fit() {
        // "a" (1) + "中" (2) would need 3 columns; asking for 2 drops the
        // wide char whole rather than splitting it.
        assert_eq!(truncate_visible("a中b", 2), "a");
        assert_eq!(truncate_visible("a中b", 3), "a中");
        // A leading SGR code is preserved; truncation still stops for
        // good once the wide char after it wouldn't fit (matching this
        // function's own pre-existing "stop entirely, don't scan past
        // the cut" behavior -- a trailing reset code past that point
        // isn't included either).
        assert_eq!(truncate_visible("\x1b[1ma中\x1b[0m", 1), "\x1b[1ma");
    }

    #[test]
    fn decode_csi_final_unrecognized_final_byte_is_unknown() {
        assert_eq!(decode_csi_final("", b'Q'), Key::Unknown);
    }

    #[test]
    fn decode_csi_final_tilde_form_page_up_and_down() {
        assert_eq!(decode_csi_final("5", b'~'), Key::PageUp);
        assert_eq!(decode_csi_final("6", b'~'), Key::PageDown);
    }

    #[test]
    fn decode_sgr_mouse_final_press_and_release() {
        assert_eq!(decode_sgr_mouse_final("0;12;5", b'M'), Key::Mouse(MouseEvent { button: 0, col: 12, row: 5, pressed: true }));
        assert_eq!(decode_sgr_mouse_final("0;12;5", b'm'), Key::Mouse(MouseEvent { button: 0, col: 12, row: 5, pressed: false }));
    }

    #[test]
    fn decode_sgr_mouse_final_carries_the_raw_button_code() {
        // Bit 5 (0x20) set means "motion while a button is held" in SGR's
        // own Cb encoding -- a drag, not a fresh press. This decoder
        // deliberately doesn't interpret that bit itself (see MouseEvent's
        // own doc comment); just confirms the raw code round-trips.
        assert_eq!(decode_sgr_mouse_final("32;1;1", b'M'), Key::Mouse(MouseEvent { button: 32, col: 1, row: 1, pressed: true }));
    }

    #[test]
    fn decode_sgr_mouse_final_missing_params_is_unknown() {
        assert_eq!(decode_sgr_mouse_final("0;12", b'M'), Key::Unknown);
        assert_eq!(decode_sgr_mouse_final("", b'M'), Key::Unknown);
    }

    #[test]
    fn decode_sgr_mouse_final_non_numeric_params_is_unknown() {
        assert_eq!(decode_sgr_mouse_final("x;y;z", b'M'), Key::Unknown);
    }

    #[test]
    fn mouse_event_recognizes_wheel_up_and_down() {
        let up = MouseEvent { button: 64, col: 1, row: 1, pressed: true };
        let down = MouseEvent { button: 65, col: 1, row: 1, pressed: true };
        assert!(up.is_scroll_up());
        assert!(!up.is_scroll_down());
        assert!(down.is_scroll_down());
        assert!(!down.is_scroll_up());
        // Neither is ever mistaken for a click.
        assert!(!up.is_left_click());
        assert!(!down.is_left_click());
    }

    #[test]
    fn mouse_event_wheel_ignores_modifier_bits() {
        // Shift/Meta/Ctrl (bits 2-4) held during a wheel notch -- still a
        // wheel event, same as is_left_click already ignores them for an
        // ordinary click.
        let shift_wheel_up = MouseEvent { button: 64 | 0x04, col: 1, row: 1, pressed: true };
        assert!(shift_wheel_up.is_scroll_up());
    }

    #[test]
    fn mouse_event_plain_click_is_not_a_wheel_event() {
        let click = MouseEvent { button: 0, col: 1, row: 1, pressed: true };
        assert!(!click.is_scroll_up());
        assert!(!click.is_scroll_down());
    }

    fn make_editor(text: &str, cursor: usize) -> LineEditor {
        LineEditor { buf: text.chars().collect(), cursor }
    }

    #[test]
    fn ghost_text_appears_in_the_composed_output() {
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, " -la", 0, 40, HighlightContext::default(), &[]);
        assert!(out.contains(" -la"), "{out:?}");
    }

    #[test]
    fn ghost_uses_the_existing_dim_grey_convention() {
        // Same (Indexed(8), dim) pair default_style(HighlightKind::Comment)
        // already uses -- vt100::sgr_codes turns that into "0;2;90".
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, " -la", 0, 40, HighlightContext::default(), &[]);
        assert!(out.contains("\x1b[0;2;90m"), "{out:?}");
    }

    #[test]
    fn cursor_walks_back_past_the_ghost_tail() {
        let ed = make_editor("ls", 2);
        let ghost = " -la";
        let out = compose_redraw("$ ", &ed, ghost, 0, 40, HighlightContext::default(), &[]);
        assert!(out.contains(&format!("\x1b[{}D", ghost.chars().count())), "{out:?}");
    }

    // Regression guard for the branch-test swap from `ed.buf.len() <=
    // remaining` to `combined.len() <= remaining`: with no ghost,
    // combined must equal ed.buf exactly, producing byte-for-byte the
    // same output as before this feature existed -- no stray dim
    // styling, no extra cursor-walk distance beyond the ordinary
    // buf.len() - cursor case (which is 0 here, cursor already at the end).
    #[test]
    fn empty_ghost_changes_nothing() {
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, "", 0, 40, HighlightContext::default(), &[]);
        assert!(!out.contains("\x1b[0;2;90m"), "{out:?}");
        assert!(!out.contains('D'), "{out:?}");
    }

    #[test]
    fn ghost_longer_than_remaining_width_truncates_rather_than_wrapping() {
        // prompt "$ " (2 cols) + width 6 -> remaining 4; buffer "ls" (2
        // chars) leaves exactly 2 columns of room. A 10-char ghost must
        // be cut down to those 2 chars, not spill past the pane's width.
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, "0123456789", 0, 6, HighlightContext::default(), &[]);
        assert!(out.contains("01"), "{out:?}");
        assert!(!out.contains("0123"), "{out:?}");
    }

    #[test]
    fn overlong_buffer_suppresses_the_ghost_entirely() {
        // The buffer alone already exceeds `remaining` -- compose_redraw
        // takes the horizontal-scroll branch, which never has any room
        // left for a ghost tail at all. Cursor at 24, the string's own
        // length (end of buffer, same as every other make_editor call
        // here) -- 25 panicked compose_redraw's window math with a
        // subtract-with-overflow, since a cursor a full char past the
        // buffer's end is a state LineEditor's own invariant never
        // allows outside this one test's own typo.
        let ed = make_editor("a very long command line", 24);
        let out = compose_redraw("$ ", &ed, "ignored-ghost", 0, 10, HighlightContext::default(), &[]);
        assert!(!out.contains("ignored-ghost"), "{out:?}");
        assert!(!out.contains("\x1b[0;2;90m"), "{out:?}");
    }

    fn make_registers() -> Registers {
        Registers::new_for_test()
    }

    #[test]
    fn delete_motion_removes_an_exclusive_range_and_writes_the_register() {
        let mut ed = make_editor("foo bar baz", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        let deleted = delete_motion(&mut lb, &mut registers, motion::Motion::WordForward, None, None);
        assert!(deleted);
        assert_eq!(ed.buf.iter().collect::<String>(), "bar baz");
        assert_eq!(ed.cursor, 0);
        let value = registers.read(None);
        assert_eq!(value.text, "foo ");
        assert_eq!(value.shape, RegisterShape::Char);
    }

    #[test]
    fn delete_motion_removes_an_inclusive_range() {
        let mut ed = make_editor("foo bar baz", 4); // 'b' of bar
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        delete_motion(&mut lb, &mut registers, motion::Motion::LineEnd, None, None);
        assert_eq!(ed.buf.iter().collect::<String>(), "foo ");
        assert_eq!(ed.cursor, 3);
        assert_eq!(registers.read(None).text, "bar baz");
    }

    #[test]
    fn delete_motion_on_a_failed_target_deletes_nothing_and_reports_false() {
        let mut ed = make_editor("abc", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        // Left at column 0 never moves -- motion_range returns None.
        let deleted = delete_motion(&mut lb, &mut registers, motion::Motion::Left, None, None);
        assert!(!deleted);
        assert_eq!(ed.buf.iter().collect::<String>(), "abc");
        assert_eq!(registers.read(None).text, "");
    }

    #[test]
    fn delete_motion_linewise_clears_the_whole_single_line_buffer() {
        // `Motion::Down` is a no-op on a single-line buffer (there's only
        // ever one line to move to -- motion_range would report "nothing
        // moved" and delete_motion would correctly do nothing, same as
        // the failed-target test above). `GotoFirstLine` still counts as
        // "moved" here because it also repositions the column to the
        // first non-blank, which is enough to give motion_range a real
        // range to work with while still being Linewise-classified --
        // exercising delete_motion's whole-buffer-clear branch, which
        // ignores the specific from/to columns entirely.
        let mut ed = make_editor("  foo bar", 5);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        let deleted = delete_motion(&mut lb, &mut registers, motion::Motion::GotoFirstLine, None, None);
        assert!(deleted);
        assert!(ed.buf.is_empty());
        assert_eq!(ed.cursor, 0);
        assert_eq!(registers.read(None).shape, RegisterShape::Line);
    }

    #[test]
    fn delete_lines_clears_the_buffer_and_writes_a_linewise_register() {
        let mut ed = make_editor("foo bar", 2);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        delete_lines(&mut lb, &mut registers, None, None);
        assert!(ed.buf.is_empty());
        assert_eq!(ed.cursor, 0);
        let value = registers.read(None);
        assert_eq!(value.text, "foo bar\n");
        assert_eq!(value.shape, RegisterShape::Line);
    }

    #[test]
    fn indent_line_prepends_shiftwidth_spaces() {
        let mut ed = make_editor("foo", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        indent_line(&mut lb);
        assert_eq!(ed.buf.iter().collect::<String>(), "    foo");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn indent_line_is_a_noop_on_an_empty_line() {
        let mut ed = make_editor("", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        indent_line(&mut lb);
        assert!(ed.buf.is_empty());
    }

    #[test]
    fn outdent_line_strips_up_to_shiftwidth_leading_whitespace() {
        let mut ed = make_editor("      foo", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        outdent_line(&mut lb);
        assert_eq!(ed.buf.iter().collect::<String>(), "  foo");

        let mut ed = make_editor("  foo", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        outdent_line(&mut lb);
        assert_eq!(ed.buf.iter().collect::<String>(), "foo");
    }

    #[test]
    fn add_surround_wraps_a_motion_target_with_padding() {
        let mut ed = make_editor("word", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        add_surround(&mut lb, vimkeys::SurroundTarget::Motion(motion::Motion::TextObject(motion::TextObjectKind::Word, false), None), '(');
        assert_eq!(ed.buf.iter().collect::<String>(), "( word )");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn add_surround_tight_closing_variant_inserts_no_padding() {
        let mut ed = make_editor("word", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        add_surround(&mut lb, vimkeys::SurroundTarget::Motion(motion::Motion::TextObject(motion::TextObjectKind::Word, false), None), ')');
        assert_eq!(ed.buf.iter().collect::<String>(), "(word)");
    }

    #[test]
    fn add_surround_yss_line_target_skips_leading_indentation() {
        let mut ed = make_editor("  foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        add_surround(&mut lb, vimkeys::SurroundTarget::Line(None), '"');
        assert_eq!(ed.buf.iter().collect::<String>(), "  \"foo bar\"");
    }

    #[test]
    fn add_surround_on_a_failed_motion_target_is_a_no_op() {
        let mut ed = make_editor("abc", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        add_surround(&mut lb, vimkeys::SurroundTarget::Motion(motion::Motion::Left, None), '(');
        assert_eq!(ed.buf.iter().collect::<String>(), "abc");
    }

    #[test]
    fn delete_surround_removes_the_pair_and_its_padding() {
        let mut ed = make_editor("( word )", 3);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        delete_surround(&mut lb, '(');
        assert_eq!(ed.buf.iter().collect::<String>(), "word");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_surround_quote_pair_leaves_no_padding_to_strip() {
        let mut ed = make_editor(r#""word""#, 3);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        delete_surround(&mut lb, '"');
        assert_eq!(ed.buf.iter().collect::<String>(), "word");
    }

    #[test]
    fn delete_surround_with_no_enclosing_pair_is_a_no_op() {
        let mut ed = make_editor("word", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        delete_surround(&mut lb, '(');
        assert_eq!(ed.buf.iter().collect::<String>(), "word");
    }

    #[test]
    fn change_surround_replaces_the_delimiters_with_the_new_pair() {
        let mut ed = make_editor(r#""word""#, 3);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        change_surround(&mut lb, '"', '\'');
        assert_eq!(ed.buf.iter().collect::<String>(), "'word'");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn change_surround_to_a_padded_bracket_variant() {
        let mut ed = make_editor("(word)", 3);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        change_surround(&mut lb, '(', '{');
        assert_eq!(ed.buf.iter().collect::<String>(), "{ word }");
    }

    #[test]
    fn change_surround_with_no_enclosing_pair_is_a_no_op() {
        let mut ed = make_editor("word", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        change_surround(&mut lb, '(', '{');
        assert_eq!(ed.buf.iter().collect::<String>(), "word");
    }

    #[test]
    fn surround_selections_wraps_every_committed_selection() {
        let mut ed = make_editor("foo bar baz", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = vec![
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 2) },
            motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 8), to: (0, 10) },
        ];
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        surround_selections(&mut lb, ']');
        assert_eq!(ed.buf.iter().collect::<String>(), "[foo] bar [baz]");
        // cursor lands on the leftmost selection's own open delimiter
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn surround_selections_is_a_no_op_when_nothing_is_selected() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        surround_selections(&mut lb, '(');
        assert_eq!(ed.buf.iter().collect::<String>(), "foo bar");
    }

    #[test]
    fn replace_char_replaces_count_characters_and_advances_the_cursor() {
        let mut ed = make_editor("abcdef", 1);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        replace_char(&mut lb, 'x', 3);
        assert_eq!(ed.buf.iter().collect::<String>(), "axxxef");
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn replace_char_refuses_when_not_enough_characters_remain() {
        let mut ed = make_editor("abc", 1);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        replace_char(&mut lb, 'x', 5);
        assert_eq!(ed.buf.iter().collect::<String>(), "abc");
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn toggle_case_toggles_count_characters_and_advances_past_them() {
        let mut ed = make_editor("abcDEF", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        toggle_case(&mut lb, 3);
        assert_eq!(ed.buf.iter().collect::<String>(), "ABCDEF");
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn toggle_case_clamps_to_the_last_character_when_count_runs_past_the_end() {
        let mut ed = make_editor("ab", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        toggle_case(&mut lb, 5);
        assert_eq!(ed.buf.iter().collect::<String>(), "AB");
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn toggle_case_is_a_no_op_on_an_empty_buffer() {
        let mut ed = make_editor("", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        toggle_case(&mut lb, 1);
        assert_eq!(ed.buf.iter().collect::<String>(), "");
    }

    #[test]
    fn case_operator_motion_uppercases_a_word_and_leaves_cursor_at_start() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        case_operator_motion(&mut lb, motion::Motion::WordForward, None, motion::CaseKind::Upper);
        assert_eq!(ed.buf.iter().collect::<String>(), "FOO bar");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn case_operator_motion_on_a_failed_target_is_a_no_op() {
        let mut ed = make_editor("abc", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        case_operator_motion(&mut lb, motion::Motion::Left, None, motion::CaseKind::Upper);
        assert_eq!(ed.buf.iter().collect::<String>(), "abc");
    }

    #[test]
    fn case_operator_lines_transforms_the_whole_buffer() {
        let mut ed = make_editor("Foo Bar", 3);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        case_operator_lines(&mut lb, motion::CaseKind::Toggle);
        assert_eq!(ed.buf.iter().collect::<String>(), "fOO bAR");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn adjust_number_increments_and_positions_cursor_on_the_last_digit() {
        let mut ed = make_editor("count: 41 items", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        adjust_number(&mut lb, 1);
        assert_eq!(ed.buf.iter().collect::<String>(), "count: 42 items");
        assert_eq!(ed.cursor, 8);
    }

    #[test]
    fn adjust_number_decrements_and_can_grow_the_buffer() {
        let mut ed = make_editor("x9", 1); // cursor on the '9'
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        adjust_number(&mut lb, 1);
        assert_eq!(ed.buf.iter().collect::<String>(), "x10");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn adjust_number_with_no_number_on_the_line_is_a_no_op() {
        let mut ed = make_editor("no digits", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        adjust_number(&mut lb, 1);
        assert_eq!(ed.buf.iter().collect::<String>(), "no digits");
    }

    #[test]
    fn redirect_cw_to_ce_only_fires_on_a_non_blank_cursor() {
        let mut ed = make_editor("foo bar", 0); // 'f', non-blank
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        assert_eq!(redirect_cw_to_ce(&lb, &motion::Motion::WordForward), motion::Motion::WordEnd);
        assert_eq!(redirect_cw_to_ce(&lb, &motion::Motion::WordForwardBig), motion::Motion::WordEndBig);
        // Unaffected motions pass through unchanged.
        assert_eq!(redirect_cw_to_ce(&lb, &motion::Motion::LineEnd), motion::Motion::LineEnd);
    }

    #[test]
    fn redirect_cw_to_ce_does_not_fire_on_a_blank_cursor() {
        let mut ed = make_editor("foo bar", 3); // the space between words
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        assert_eq!(redirect_cw_to_ce(&lb, &motion::Motion::WordForward), motion::Motion::WordForward);
    }

    #[test]
    fn change_via_delete_motion_then_insert_mirrors_the_cw_to_ce_redirect() {
        // "cw" from the middle of "foo" should change only through the
        // 'o' -- not through the trailing space into "bar" -- matching
        // vim's own cw-acts-like-ce rule.
        let mut ed = make_editor("foo bar", 1); // 'o' of foo
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        let motion = redirect_cw_to_ce(&lb, &motion::Motion::WordForward);
        delete_motion(&mut lb, &mut registers, motion, None, None);
        assert_eq!(ed.buf.iter().collect::<String>(), "f bar");
        assert_eq!(ed.cursor, 1);
        assert_eq!(registers.read(None).text, "oo");
    }

    fn range(from_col: usize, to_col: usize) -> motion::MotionRange {
        motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, from_col), to: (0, to_col) }
    }

    #[test]
    fn yank_selections_line_concatenates_two_charwise_ranges_with_no_separator() {
        let mut ed = make_editor("foo bar baz", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![range(0, 2), range(4, 6)]; // "foo", "bar"
        let lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        yank_selections_line(&lb, &mut registers, None);
        let value = registers.read(None);
        assert_eq!(value.text, "foobar");
        assert_eq!(value.shape, RegisterShape::Char);
    }

    #[test]
    fn yank_selections_line_shape_is_line_if_any_range_is_linewise() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) }];
        let lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        yank_selections_line(&lb, &mut registers, None);
        assert_eq!(registers.read(None).shape, RegisterShape::Line);
    }

    // Regression-shaped test for the deletion-order reasoning in
    // `delete_selections`' own doc comment: two ranges removed
    // rightmost-first so the leftward one's own columns never shift,
    // with the register holding both pieces concatenated in *commit*
    // order regardless of their spatial order.
    #[test]
    fn delete_selections_removes_every_range_leftmost_cursor_concatenated_register() {
        let mut ed = make_editor("foo bar baz", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![range(0, 2), range(4, 6)]; // "foo", "bar"
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        let deleted = delete_selections(&mut lb, &mut registers, None);
        assert!(deleted);
        assert_eq!(ed.buf.iter().collect::<String>(), "  baz");
        assert_eq!(ed.cursor, 0);
        assert_eq!(registers.read(None).text, "foobar");
    }

    #[test]
    fn delete_selections_linewise_clears_the_whole_buffer_even_mixed_with_charwise() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![range(0, 2), motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) }];
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        assert!(delete_selections(&mut lb, &mut registers, None));
        assert!(ed.buf.is_empty());
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn delete_selections_is_a_no_op_when_nothing_is_selected() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        assert!(!delete_selections(&mut lb, &mut registers, None));
        assert_eq!(ed.buf.iter().collect::<String>(), "foo bar");
    }

    #[test]
    fn put_over_selections_replaces_every_selection_with_the_same_register_text() {
        let mut ed = make_editor("foo bar baz", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![range(0, 2), range(4, 6)]; // "foo", "bar"
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        registers.write(None, RegisterValue { text: "X".to_string(), shape: RegisterShape::Char });
        assert!(put_over_selections(&mut lb, &mut registers, None));
        assert_eq!(ed.buf.iter().collect::<String>(), "X X baz");
        assert_eq!(ed.cursor, 0);
        // The register itself is untouched -- a deliberate departure
        // from real vim's single-selection swap (see this function's own
        // doc comment) -- so the same replacement can be pasted again.
        assert_eq!(registers.read(None).text, "X");
    }

    #[test]
    fn put_over_selections_is_a_no_op_with_an_empty_register() {
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections = vec![range(0, 2)];
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let mut registers = make_registers();
        assert!(!put_over_selections(&mut lb, &mut registers, None));
        assert_eq!(ed.buf.iter().collect::<String>(), "foo bar");
    }

    #[test]
    fn active_visual_range_line_orders_anchor_and_cursor_regardless_of_direction() {
        let mut vk = VimKeys::new();
        vk.begin_visual(RegisterShape::Char, (0, 5));
        let mut ed = make_editor("foo bar baz", 2); // cursor before the anchor
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        let got = active_visual_range_line(&vk, &lb).unwrap();
        assert_eq!(got.from, (0, 2));
        assert_eq!(got.to, (0, 5));
        assert_eq!(got.shape, motion::MotionShape::Inclusive);
    }

    #[test]
    fn apply_motion_or_reselect_extends_visual_selection_to_the_text_object() {
        let mut vk = VimKeys::new();
        vk.begin_visual(RegisterShape::Char, (0, 0));
        let mut ed = make_editor("foo bar baz", 5); // cursor on the 'a' of "bar"
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        apply_motion_or_reselect(&mut vk, &mut lb, motion::Motion::TextObject(motion::TextObjectKind::Word, false), None);
        // anchor jumps to the word's own start, not wherever 'v' was
        // originally pressed -- matches vim's own viw behavior.
        assert_eq!(vk.visual_anchor(), Some((RegisterShape::Char, (0, 4))));
        assert_eq!(lb.cursor(), (0, 6));
    }

    #[test]
    fn apply_motion_or_reselect_records_jump_motions() {
        let mut vk = VimKeys::new();
        let mut ed = make_editor("one two three four", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        apply_motion_or_reselect(&mut vk, &mut lb, motion::Motion::GotoLastLine, None);
        // GotoLastLine on a single-line buffer doesn't move the cursor,
        // but it's still a jump motion -- the pre-motion position (0, 0)
        // is what gets recorded either way.
        assert_eq!(vk.jump_back((0, 0)), Some((0, 0)));
        assert_eq!(lb.get_mark('\''), Some((0, 0)));
    }

    #[test]
    fn apply_motion_or_reselect_does_not_record_non_jump_motions() {
        let mut vk = VimKeys::new();
        let mut ed = make_editor("one two three", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        apply_motion_or_reselect(&mut vk, &mut lb, motion::Motion::WordForward, None);
        assert_eq!(vk.jump_back((0, 4)), None);
    }

    #[test]
    fn apply_motion_or_reselect_falls_through_to_apply_motion_outside_visual_mode() {
        let mut vk = VimKeys::new();
        let mut ed = make_editor("foo bar", 0);
        let mut marks = HashMap::new();
        let mut selections: Vec<motion::MotionRange> = Vec::new();
        let mut lb = LineBuffer { ed: &mut ed, marks: &mut marks, selections: &mut selections };
        apply_motion_or_reselect(&mut vk, &mut lb, motion::Motion::WordForward, None);
        assert_eq!(lb.cursor(), (0, 4));
    }

    #[test]
    fn selection_columns_line_charwise_and_linewise() {
        assert_eq!(selection_columns_line(&range(2, 5), 11), (2, 6));
        let linewise = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) };
        assert_eq!(selection_columns_line(&linewise, 11), (0, 11));
    }

    fn abbrs(pairs: &[(&str, &str)]) -> Vec<Abbr> {
        pairs.iter().map(|(n, e)| Abbr::new(n, e)).collect()
    }

    // The two trigger sites both pass a `&mut Option<LiveSnippet>`;
    // these tests are about plain (placeholder-free) expansions, where it
    // is never written to.
    fn expand(ed: &mut LineEditor, table: &[Abbr]) -> bool {
        let mut snippet = None;
        let expanded = expand_abbr_at_cursor(ed, table, &mut snippet);
        assert!(snippet.is_none(), "a placeholder-free expansion is plain text, not a snippet");
        expanded
    }

    #[test]
    fn expand_abbr_at_cursor_replaces_a_known_command_position_word() {
        let mut ed = make_editor("gco", 3);
        let table = abbrs(&[("gco", "git checkout")]);
        assert!(expand(&mut ed, &table));
        assert_eq!(ed.as_string(), "git checkout");
        assert_eq!(ed.cursor, "git checkout".len());
    }

    #[test]
    fn expand_abbr_at_cursor_does_nothing_for_an_unknown_word() {
        let mut ed = make_editor("nope", 4);
        let table = abbrs(&[("gco", "git checkout")]);
        assert!(!expand(&mut ed, &table));
        assert_eq!(ed.as_string(), "nope");
    }

    #[test]
    fn expand_abbr_at_cursor_does_nothing_in_argument_position() {
        // "gco" here is the *argument* to "echo", not the command itself --
        // fish's own default (`--position command`) leaves this alone, and
        // so does this port (see expand_abbr_at_cursor's own doc comment).
        let mut ed = make_editor("echo gco", 8);
        let table = abbrs(&[("gco", "git checkout")]);
        assert!(!expand(&mut ed, &table));
        assert_eq!(ed.as_string(), "echo gco");
    }

    #[test]
    fn expand_abbr_at_cursor_only_matches_the_word_ending_exactly_at_the_cursor() {
        // Cursor sits right after "gc", not "gco" -- "gco" isn't the word
        // at the cursor at all, so no expansion should fire even though
        // "gco" appears later on the line.
        let mut ed = make_editor("gco", 2);
        let table = abbrs(&[("gco", "git checkout"), ("gc", "git commit")]);
        assert!(expand(&mut ed, &table));
        assert_eq!(ed.as_string(), "git commito");
    }

    #[test]
    fn expand_abbr_at_cursor_is_a_noop_on_an_empty_table() {
        let mut ed = make_editor("gco", 3);
        assert!(!expand(&mut ed, &[]));
        assert_eq!(ed.as_string(), "gco");
    }

    // --- `abbr` snippets -------------------------------------------
    //
    // The keystroke *dispatch* (which key advances, accepts, cancels)
    // lives inline in read_line's own match and needs a real terminal;
    // what's testable here is everything underneath it -- the model
    // (bishedit::snippet's own tests) and the seam between the model and
    // the line editor, which is this.

    fn start_snippet(line: &str, cursor: usize, expansion: &str, order: &[usize]) -> (LineEditor, LiveSnippet) {
        let mut ed = make_editor(line, cursor);
        let table = vec![Abbr { order: order.to_vec(), ..Abbr::new("foo", expansion) }];
        let mut snippet = None;
        assert!(expand_abbr_at_cursor(&mut ed, &table, &mut snippet));
        (ed, snippet.expect("an expansion with placeholders is a snippet"))
    }

    fn type_text(ed: &mut LineEditor, state: &mut LiveSnippet, text: &str) {
        for c in text.chars() {
            state.snip.type_char(c);
            state.sync(ed);
        }
    }

    #[test]
    fn an_expansion_with_placeholders_splices_in_tentatively() {
        let (ed, state) = start_snippet("foo", 3, "bar -x %s -y %s | qoo", &[]);
        assert_eq!(ed.as_string(), "bar -x %s -y %s | qoo");
        assert_eq!(state.holes()[0].0, 7, "the first placeholder starts after `bar -x `");
        // The caret parks at the first placeholder, not at the end of the
        // expansion the way a plain abbreviation leaves it.
        assert_eq!(ed.cursor, 7);
    }

    #[test]
    fn a_snippet_only_ever_rewrites_its_own_span_of_the_line() {
        let (mut ed, mut state) = start_snippet("foo", 3, "cd %s", &[]);
        // Text typed after the abbreviation would normally be to the
        // right; simulate the same thing by expanding mid-line.
        assert_eq!(ed.as_string(), "cd %s");
        type_text(&mut ed, &mut state, "src");
        assert_eq!(ed.as_string(), "cd src");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn accepting_leaves_the_cursor_where_typing_it_out_would_have() {
        let (mut ed, mut state) = start_snippet("foo", 3, "bar -x %s -y %s | qoo", &[]);
        type_text(&mut ed, &mut state, "one");
        state.snip.advance(false);
        state.sync(&mut ed);
        type_text(&mut ed, &mut state, "two");
        state.accept(&mut ed);
        assert_eq!(ed.as_string(), "bar -x one -y two | qoo");
        assert_eq!(ed.cursor, ed.buf.len(), "the cursor lands after the whole expansion, as if typed");
    }

    #[test]
    fn cancelling_puts_the_abbreviation_name_back_verbatim() {
        let (mut ed, mut state) = start_snippet("echo hi; foo", 12, "cd %s", &[]);
        assert_eq!(ed.as_string(), "echo hi; cd %s");
        type_text(&mut ed, &mut state, "src");
        state.cancel(&mut ed);
        assert_eq!(ed.as_string(), "echo hi; foo");
        assert_eq!(ed.cursor, 12);
    }

    #[test]
    fn the_placeholder_layer_marks_the_active_one_differently() {
        let (_, state) = start_snippet("foo", 3, "a %s b %s", &[]);
        let layer = snippet_layer(&state);
        assert_eq!(layer.len(), 2);
        assert!(layer[0].attrs.reverse, "the placeholder being typed into is the reverse-video one");
        assert!(!layer[1].attrs.reverse && layer[1].attrs.underline);
    }

    #[test]
    fn the_layer_is_offset_by_where_the_snippet_actually_sits() {
        let (_, state) = start_snippet("echo hi; foo", 12, "cd %s", &[]);
        let layer = snippet_layer(&state);
        // "echo hi; cd " is 12 chars, then the `%s`.
        assert_eq!((layer[0].start, layer[0].end), (12, 14));
    }

    #[test]
    fn a_reversed_order_fills_the_second_placeholder_first() {
        let (mut ed, mut state) = start_snippet("foo", 3, "bar -x %s -y %s", &[1, 0]);
        type_text(&mut ed, &mut state, "why");
        assert_eq!(ed.as_string(), "bar -x %s -y why", "the caret started on the *second* hole");
        state.snip.advance(false);
        state.sync(&mut ed);
        type_text(&mut ed, &mut state, "ex");
        state.accept(&mut ed);
        assert_eq!(ed.as_string(), "bar -x ex -y why");
    }
}
