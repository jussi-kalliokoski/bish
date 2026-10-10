// The line editor for interactive input: decodes raw terminal bytes into
// keys, keeps an editable buffer with a cursor, and redraws after each
// edit. One line per editing session; repl.rs stitches continuation lines
// into a program.
//
// Redraws fit the columns given (a pane's width or the terminal's),
// truncating the prompt if needed and scrolling the buffer horizontally
// around the cursor. Nothing wraps onto a second row, so a prompt in a
// split pane can't spill into its neighbour.

use std::collections::HashMap;
use std::io::{self, Write};

use crate::bishedit::Buffer;
use crate::bishedit::completion::{self, CompletionCandidate, CompletionProvider, CompletionRequest};
use crate::bishedit::highlight::{self, BashHighlighter, HighlightContext, Highlighter, StyledSpan};
use crate::bishedit::motion;
use crate::bishedit::registers::{RegisterShape, RegisterValue, Registers};
use crate::bishedit::snippet::{self, Abbr, LiveSnippet, Snippet};
use crate::bishedit::suggestion::{SuggestionProvider, SuggestionRequest};
use crate::bishedit::undo::UndoTree;
use crate::bishedit::unicode_width::char_width;
use crate::bishedit::vimkeys::{self, KeyOutcome, Op, VimKeys};
use crate::history::History;
use crate::repl::{erase_global_status_row, render_global_status_row};
use crate::term;
use crate::vt100;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    // Raw NUL, which most terminals send for Ctrl+Space. Still accepted, but
    // the gesture is `Key::CtrlG`: macOS, ibus and fcitx bind Ctrl+Space to
    // switching input source, so it often never arrives.
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
    // `CSI 5 ~` and `CSI 6 ~`; see `decode_csi_final`, and `Motion::PageUp`
    // for where they go.
    PageUp,
    PageDown,
    // `CSI 200 ~` and `CSI 201 ~`, which a terminal puts around pasted text
    // with DECSET 2004 on. They tell a paste from typing, so what reacts to
    // typing (autoindent, `abbr`) can leave a paste alone. Decoded even when
    // the mode is off, since a terminal may send them anyway (tmux passing a
    // neighbour's request through), and "200~" would land in the buffer.
    PasteStart,
    PasteEnd,
    Escape,
    // xterm and tmux send Alt+arrows as `CSI 1 ; 3 <letter>` (3 is Alt); see
    // `decode_csi_final`. Alt-Enter, a newline within the command, arrives as
    // Esc plus CR or LF, like other Alt keys (see `decode_escape_prefixed`).
    AltEnter,
    AltLeft,
    AltRight,
    AltUp,
    CtrlA,
    CtrlB,
    CtrlC,
    CtrlD,
    CtrlE,
    CtrlF,
    // Unbound in bish, decoded so a mapping can use them, such as `<C-s>` for
    // save.
    CtrlG,
    CtrlQ,
    CtrlS,
    CtrlT,
    CtrlK,
    CtrlL,
    // Completion cycling (see `read_line`). BackTab is xterm's CBT
    // (`CSI Z`, Shift-Tab); CtrlN/CtrlP cycle forward and back.
    Tab,
    BackTab,
    CtrlN,
    CtrlP,
    // vim's insert-mode one-shot Normal command; see
    // `run_one_shot_normal_command`.
    CtrlO,
    CtrlU,
    // 0x16: Visual block in Normal mode; unbound at the prompt.
    CtrlV,
    CtrlW,
    /// Alt-Backspace: readline's `backward-kill-word`, stopping at the first
    /// non-alphanumeric character, so it deletes `bar` from `cd foo/bar`.
    AltBackspace,
    // vim's insert-mode register paste; see `read_line`'s `Key::CtrlR` arm.
    CtrlR,
    // Normal mode's scroll one line up; unbound at the prompt.
    CtrlY,
    CtrlZ,
    // Normal mode's number decrement; unbound at the prompt.
    CtrlX,
    // An SGR (mode 1006) mouse report (see `read_sgr_mouse`), decoded so
    // it's consumed whole rather than as stray digit, `;` and `M`
    // keystrokes in any `read_key` loop. repl.rs's `drive_fg_job` forwards
    // raw bytes to a foreground job's pty instead, mouse reports included.
    Mouse(MouseEvent),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseEvent {
    // The raw Cb parameter: the button (bits 0-1, with bit 6 for buttons
    // 4-7), motion with a button held (bit 5), and Shift, Meta, Ctrl (bits
    // 2-4), per xterm's ctlseqs. The accessors below decode what's used.
    pub button: u16,
    // 1-indexed, as the terminal counts (row 1 is the top).
    pub col: u16,
    pub row: u16,
    // `true` for a press or drag (final `M`), `false` for a release (`m`).
    pub pressed: bool,
}

impl MouseEvent {
    // A left-button press: not a drag (bit 5), a wheel (bit 6), another
    // button, or a release.
    pub fn is_left_click(&self) -> bool {
        self.pressed && self.button & 0x60 == 0 && self.button & 0x03 == 0
    }

    // Motion with the left button held: a drag, streamed between press and
    // release once any-event tracking (DECSET 1002) is on.
    pub fn is_left_drag(&self) -> bool {
        self.pressed && self.button & 0x40 == 0 && self.button & 0x20 != 0 && self.button & 0x03 == 0
    }

    // Motion with no button held: hovering. Sent only with DECSET 1003, which
    // `mouse_hover` enables. The low two bits are 3 ("none") as in a release;
    // the motion bit tells them apart.
    pub fn is_hover(&self) -> bool {
        self.pressed && self.button & 0x40 == 0 && self.button & 0x20 != 0 && self.button & 0x03 == 0x03
    }

    // Any release, told by the final byte; which button doesn't matter.
    pub fn is_release(&self) -> bool {
        !self.pressed
    }

    // A wheel notch: bit 6 plus the X11 button in bits 0-1, 4/5 vertical and
    // 6/7 horizontal. Modifiers are masked out, so a Shift-held notch still
    // scrolls; a terminal folding sideways gestures into Shift+wheel gets no
    // horizontal scroll, since a modifier never changes what a notch means.
    // Notches arrive as presses, but `pressed` is checked anyway.
    pub fn is_scroll_up(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x40
    }

    pub fn is_scroll_down(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x41
    }

    pub fn is_scroll_left(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x42
    }

    pub fn is_scroll_right(&self) -> bool {
        self.pressed && self.button & 0x43 == 0x43
    }
}

// How long to wait after a lone Esc for more bytes before taking it as a
// key press: a sequence arrives as a burst, while a person pressing Esc
// has nothing following.
const ESCAPE_TIMEOUT_MS: i32 = 30;

// Reads fd 0 with a raw syscall: `Stdin`'s buffer could hold bytes that
// `term::stdin_ready`'s poll doesn't see, so the poll would lie.
fn read_byte() -> io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    match crate::platform::read_bytes(0, &mut b)? {
        0 => Ok(None),
        _ => Ok(Some(b[0])),
    }
}

// The keys that are one byte with nothing after them, split out of
// `read_key` for testing. 0x07, 0x11, 0x13 and 0x14 mean nothing in bish
// and are decoded so `::bish map` can bind them (`<C-s>` for save).
// 0x11/0x13 (XON/XOFF) arrive only because `term::derive_raw` clears
// IXON.
fn decode_solo_byte(b: u8) -> Option<Key> {
    Some(match b {
        0x00 => Key::CtrlSpace,
        0x01 => Key::CtrlA,
        0x02 => Key::CtrlB,
        0x03 => Key::CtrlC,
        0x04 => Key::CtrlD,
        0x05 => Key::CtrlE,
        0x06 => Key::CtrlF,
        0x07 => Key::CtrlG,
        0x09 => Key::Tab,
        0x0b => Key::CtrlK,
        0x0c => Key::CtrlL,
        0x0e => Key::CtrlN,
        0x0f => Key::CtrlO,
        0x10 => Key::CtrlP,
        0x11 => Key::CtrlQ,
        0x12 => Key::CtrlR,
        0x13 => Key::CtrlS,
        0x14 => Key::CtrlT,
        0x15 => Key::CtrlU,
        0x16 => Key::CtrlV,
        0x17 => Key::CtrlW,
        0x18 => Key::CtrlX,
        0x19 => Key::CtrlY,
        0x1a => Key::CtrlZ,
        _ => return None,
    })
}

// Reads one key from stdin in raw mode (see `term::RawGuard`), so Ctrl-C
// and Ctrl-Z arrive as bytes, not signals. Decodes escape sequences and
// multi-byte UTF-8.
fn read_key() -> io::Result<Option<Key>> {
    let b = match read_byte()? {
        Some(b) => b,
        None => return Ok(None),
    };
    if let Some(key) = decode_solo_byte(b) {
        return Ok(Some(key));
    }
    let key = match b {
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        0x1b => return Ok(Some(read_escape()?)),
        0x20..=0x7e => Key::Char(b as char),
        _ if b >= 0xc0 => Key::Char(read_utf8_cont(b)?),
        _ => Key::Unknown,
    };
    Ok(Some(key))
}

/// The key Esc followed by `b` stands for, `b` being other than the `[`
/// or `O` that starts a CSI sequence. Split out for testing.
fn decode_escape_prefixed(b: u8) -> Option<Key> {
    match b {
        // Alt-Backspace: terminals disagree on Backspace's byte, so both
        // arrive.
        0x7f | 0x08 => Some(Key::AltBackspace),
        // Alt-Enter, in either spelling.
        b'\r' | b'\n' => Some(Key::AltEnter),
        _ => None,
    }
}

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
        _ => return Ok(decode_escape_prefixed(b1).unwrap_or(Key::Unknown)),
    }
    // An SGR mouse report (`\x1b[<Cb;Cx;CyM`) has a `<` after the CSI intro,
    // where every other sequence has a digit or `;`. Checked here, before the
    // parameter loop.
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
        // Not a mouse report: continue as a normal sequence, `b2` the first
        // byte.
        return read_csi_params(String::from(b2 as char));
    }
    read_csi_params(String::new())
}

// Collects parameter bytes (digits and `;`) until the final byte (a
// letter or `~`): plain forms (`\x1b[D`), modified ones xterm and tmux send
// (`\x1b[1;3D` is Alt+Left), and the `\x1b[<n>~` family; see
// `decode_csi_final`. `seed` is what `read_escape` consumed checking for a
// mouse report's `<`.
fn read_csi_params(seed: String) -> io::Result<Key> {
    let mut params = seed;
    if params.chars().next().is_some_and(|c| !c.is_ascii_digit() && c != ';') {
        // The peeked byte was the final byte (a paramless sequence like
        // `\x1b[H`), so there's nothing to collect.
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

// After `\x1b[<`: collects `Cb;Cx;Cy` and a final `M` (press or drag) or
// `m` (release), xterm's SGR mouse protocol (mode 1006). The legacy
// encodings can't go past column 223, and mouse reporting is never
// enabled without SGR mode (see `drive_fg_job`).
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

// `params` is the `Cb;Cx;Cy` between `\x1b[<` and `final_byte` (`M` or
// `m`). Pure, for testing.
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

// `params` are the digit and `;` bytes between `\x1b[` and `final_byte`:
// "" for a plain arrow, "3" for a `~` form like Delete, or "1;3" for
// xterm's modified keys (3 is Alt). The other modifiers (Shift, Ctrl and
// combinations) are treated as the plain key.
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
            Some(200) => Key::PasteStart,
            Some(201) => Key::PasteEnd,
            _ => Key::Unknown,
        },
        b'Z' => Key::BackTab,
        _ => Key::Unknown,
    }
}

// How long to wait for a key before running `on_idle` again (see
// `read_line`), short enough that a job in another window doesn't visibly
// stall. repl.rs's `run_normal_mode_navigation` uses it too.
pub(crate) const IDLE_POLL_MS: i32 = 15;

// Waits for a key in slices of IDLE_POLL_MS, calling `on_idle` between.
// Once a byte is ready, the ordinary blocking reads take over: the rest
// of an escape sequence or UTF-8 character arrives right behind it.
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

    /// Ctrl-W: readline's `unix-word-rubout`, back to whitespace, so
    /// `cd foo/bar` loses the whole path.
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

    /// Alt-Backspace: readline's `backward-kill-word`. Only letters and
    /// digits make a word, so `/`, `.`, `-` and `_` end one: `cd foo/bar`
    /// loses `bar` and keeps `foo/`. Ctrl-W doesn't do that, as in readline
    /// and bash. Pressing it again skips the `/` and takes `foo`, as bash
    /// does.
    fn kill_word_backward_to_punctuation(&mut self) {
        let mut i = self.cursor;
        while i > 0 && !self.buf[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.buf[i - 1].is_alphanumeric() {
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

    // Replaces `buf[start..end]` with `text`, cursor after it. Completion
    // splices candidates in as real text, so typing on accepts one with
    // nothing more to do.
    fn splice_word(&mut self, start: usize, end: usize, text: &str) {
        let new_chars: Vec<char> = text.chars().collect();
        let new_len = new_chars.len();
        self.buf.splice(start..end, new_chars);
        self.cursor = start + new_len;
    }
}

// An in-progress multi-candidate completion. `word_start..cursor` is the
// spliced candidate's span (`splice_word` keeps the cursor in step),
// `original` what was there before (restored by Ctrl-E), and `selected`
// the current candidate. It exists only with several candidates: a single
// one is filled in directly (see `trigger_completion`).
struct CompletionState {
    word_start: usize,
    original: String,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

// Runs the provider and fills in a lone candidate, or splices in the
// first (Tab, Ctrl-N) or last (Shift-Tab, Ctrl-P) of several and returns
// the state for cycling.
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

// Replaces the spliced candidate with the next or previous one, wrapping.
fn cycle_completion(ed: &mut LineEditor, state: &mut CompletionState, backward: bool) {
    let n = state.candidates.len();
    state.selected = if backward { (state.selected + n - 1) % n } else { (state.selected + 1) % n };
    let text = state.candidates[state.selected].display.clone();
    ed.splice_word(state.word_start, ed.cursor, &text);
}

// Computed once per redraw, before dispatch, so the suggestion the Right
// and Ctrl-Y arms accept is exactly what was drawn; computing inside an
// arm could show one in the very keystroke where completion just ended.
//
// Shown only when: a provider was given; no completion is active (a lone
// candidate auto-filled leaves none, so a suggestion may follow); no
// history browse is active (a ghost extending a recalled entry would be
// noise); the cursor is at the end; and the buffer isn't empty.
fn compute_suggestion(ed: &LineEditor, provider: Option<&dyn SuggestionProvider>, completion_active: bool, browsing: bool) -> Option<String> {
    let provider = provider?;
    if completion_active || browsing || ed.buf.is_empty() || ed.cursor != ed.buf.len() {
        return None;
    }
    let line = ed.as_string();
    let suggestion = provider.suggest(SuggestionRequest { line: &line, cursor: ed.cursor })?;
    Some(suggestion.text.chars().skip(ed.buf.len()).collect())
}

// Appends the ghost tail being shown, through `splice_word` like
// completion. Only reachable with the cursor at the end.
fn accept_suggestion(ed: &mut LineEditor, tail: &str) {
    let end = ed.buf.len();
    ed.splice_word(end, end, tail);
}

// The line editor's side of a live snippet: editing lives in
// `snippet::LiveSnippet`, shared with the file editor; this is the
// buffer adapter. `line` is always 0.
impl snippet::SnippetHost for LineEditor {
    fn replace_span(&mut self, from: (usize, usize), to: (usize, usize), text: &str) {
        self.buf.splice(from.1..to.1, text.chars());
    }

    fn place_cursor(&mut self, _line: usize, col: usize) {
        self.cursor = col;
    }

    // One line, so a multi-line snippet folds to spaces here.
    fn is_multiline(&self) -> bool {
        false
    }
}

// The char index in `ed.buf` under terminal column `col`, or `None` for
// the prompt or past the end. Through `char_at_col`, since columns are
// display positions: CJK takes two, a combining mark none.
fn buf_index_at_column(prompt: &str, ed: &LineEditor, col_origin: usize, col: usize) -> Option<usize> {
    let prompt_len = visible_len(prompt);
    let text_start = col_origin + prompt_len;
    if col < text_start {
        return None;
    }
    let idx = crate::bishedit::unicode_width::char_at_col(&ed.buf, col - text_start);
    (idx <= ed.buf.len()).then_some(idx)
}

// A mouse selection, drawn like a Visual one: reversed.
fn selection_layer(ed: &LineEditor, range: (usize, usize)) -> Vec<StyledSpan> {
    let (from, to) = (range.0.min(range.1), range.0.max(range.1).min(ed.buf.len()));
    if from >= to {
        return Vec::new();
    }
    vec![StyledSpan { start: from, end: to, fg: vt100::Color::Default, attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() } }]
}

// A live snippet's placeholders: the active one reversed, as the
// completion menu marks its selection, the rest underlined, all over the
// syntax highlighting.
fn snippet_layer(live: &LiveSnippet) -> Vec<StyledSpan> {
    live.holes()
        .into_iter()
        .map(|(start, end, active)| StyledSpan {
            start: start.1,
            end: end.1,
            fg: vt100::Color::Default,
            attrs: if active {
                vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() }
            } else {
                vt100::CellAttrs { underline: true, ..vt100::CellAttrs::default() }
            },
        })
        .collect()
}

// `abbr` expansion, when the key that ends the word (Space) or submits
// the line (Enter) arrives (see `Shell::abbrs`). Uses completion.rs's
// `find_word_start`/`classify_word_role`, and expands only in command
// position. Returns `true` if it expanded, so Enter expands first and runs
// on the second press. An expansion with tabstops becomes a live snippet,
// reported through `snippet`, which keeps the return a `bool` usable as a
// match guard (`Key::Enter if expand_abbr_at_cursor(..)`).
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
    match Snippet::parse(&abbr.expansion) {
        Some(snip) => *snippet = Some(LiveSnippet::start(snip, 0, word_start, word, ed)),
        // No tabstops: plain text, but with the same escapes, so `\$` is `$`.
        None => ed.splice_word(word_start, ed.cursor, &snippet::flatten(&abbr.expansion)),
    }
    true
}

// `col_origin` is the terminal column this line's column 0 is at: 0
// normally, more inside a pane not at the left edge. `\x1b[nG` (Cursor
// Horizontal Absolute) is used rather than `\r` for that reason.
//
// `width` is the columns of this row that belong to this line (the pane's
// or the terminal's). Erasing writes that many spaces rather than
// `\x1b[K`, which would wipe a neighbouring pane on the same row. It also
// bounds what's printed: autowrap would put overflow into the next row's
// pane, so once the buffer doesn't fit beside the prompt, the buffer
// scrolls horizontally to keep the cursor visible, recomputed each redraw.
//
// `search_matches` are char ranges in `ed.buf` drawn reversed, from
// `run_line_normal_mode`'s vim search, passed to `compose_redraw` as its
// overlay.
fn redraw(
    prompt: &str,
    ed: &LineEditor,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    search_matches: &[(usize, usize)],
) -> io::Result<()> {
    let overlay: Vec<StyledSpan> = search_matches
        .iter()
        .map(|&(start, end)| StyledSpan {
            start,
            end,
            fg: vt100::Color::Default,
            attrs: vt100::CellAttrs { reverse: true, ..vt100::CellAttrs::default() },
        })
        .collect();
    print!("{}", compose_redraw(prompt, ed, "", col_origin, width, ctx, &overlay));
    io::stdout().flush()
}

// The string `redraw` prints, separate so
// `redraw_with_completion_row` can make one write of it plus its own:
// two writes showed the cursor jumping between them on every key.
//
// `ghost` is a suggestion's full dimmed tail, drawn after the buffer and
// truncated here (see `ghost_room`); only non-empty with the cursor at
// the end. `overlay` is what to mark on top (search matches, a snippet's
// placeholders; see `snippet_layer`), composed last so it wins over
// highlighting and the ghost, as vim's `hlsearch` does.
fn compose_redraw(
    prompt: &str,
    ed: &LineEditor,
    ghost: &str,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    overlay: &[StyledSpan],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("\x1b[{}G", col_origin + 1));
    out.push_str(&" ".repeat(width));
    out.push_str(&format!("\x1b[{}G", col_origin + 1));

    let prompt_len = visible_len(prompt);
    if prompt_len >= width {
        // The prompt alone doesn't fit, so show what does; no room is left
        // for the buffer anyway.
        out.push_str(&truncate_visible(prompt, width));
        out.push_str("\x1b[0m");
        return out;
    }

    out.push_str(prompt);
    let remaining = width - prompt_len;

    // The whole buffer is highlighted, then the visible window's cells
    // sliced out, so a span starting before the window still renders from
    // its first column.
    let buf_text: String = ed.buf.iter().collect();
    let raw = BashHighlighter.highlight(&buf_text, ctx);
    // A path resolved against the cwd carries its `file://` URL, and a typed
    // URL is one: both become terminal hyperlinks (see
    // `highlight::render_linked`).
    let mut links: Vec<highlight::LinkSpan> = if !ctx.hyperlinks {
        Vec::new()
    } else {
        raw.iter().filter_map(|s| Some(highlight::LinkSpan { start: s.start, end: s.end, url: s.link.clone()? })).collect()
    };
    if ctx.hyperlinks {
        for found in crate::url::find(&buf_text) {
            let url: String = buf_text.chars().skip(found.start).take(found.end - found.start).collect();
            links.push(highlight::LinkSpan { start: found.start, end: found.end, url });
        }
    }
    let styled: Vec<StyledSpan> = raw
        .into_iter()
        .map(|s| {
            let (fg, attrs) = highlight::resolve_style(s.kind, ctx.color_overrides);
            StyledSpan { start: s.start, end: s.end, fg, attrs }
        })
        .collect();

    // `combined` is the buffer plus as much of the ghost as fits in what's
    // left after the prompt, known only here; with no room the ghost is
    // dropped rather than wrapped into a neighbour. Drawn as two layers with
    // `compose`. With no ghost it's just the buffer.
    let ghost_room = remaining.saturating_sub(ed.buf.len());
    let mut combined = ed.buf.clone();
    combined.extend(ghost.chars().take(ghost_room));
    let ghost_layer = if combined.len() == ed.buf.len() {
        Vec::new()
    } else {
        // The (fg, attrs) of `default_style(HighlightKind::Comment)`, the
        // greyed look.
        vec![StyledSpan {
            start: ed.buf.len(),
            end: combined.len(),
            fg: vt100::Color::Indexed(8),
            attrs: vt100::CellAttrs { dim: true, ..vt100::CellAttrs::default() },
        }]
    };
    // `overlay` last, so the caller's marks (search matches, snippet
    // placeholders) win over the highlighting of a line that's often not
    // valid syntax yet.
    let cells = highlight::compose(&combined, &[&styled, &ghost_layer, overlay]);

    if combined.len() <= remaining {
        out.push_str(&highlight::render_linked(&cells, &links));
        // Moves the cursor back past the ghost to `ed.cursor`: as far as the
        // cursor is concerned, the ghost isn't there.
        let back = combined.len() - ed.cursor;
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    } else {
        // Right-aligns the window on the cursor when it would fall outside,
        // clamped so the window never scrolls past the buffer's tail.
        let window_start = ed.cursor.saturating_sub(remaining - 1).min(ed.buf.len() - remaining);
        let window_end = (window_start + remaining).min(ed.buf.len());
        // Rebased onto the window, as the cells are, so a link starting
        // before it still links the part that shows.
        let windowed: Vec<highlight::LinkSpan> = links
            .iter()
            .filter(|l| l.start < window_end && l.end > window_start)
            .map(|l| highlight::LinkSpan {
                start: l.start.saturating_sub(window_start),
                end: (l.end - window_start).min(window_end - window_start),
                url: l.url.clone(),
            })
            .collect();
        out.push_str(&highlight::render_linked(&cells[window_start..window_end], &windowed));
        let back = (window_end - window_start) - (ed.cursor - window_start);
        if back > 0 {
            out.push_str(&format!("\x1b[{}D", back));
        }
    }

    out
}

// A completion menu row: the candidates joined by spaces, a bold span
// per matched character, and the selected candidate's span, drawn
// reversed and anchoring the width clamp. As separate layers, the
// reversed span covers the selected candidate's bold matches, which is
// fine since it already stands out.
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

/// Repaints the prompt line with only what was typed: no suggestion,
/// snippet placeholders or mouse selection. A suggestion drawn after the
/// cursor would otherwise stay in the scrollback as a command nobody ran.
/// `compose_redraw` blanks its own columns, so no erase is needed.
fn repaint_without_affordances(prompt: &str, ed: &LineEditor, col_origin: usize, width: usize, ctx: HighlightContext) -> String {
    compose_redraw(prompt, ed, "", col_origin, width, ctx, &[])
}

/// Blanks the completion menu row, right after the `\r\n` that ends a
/// prompt. What runs next writes from the cursor and leaves what it
/// doesn't cover, so the candidates would outlive the shell that drew
/// them. Only the columns this prompt drew, never `\x1b[K`, which would
/// reach the next pane; the cursor goes back to column 0.
///
/// Plain mode only: in grid mode the compositor repaints the pane, and on
/// the pane's last row this would write onto the tab bar.
fn erase_menu_row_under_cursor(
    menu_capable: bool,
    menu_was_shown: bool,
    row_origin: Option<(usize, usize)>,
    col_origin: usize,
    width: usize,
) -> String {
    match menu_capable && menu_was_shown && row_origin.is_none() {
        true => format!("\x1b[{}G{}\r", col_origin + 1, " ".repeat(width)),
        false => String::new(),
    }
}

// The completion menu row's styled content, without positioning: shared
// by `redraw_with_completion_row`'s relative (plain) and absolute (grid)
// paths.
fn render_menu_row_content(state: &CompletionState, width: usize) -> String {
    let row = build_completion_row(state);
    let cells = highlight::compose(&row.chars, &[&row.bold, &row.selected]);
    let visible: &[vt100::Cell] = if row.chars.len() <= width {
        &cells
    } else {
        // Right-anchored on the selected candidate's end, so a long list
        // scrolls within `width` rather than wrapping into a neighbouring
        // pane.
        let window_start = row.selected_range.end.saturating_sub(width).min(row.chars.len() - width);
        let window_end = (window_start + width).min(row.chars.len());
        &cells[window_start..window_end]
    };
    highlight::render_styled(visible)
}

// `redraw` plus one row for the completion menu, reached by relative
// movement (`\n` down, `\x1b[1A` up) rather than saved cursor positions,
// which a scroll invalidates.
//
// That isn't free either: on the terminal's bottom row, `\n` scrolls the
// screen but leaves the cursor's row index, so `\x1b[1A` lands on the
// shifted content and overwrites it, losing a row. So this runs only
// while a menu is relevant (`completion.is_some() || menu_was_shown`),
// not on every key, which makes that loss rare; `menu_was_shown` keeps
// one more pass to erase the row after a completion ends. The second
// `compose_redraw` restores the column.
//
// Down is a literal `\n`, not `\x1b[1B`, which clamps at the bottom margin
// instead of scrolling and overwrote the previous command's output.
// Everything is one write, so the cursor never visibly jumps.
fn redraw_with_completion_row(
    prompt: &str,
    ed: &LineEditor,
    ghost: &str,
    completion: Option<&CompletionState>,
    // Whether a menu showed on the previous redraw: the row has to be erased
    // once more after a completion ends, though `completion` is `None` by
    // then.
    menu_was_shown: bool,
    menu_capable: bool,
    // `Some((prompt_row, pane_bottom_row))` in grid mode (an unsplit window;
    // see `read_line`): absolute 0-indexed terminal rows, computed by repl.rs
    // from the session's `vt100::Screen` cursor before the call and constant
    // throughout. `pane_bottom_row` is the pane's last row, never the tab
    // bar's; without room below the prompt the menu is skipped. `None` in
    // plain mode, and on the colon line.
    row_origin: Option<(usize, usize)>,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    // A live snippet's placeholder marking, usually empty; see
    // `snippet_layer`.
    overlay: &[StyledSpan],
) -> io::Result<()> {
    let mut out = compose_redraw(prompt, ed, ghost, col_origin, width, ctx, overlay);
    if menu_capable && (completion.is_some() || menu_was_shown) {
        match row_origin {
            // Grid mode: absolute positioning only. A real scroll here
            // would desync the terminal from the session's grid and corrupt
            // the tab bar or a neighbouring pane; `\x1b[{row};{col}H` is
            // unambiguous.
            Some((prompt_row, pane_bottom_row)) => {
                let menu_row = prompt_row + 1;
                if menu_row <= pane_bottom_row {
                    out.push_str(&format!("\x1b[{};{}H", menu_row + 1, col_origin + 1));
                    out.push_str(&" ".repeat(width));
                    out.push_str(&format!("\x1b[{};{}H", menu_row + 1, col_origin + 1));
                    if let Some(state) = completion {
                        out.push_str(&render_menu_row_content(state, width));
                    }
                    // An absolute jump back fixes row and column at once.
                    out.push_str(&format!("\x1b[{};{}H", prompt_row + 1, col_origin + 1));
                    out.push_str(&compose_redraw(prompt, ed, ghost, col_origin, width, ctx, overlay));
                }
                // No room below the prompt in this pane: no menu this time.
            }
            // Plain mode: the relative-movement sequence.
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

// How many columns `s` takes when drawn, ignoring its SGR codes (the only
// escapes a prompt holds). Each character counts its display width, and
// clusters count once (see `unicode_width::str_width`), since the cwd in a
// prompt can hold CJK or emoji. The escapes are stripped into a char
// buffer first, because finding cluster boundaries looks several
// codepoints back. repl.rs uses it to place a frozen prompt's cursor.
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

// The prefix of `s` at most `max_visible` columns wide, keeping its SGR
// codes, which cost nothing. Taken by whole grapheme clusters, finding
// boundaries among the visible characters only (an escape between a base
// and its mark doesn't split them): half a cluster is a different
// character. A wide character that would only partly fit is dropped,
// since callers tolerate one column short, never one over.
fn truncate_visible(s: &str, max_visible: usize) -> String {
    enum Piece {
        /// An SGR sequence, kept as written and costing nothing.
        Escape(String),
        /// One visible character and its index among the visible ones, where
        /// cluster boundaries are found.
        Visible(char, usize),
    }

    let mut pieces: Vec<Piece> = Vec::new();
    let mut visible: Vec<char> = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut escape = String::from(c);
            escape.push(chars.next().expect("peeked"));
            for c2 in chars.by_ref() {
                escape.push(c2);
                if c2 == 'm' {
                    break;
                }
            }
            pieces.push(Piece::Escape(escape));
            continue;
        }
        pieces.push(Piece::Visible(c, visible.len()));
        visible.push(c);
    }

    // Where each cluster starts; the other characters belong to the cluster
    // before and follow its fate.
    let mut starts = vec![false; visible.len()];
    let mut i = 0;
    while i < visible.len() {
        starts[i] = true;
        i = crate::bishedit::grapheme::next_boundary(&visible, i);
    }

    let mut out = String::new();
    let mut used = 0;
    for piece in pieces {
        match piece {
            Piece::Escape(escape) => out.push_str(&escape),
            Piece::Visible(c, at) => {
                if starts[at] {
                    let width = char_width(c);
                    if used + width > max_visible {
                        break;
                    }
                    used += width;
                }
                out.push(c);
            }
        }
    }
    out
}

pub enum ReadOutcome {
    Line(String),
    /// Text with newlines in it that must not run yet. Without bracketed
    /// paste, a pasted block arrives as characters and Enters, so each line
    /// runs as it lands. With it, the newlines are text: `lines` are the
    /// complete lines, which the caller appends to its continuation buffer
    /// without running, and `rest` is what follows the last newline, which
    /// goes back into the prompt with the cursor at `cursor`.
    ///
    /// Alt-Enter produces one too, with a single line: deliberately adding a
    /// line to a command that would otherwise run.
    Pasted {
        lines: Vec<String>,
        rest: String,
        cursor: usize,
    },
    Eof,
    // Ctrl-C, or the colon line's Escape and Backspace cancel. `text` is what
    // was typed, not to resume (Ctrl-C abandons the line) but so the caller
    // can put what the terminal shows into its grid before repainting:
    // `read_line` draws straight to the screen, and a caller repainting from
    // its grid would otherwise blank the line and lose the `^C`, which bash
    // leaves in the scrollback.
    Interrupted {
        text: String,
    },
    // Alt+Left/Right/Up on an empty buffer; see `DirNav`.
    DirNav(DirNav),
    // Ctrl+G, whatever has been typed: Normal-mode navigation over the
    // pane's content (repl.rs's `run_normal_mode_navigation`), and from
    // there `:` for command mode. `text` and `cursor` are the line at that
    // moment, shown in the pane and handed back through `read_line`'s
    // `initial` when editing resumes; `i`, `a`, `I`, `A`, `s`, `S`, `C` act
    // relative to that cursor.
    //
    // `wheel` is the notch that opened it, if a wheel did. A promoted
    // prompt has mouse reporting on (see `prompt_claims_mouse`), so the
    // terminal doesn't scroll its scrollback, and Normal mode is bish's
    // scrollback view, so the notch that got you there scrolls too.
    NormalMode {
        text: String,
        cursor: usize,
        wheel: Option<MouseEvent>,
    },
    // A left click (see `MouseEvent::is_left_click`) while typing, passed up
    // like `NormalMode`: `read_line` doesn't know about panes, so hit-testing
    // is repl.rs's (`hit_test_click`). `text` and `cursor` keep what was
    // typed; on a focus change repl.rs freezes it into the old pane's grid.
    Mouse {
        event: MouseEvent,
        text: String,
        cursor: usize,
    },
    // Ctrl+T: the fuzzy opener (repl.rs's `run_opener`), passed up since
    // its list is windows and repositories. `text` and `cursor` come back
    // through `initial` if nothing is picked.
    Opener {
        text: String,
        cursor: usize,
    },
    // Ctrl-L when the caller asked for it (`ctrl_l_reports`): command mode's
    // transcript toggle. What was typed is discarded.
    CtrlL,
}

// Directory navigation (back, forward, up), from Alt+Left/Right/Up.
// repl.rs owns the history and the `cd`; this reports the direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DirNav {
    Back,
    Forward,
    Up,
}

/// What the screen shows once a paste is taken in: each complete line in
/// place, as if typed and submitted. The first is already under the
/// primary prompt and is repainted there; the rest get the continuation
/// prompt. Written to the terminal; a caller with a grid feeds them in
/// separately (see repl.rs).
#[allow(clippy::too_many_arguments)]
fn paste_echo(
    prompt: &str,
    continuation_prompt: &str,
    lines: &[String],
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    menu_capable: bool,
    menu_was_shown: bool,
    row_origin: Option<(usize, usize)>,
) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let shown = LineEditor { buf: line.chars().collect(), cursor: 0 };
        let prompt = if i == 0 { prompt } else { continuation_prompt };
        out.push_str(&repaint_without_affordances(prompt, &shown, col_origin, width, ctx));
        out.push_str("\r\n");
    }
    // A menu row sat under the first line, and a second line has already
    // overwritten it.
    if lines.len() == 1 {
        out.push_str(&erase_menu_row_under_cursor(menu_capable, menu_was_shown, row_origin, col_origin, width));
    }
    out
}

/// Where a pasted block leaves the prompt: the finished lines and what's
/// still being edited. Only the paste can contain newlines, so everything
/// before its last newline is complete and the rest is the new line. The
/// cursor lands at the end of the paste, as if typed, with what was right
/// of the cursor still to its right. No lines when the paste has no
/// newline: the caller inserts it normally.
fn splice_paste(buf: &[char], cursor: usize, paste: &str) -> (Vec<String>, String, usize) {
    if !paste.contains('\n') {
        return (Vec::new(), String::new(), 0);
    }
    let mut text: Vec<char> = buf[..cursor].to_vec();
    text.extend(paste.chars());
    let after_paste = text.len();
    text.extend(buf[cursor..].iter().copied());
    let joined: String = text.iter().collect();
    let mut parts: Vec<String> = joined.split('\n').map(str::to_string).collect();
    let rest = parts.pop().unwrap_or_default();
    let before: usize = parts.iter().map(|l| l.chars().count() + 1).sum();
    (parts, rest, after_paste.saturating_sub(before))
}

// Reads one line interactively. Raw mode is on only during the call
// (RawGuard restores it), so a command run between calls sees a normal
// terminal.
//
// `history` is the session's own history chain, so Up/Down see its
// commands and what it inherited. `esc_cancels` makes Esc, outside a
// history browse, cancel like Ctrl-C without the `^C` (command mode, as
// vim's `:`). `ctrl_l_reports` returns Ctrl-L as `ReadOutcome::CtrlL`
// instead of clearing the screen (command mode's transcript toggle).
// `on_idle` runs while waiting for a key, so repl.rs can service other
// windows' jobs; `|| {}` for callers with nothing to service.
// `col_origin` and `width` are this prompt's column 0 and its columns
// (see `redraw`): 0 and the terminal's width unless split. The caller
// positions the cursor's row first. `initial` preloads the buffer, to
// resume after Ctrl+G (see `ReadOutcome::NormalMode`).
pub fn read_line(
    prompt: &str,
    // The prompt for continuation lines, only drawn by `paste_echo`, so the
    // colon line can pass its ordinary prompt.
    continuation_prompt: &str,
    history: &History,
    esc_cancels: bool,
    ctrl_l_reports: bool,
    initial: Option<(String, usize)>,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    // `None` without a shell to complete against (the colon line).
    completion_provider: Option<&dyn CompletionProvider>,
    // `None` likewise.
    suggestion_provider: Option<&dyn SuggestionProvider>,
    menu_capable: bool,
    // See `redraw_with_completion_row`; passed through.
    row_origin: Option<(usize, usize)>,
    // The shell's register table (see `Registers`), for Ctrl-R here and for
    // `y`/`p` in the Ctrl-E and Ctrl-O Normal modes.
    registers: &mut Registers,
    // Abbreviations, snapshotted per prompt (see `Shell::abbrs`); empty on
    // the colon line.
    abbrs: &[Abbr],
    // `Some((term_rows, term_cols))` when Ctrl-E's Normal mode should draw a
    // search being typed on the global status row (as `:` and Ctrl+G's status
    // do), full terminal width, instead of in place of the prompt. `None` on
    // the colon line, which already occupies that row. A snapshot: `on_idle`
    // can resize the terminal, and being one resize stale is acceptable.
    global_row_size: Option<(usize, usize)>,
    // Called on each idle tick while waiting for a key. `true` means the
    // screen was repainted under this line (see repl.rs's closure), so the
    // prompt is drawn again, or job output would erase what was typed.
    on_idle: &mut dyn FnMut() -> bool,
    // The `mouse` bishopt. Off, reporting is never enabled, leaving the
    // terminal's own selection (see `RawGuard::enable_maybe_mouse`).
    mouse: bool,
    // `::bish map -m command`. Empty at the shell prompt, which isn't a
    // remappable mode, making the mechanism inert there.
    mappings: Vec<crate::keymap::Mapping>,
    // Keys a mapping produced elsewhere that still need delivering:
    // `<Esc>:w<CR>` fires `:` here, and the rest arrives with it. Dispatched
    // before reading, never matched, which keeps noremap intact.
    queued: Vec<Key>,
) -> io::Result<ReadOutcome> {
    let mut guard = Some(term::RawGuard::enable_maybe_mouse(0, mouse)?);
    // Bracketed paste for the whole read, so a paste can be told from typing
    // (see `Key::PasteStart`). The guard turns it off on every exit, so a
    // foreground command never inherits it.
    let _paste = term::BracketedPasteGuard::enable();
    let mut matcher = crate::keymap::Matcher::new(mappings);
    // A mapping's expansion beyond its first key, delivered before reading
    // and never matched again, so mappings don't chain.
    let mut mapped: std::collections::VecDeque<Key> = queued.into_iter().collect();
    let mut ed = match initial {
        Some((text, cursor)) => {
            let mut e = LineEditor::new();
            e.buf = text.chars().collect();
            e.cursor = cursor.min(e.buf.len());
            e
        }
        None => LineEditor::new(),
    };

    // Undo history for this line's Ctrl-E and Ctrl-O excursions, shared
    // across all of them, so a `<C-o>dd` can be undone from a later Ctrl-E.
    // Lives as long as the line; `marks` and `selections` only last one
    // excursion, since what motions resolve against is meaningful only there.
    let mut undo: UndoTree<Vec<char>> = UndoTree::new(ed.buf.clone(), (0, ed.cursor));

    // History browsing: Up/Down move through entries starting with what was
    // typed before browsing (`prefix`, restored on Esc or past the newest
    // match). Any other key keeps the shown entry as ordinary text and ends
    // the browse.
    let mut browse: Option<(String, usize)> = None;

    // An in-progress completion (see `CompletionState`); `None` also after a
    // lone candidate was filled in.
    let mut completion: Option<CompletionState> = None;

    // The live `abbr` snippet, if any (see `snippet::LiveSnippet`). Never
    // active together with `completion`.
    let mut snippet: Option<LiveSnippet> = None;

    // Whether a menu showed on the previous redraw, so its row is erased once
    // after a completion ends (see `redraw_with_completion_row`).
    let mut menu_was_shown = false;

    // The ghost tail shown, recomputed each redraw (see
    // `compute_suggestion`).
    let mut suggestion = compute_suggestion(&ed, suggestion_provider, completion.is_some(), browse.is_some());

    // Drawn with the same clear-and-draw as every redraw, even with nothing
    // typed: the cursor may sit after a frozen idle prompt for this pane (see
    // repl.rs's `freeze_idle_prompt`), and a plain print would draw a second
    // one beside it.
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

    // Mouse selection on the prompt line: where the drag started and the
    // range it covers. Here rather than on `LineEditor`, since it's scoped to
    // this line.
    let mut drag_anchor: Option<usize> = None;
    let mut mouse_selection: Option<(usize, usize)> = None;

    // A key Ctrl-E's or Ctrl-O's loop handed back (Ctrl-C/D/Z), run
    // through the same match arms as if read directly.
    let mut pending_key: Option<Key> = None;

    loop {
        let key = match pending_key.take().or_else(|| mapped.pop_front()) {
            Some(k) => k,
            None => {
                // Waiting here rather than in `read_key_idle`'s `on_idle`,
                // because only here is everything the line is drawn from
                // free to redraw. A key a mapping produced isn't on stdin,
                // so it doesn't wait.
                while mapped.is_empty() && !term::stdin_ready(IDLE_POLL_MS) {
                    if on_idle() {
                        let mut overlay = snippet.as_ref().map(snippet_layer).unwrap_or_default();
                        overlay.extend(mouse_selection.into_iter().flat_map(|r| selection_layer(&ed, r)));
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
                    }
                }
                // A byte is ready, so `on_idle` is never called.
                let raw = match read_key_idle(&mut || {})? {
                    Some(k) => k,
                    None => {
                        drop(guard.take());
                        return Ok(ReadOutcome::Eof);
                    }
                };
                if matcher.is_empty() {
                    raw
                } else {
                    // Not speculative: a key taken back on the colon line
                    // would have to be untangled from completion and
                    // snippet state too. Holding costs a key's latency on a
                    // multi-key mapping.
                    let mut out = matcher.feed(raw, "command", false);
                    if out.keys.is_empty() {
                        // Mid-sequence: wait for the key that decides it.
                        continue;
                    }
                    let first = out.keys.remove(0);
                    mapped.extend(out.keys);
                    first
                }
            }
        };
        if !matches!(key, Key::Up | Key::Down | Key::Escape) {
            browse = None;
        }
        // Any key that doesn't cycle, accept or discard a completion keeps
        // the spliced candidate as ordinary text and ends tracking; the
        // buffer is already right, so that's all "keep typing accepts"
        // needs. Ctrl-Y isn't exempt, so it accepts too.
        if completion.is_some() && !matches!(key, Key::Tab | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::CtrlE | Key::Up | Key::Down) {
            completion = None;
        }
        // Likewise for a live snippet: a key outside its own vocabulary
        // (below) accepts it as it stands, then does what it always does.
        if snippet.is_some()
            && !matches!(
                key,
                Key::Tab | Key::BackTab | Key::CtrlN | Key::CtrlP | Key::Enter | Key::CtrlY | Key::CtrlE | Key::Backspace | Key::Char(_)
            )
        {
            snippet.take().unwrap().accept(&mut ed);
        }

        // A mouse selection standing when a printable key or Backspace
        // arrives is replaced, as in the file editor's Insert mode; any
        // other key drops it. Mouse events don't, or a drag would clear its
        // own selection.
        if let Some((a, b)) = mouse_selection
            && !matches!(key, Key::Mouse(_))
        {
            let (from, to) = (a.min(b), a.max(b).min(ed.buf.len()));
            if from < to && matches!(key, Key::Char(_) | Key::Backspace | Key::Delete) {
                ed.buf.drain(from..to);
                ed.cursor = from;
            }
            mouse_selection = None;
            // Backspace's job was deleting the selection; falling through
            // would eat one more character.
            if matches!(key, Key::Backspace | Key::Delete) && from < to {
                let overlay = snippet.as_ref().map(snippet_layer).unwrap_or_default();
                redraw_with_completion_row(prompt, &ed, "", None, menu_was_shown, menu_capable, row_origin, col_origin, width, ctx, &overlay)?;
                continue;
            }
        }

        match key {
            // A live snippet owns these keys. Tab/Shift-Tab and
            // Ctrl-N/Ctrl-P step between placeholders, as they cycle
            // completions; the two are never active together.
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
            // Enter advances like Tab, except on the last placeholder,
            // where it accepts. It doesn't also submit: the line stays to
            // look over, as a plain abbreviation's first Enter leaves it.
            Key::Enter if snippet.as_ref().is_some_and(|s| !s.snip.at_last()) => {
                let state = snippet.as_mut().unwrap();
                state.snip.advance(false);
                state.sync(&mut ed);
            }
            Key::Enter | Key::CtrlY if snippet.is_some() => {
                snippet.take().unwrap().accept(&mut ed);
            }
            // Ctrl-E discards it all and restores the line, abbreviation
            // name and all, as for a completion.
            Key::CtrlE if snippet.is_some() => {
                snippet.take().unwrap().cancel(&mut ed);
            }
            // Backspace only eats what was typed into the active
            // placeholder, never the snippet's literal text, which the
            // model couldn't put back.
            Key::Backspace if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                if state.snip.backspace() {
                    state.sync(&mut ed);
                }
            }
            // Typing, spaces included, fills the active placeholder.
            Key::Char(c) if snippet.is_some() => {
                let state = snippet.as_mut().unwrap();
                state.snip.type_char(c);
                state.sync(&mut ed);
            }

            // If the word at the cursor is an abbreviation in command
            // position, this Enter only expands it; the next Enter, finding
            // nothing to expand, submits.
            Key::Enter if expand_abbr_at_cursor(&mut ed, abbrs, &mut snippet) => {}
            Key::Enter => {
                drop(guard.take());
                print!("{}\r\n{}", repaint_without_affordances(prompt, &ed, col_origin, width, ctx), erase_menu_row_under_cursor(menu_capable, menu_was_shown, row_origin, col_origin, width));
                io::stdout().flush()?;
                return Ok(ReadOutcome::Line(ed.as_string()));
            }
            // The whole paste, read at once rather than key by key.
            // Everything here that reacts to typing (abbreviations,
            // completions, a suggestion's ghost) is wrong for pasted text,
            // and telling the two apart is the point of bracketed paste.
            // Read raw, past the keymap: a mapping is for keys pressed, not
            // clipboard text.
            Key::PasteStart => {
                let mut pasted = String::new();
                loop {
                    match read_key_idle(&mut || {})? {
                        // End of input, or the closing bracket: that's all
                        // of it.
                        None | Some(Key::PasteEnd) => break,
                        Some(Key::Char(c)) => pasted.push(c),
                        // Both line endings arrive as Enter, and in a paste
                        // mean a newline.
                        Some(Key::Enter) => pasted.push('\n'),
                        Some(Key::Tab) => pasted.push('\t'),
                        // Any other key in a paste is a control sequence
                        // with no sensible text.
                        Some(_) => {}
                    }
                }
                let (lines, rest, cursor) = splice_paste(&ed.buf, ed.cursor, &pasted);
                if lines.is_empty() {
                    for c in pasted.chars() {
                        ed.insert(c);
                    }
                } else {
                    drop(guard.take());
                    print!("{}", paste_echo(prompt, continuation_prompt, &lines, col_origin, width, ctx, menu_capable, menu_was_shown, row_origin));
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Pasted { lines, rest, cursor });
                }
            }
            // A newline in the command rather than the end of it, for a
            // command that would otherwise run (an incomplete one already
            // continues).
            Key::AltEnter => {
                let (lines, rest, cursor) = splice_paste(&ed.buf, ed.cursor, "\n");
                drop(guard.take());
                print!("{}", paste_echo(prompt, continuation_prompt, &lines, col_origin, width, ctx, menu_capable, menu_was_shown, row_origin));
                io::stdout().flush()?;
                return Ok(ReadOutcome::Pasted { lines, rest, cursor });
            }
            Key::CtrlC => {
                drop(guard.take());
                print!("{}^C\r\n{}", repaint_without_affordances(prompt, &ed, col_origin, width, ctx), erase_menu_row_under_cursor(menu_capable, menu_was_shown, row_origin, col_origin, width));
                io::stdout().flush()?;
                return Ok(ReadOutcome::Interrupted { text: ed.as_string() });
            }
            Key::CtrlD => {
                if ed.buf.is_empty() {
                    drop(guard.take());
                    print!("{}\r\n{}", repaint_without_affordances(prompt, &ed, col_origin, width, ctx), erase_menu_row_under_cursor(menu_capable, menu_was_shown, row_origin, col_origin, width));
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Eof);
                }
                ed.delete_forward();
            }
            Key::CtrlZ => {
                // Restore the terminal before stopping, so whatever uses
                // the tty meanwhile sees it sane; raw mode and a redraw
                // come back on resume.
                drop(guard.take());
                term::suspend_self();
                guard = Some(term::RawGuard::enable_maybe_mouse(0, mouse)?);
            }
            Key::Backspace => {
                // Only command mode sets `esc_cancels`: as in vim,
                // Backspace on an empty `:` line leaves it (back to Normal
                // mode). Only when the buffer is empty; with text after the
                // cursor it's a no-op below.
                if esc_cancels && ed.buf.is_empty() {
                    drop(guard.take());
                    print!("{}\r\n{}", repaint_without_affordances(prompt, &ed, col_origin, width, ctx), erase_menu_row_under_cursor(menu_capable, menu_was_shown, row_origin, col_origin, width));
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Interrupted { text: ed.as_string() });
                }
                ed.backspace();
            }
            Key::Delete => ed.delete_forward(),
            // Right accepts the suggestion shown. `suggestion` is only
            // `Some` with the cursor at the end, where `move_right` would
            // do nothing, so no movement is shadowed. Ctrl-F stays a plain
            // motion.
            Key::Right if suggestion.is_some() => accept_suggestion(&mut ed, suggestion.as_deref().unwrap()),
            Key::Left | Key::CtrlB => ed.move_left(),
            Key::Right | Key::CtrlF => ed.move_right(),
            Key::Home | Key::CtrlA => ed.cursor = 0,
            Key::End => ed.cursor = ed.buf.len(),
            Key::CtrlK => ed.kill_to_end(),
            Key::CtrlU => ed.kill_to_start(),
            Key::CtrlW => ed.kill_word_backward(),
            Key::AltBackspace => ed.kill_word_backward_to_punctuation(),
            // vim's insert-mode `<C-r>{register}`: reads one key as the
            // register name and inserts its contents. Anything but a plain
            // char cancels, as Esc cancels a pending `<C-r>` in vim.
            Key::CtrlR => {
                if let Some(Key::Char(c)) = read_key_idle(&mut || {
                    on_idle();
                })? {
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
            // Space expands an abbreviation too, but unlike Enter is still
            // inserted after it: `gco` + Space becomes `git checkout `.
            Key::Char(' ') => {
                // A snippet swallows the space: the caret is already in its
                // first placeholder, where a space would be the first thing
                // typed.
                expand_abbr_at_cursor(&mut ed, abbrs, &mut snippet);
                if snippet.is_none() {
                    ed.insert(' ');
                }
            }
            Key::Char(c) => ed.insert(c),
            // With a completion menu open, Up/Down cycle it instead of
            // history.
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
                // An active history browse comes first: Esc cancels it and
                // restores what was typed, unwinding the innermost state
                // before anything else.
                Some((prefix, _)) => ed.set_text(&prefix),
                // Nothing to unwind: for command mode (`esc_cancels`),
                // cancel, as Ctrl-C does.
                None if esc_cancels => {
                    drop(guard.take());
                    print!("\r\n");
                    io::stdout().flush()?;
                    return Ok(ReadOutcome::Interrupted { text: ed.as_string() });
                }
                None => {}
            },
            // Only on an empty buffer: directory navigation replaces typing
            // a command, and mustn't discard what's being typed. No `\r\n`
            // before returning: repl.rs changes directory and calls
            // `read_line` again, whose first paint redraws in place (see
            // `redraw_with_completion_row`), and a newline would push a new
            // prompt line each time.
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
            // No `\r\n`: Normal mode redraws the pane in place (see
            // `run_normal_mode_navigation`). Ctrl+G is the key; Ctrl+Space
            // (NUL) is still accepted from terminals that send it.
            Key::CtrlG | Key::CtrlSpace => {
                drop(guard.take());
                return Ok(ReadOutcome::NormalMode { text: ed.as_string(), cursor: ed.cursor, wheel: None });
            }
            Key::CtrlT => {
                drop(guard.take());
                return Ok(ReadOutcome::Opener { text: ed.as_string(), cursor: ed.cursor });
            }
            // A wheel notch also opens it. Only at a promoted prompt: an
            // unpromoted one never enables mouse reporting, so the terminal
            // scrolls its own scrollback (see repl.rs's
            // `prompt_claims_mouse`).
            Key::Mouse(ev) if ev.is_scroll_up() || ev.is_scroll_down() => {
                drop(guard.take());
                return Ok(ReadOutcome::NormalMode { text: ed.as_string(), cursor: ed.cursor, wheel: Some(ev) });
            }
            // Ctrl-E: with a completion active, discard it and restore the
            // original word; otherwise a line-local vim Normal mode (see
            // `run_line_normal_mode`). End still moves to the end of the
            // line.
            Key::CtrlE if completion.is_some() => {
                let state = completion.take().unwrap();
                ed.splice_word(state.word_start, ed.cursor, &state.original);
            }
            Key::CtrlE => match run_line_normal_mode(&mut ed, prompt, col_origin, width, ctx, registers, &mut undo, global_row_size, &mut || {
                on_idle();
            })? {
                LineNormalExit::ToInsert => {}
                LineNormalExit::Propagate(k) => {
                    pending_key = Some(k);
                    // Neither propagated key reads `suggestion`, but it's
                    // cleared so the local always matches the screen.
                    suggestion = None;
                    continue;
                }
                LineNormalExit::Eof => {
                    drop(guard.take());
                    return Ok(ReadOutcome::Eof);
                }
            },
            // Ctrl-O: vim's insert-mode one-shot Normal command (see
            // `run_one_shot_normal_command`), straight from typing, as in
            // vim.
            Key::CtrlO => {
                if let Some(k) = run_one_shot_normal_command(&mut ed, registers, &mut undo, &mut || {
                    on_idle();
                })? {
                    pending_key = Some(k);
                    suggestion = None; // see Ctrl-E's own propagate arm
                    continue;
                }
            }
            // Tab/Ctrl-N cycle forward, or start a completion;
            // Shift-Tab/Ctrl-P cycle back. Ctrl-Y accepts through the reset
            // rule above.
            Key::Tab | Key::CtrlN => match &mut completion {
                Some(state) => cycle_completion(&mut ed, state, false),
                None => completion = completion_provider.and_then(|p| trigger_completion(&mut ed, p, false)),
            },
            Key::BackTab | Key::CtrlP => match &mut completion {
                Some(state) => cycle_completion(&mut ed, state, true),
                None => completion = completion_provider.and_then(|p| trigger_completion(&mut ed, p, true)),
            },
            // Ctrl-Y accepts a suggestion too. A suggestion only exists
            // without an active completion, so the two Ctrl-Y meanings
            // never meet.
            Key::CtrlY if suggestion.is_some() => accept_suggestion(&mut ed, suggestion.as_deref().unwrap()),
            // A click on this prompt's own line places the cursor and
            // starts a drag; elsewhere it bubbles up for the caller's tab,
            // pane and divider hit-testing (see `ReadOutcome::Mouse`).
            Key::Mouse(ev)
                if ev.is_left_click()
                    && row_origin.is_some_and(|(prompt_row, _)| (ev.row as usize).saturating_sub(1) == prompt_row)
                    && let Some(idx) = buf_index_at_column(prompt, &ed, col_origin, (ev.col as usize).saturating_sub(1)) =>
            {
                ed.cursor = idx;
                drag_anchor = Some(idx);
                mouse_selection = None;
            }
            // ...and dragging selects. A later key replaces the selection,
            // and Ctrl-E's Visual machinery can act on it.
            Key::Mouse(ev)
                if ev.is_left_drag()
                    && let Some(anchor) = drag_anchor
                    && let Some(idx) = buf_index_at_column(prompt, &ed, col_origin, (ev.col as usize).saturating_sub(1)) =>
            {
                ed.cursor = idx;
                mouse_selection = Some((anchor, idx));
            }
            // The selection outlives the release; only the drag ends.
            Key::Mouse(ev) if ev.is_release() && drag_anchor.is_some() => drag_anchor = None,
            Key::Mouse(ev) if ev.is_left_click() => {
                drop(guard.take());
                return Ok(ReadOutcome::Mouse { event: ev, text: ed.as_string(), cursor: ed.cursor });
            }
            // Keys with no meaning at the prompt: PageUp/PageDown (Ctrl+G's
            // Normal mode pages scrollback), and a stray closing paste
            // bracket from a terminal with the mode on for someone else,
            // better dropped than typed.
            Key::AltLeft
            | Key::AltRight
            | Key::AltUp
            | Key::CtrlY
            | Key::CtrlX
            | Key::Mouse(_)
            | Key::PageUp
            | Key::PageDown
            | Key::PasteEnd
            // Visual block on one line would be a plain selection, which
            // `v` gives.
            | Key::CtrlV
            // Decoded so `::bish map` can bind them; unbound here.
            | Key::CtrlQ
            | Key::CtrlS
            | Key::Unknown => {}
        }
        // After the match, so it reflects what the key did, and before the
        // redraw that shows it (see `compute_suggestion`).
        suggestion = compute_suggestion(&ed, suggestion_provider, completion.is_some() || snippet.is_some(), browse.is_some());
        let mut overlay = snippet.as_ref().map(snippet_layer).unwrap_or_default();
        overlay.extend(mouse_selection.into_iter().flat_map(|r| selection_layer(&ed, r)));
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

// How Ctrl-E's line-local Normal mode (`run_line_normal_mode`) ended.
enum LineNormalExit {
    /// A motion or insert command resolved: back to typing, no key to
    /// reprocess.
    ToInsert,
    /// Ctrl-C, Ctrl-D or Ctrl-Z, handed back for the caller to process as if
    /// it had read it.
    Propagate(Key),
    /// Stdin closed mid-excursion.
    Eof,
}

// Visual mode's active, uncommitted selection as a `MotionRange`, as
// repl.rs's `active_visual_range` does (`Char` is `Inclusive`, `Line` is
// `Linewise`); duplicated since the buffer types differ.
fn active_visual_range_line(vk: &VimKeys, lb: &LineBuffer) -> Option<motion::MotionRange> {
    let (shape, anchor) = vk.visual_anchor()?;
    let cursor = lb.cursor();
    let motion_shape = if shape == RegisterShape::Line { motion::MotionShape::Linewise } else { motion::MotionShape::Inclusive };
    let (from, to) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    Some(motion::MotionRange { shape: motion_shape, from, to })
}

// The columns `range` covers on the one line: `Linewise` is the whole
// buffer, `Inclusive` ends at `to.1 + 1`, clamped.
fn selection_columns_line(range: &motion::MotionRange, len: usize) -> (usize, usize) {
    if range.shape == motion::MotionShape::Linewise {
        return (0, len);
    }
    (range.from.1, (range.to.1 + 1).min(len))
}

// The live `/` or `?` pattern while one is typed (else `None`), and the
// match ranges to highlight on the line: for the pattern being typed,
// else the last search's (a `*`/`#` search's word recovered with
// `motion::word_under_cursor`). The pattern replaces the prompt only when
// there's no global row to draw it in (see `run_line_normal_mode`).
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

// Adds every committed selection, and the active one, to `matches`, so
// Visual highlighting reuses the search-match rendering, as in repl.rs.
fn push_selection_matches(lb: &LineBuffer, vk: &VimKeys, matches: &mut Vec<(usize, usize)>) {
    let len = lb.ed.buf.len();
    for range in lb.selections.iter().chain(active_visual_range_line(vk, lb).iter()) {
        matches.push(selection_columns_line(range, len));
    }
}

// Draws (`Some`) or erases (`None`) the search pattern on the global
// status row; nothing when `global_row_size` is `None`. Saves and restores
// the cursor, which must stay where `redraw` left it.
fn draw_global_search_bar(global_row_size: Option<(usize, usize)>, bar_text: Option<&str>) {
    let Some((term_rows, term_cols)) = global_row_size else { return };
    let row = match bar_text {
        Some(text) => render_global_status_row(&pad_to_width(text, term_cols), term_rows),
        None => erase_global_status_row(term_rows),
    };
    print!("\x1b7{}\x1b8", row);
    let _ = io::stdout().flush();
}

// Pads or truncates `text` to exactly `cols` columns, as
// `render_global_status_row` requires.
fn pad_to_width(text: &str, cols: usize) -> String {
    let len = text.chars().count();
    match len.cmp(&cols) {
        std::cmp::Ordering::Less => format!("{}{}", text, " ".repeat(cols - len)),
        std::cmp::Ordering::Greater => text.chars().take(cols).collect(),
        std::cmp::Ordering::Equal => text.to_string(),
    }
}

// Ctrl-E: a line-local vim Normal mode over the line being typed, with
// no promotion or pane, split or not. Motions and insert commands act on
// the live cursor, since nothing here resumes later (unlike Ctrl+G's
// full-pane mode). Ctrl-E keeps its vim meaning inside; as in vim, Insert
// returns via `i`, `a`, `I`, `A`, `s`, `S`, `C`.
#[allow(clippy::too_many_arguments)]
fn run_line_normal_mode(
    ed: &mut LineEditor,
    prompt: &str,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    registers: &mut Registers,
    undo: &mut UndoTree<Vec<char>>,
    // `Some((term_rows, term_cols))` draws a pattern being typed on the
    // global status row (`repl::render_global_status_row`), full terminal
    // width, instead of replacing the prompt. `None` replaces the prompt.
    global_row_size: Option<(usize, usize)>,
    on_idle: &mut dyn FnMut(),
) -> io::Result<LineNormalExit> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
    // Visual mode's committed selections, empty for each excursion, like
    // `marks`.
    let mut selections: Vec<motion::MotionRange> = Vec::new();
    // Checkpoint what ordinary typing added first: otherwise, if the first
    // edit here happens to recreate the stale undo content, `checkpoint`
    // sees no change and the typing and edit are lost as separate steps.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    // The reverse-video prompt marks the mode. Not a cursor-shape change
    // (DECSCUSR), which is global state with no way to restore the user's
    // own setting.
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
            // A search being typed keeps Ctrl-C (see vimkeys.rs's
            // `feed_search`).
            Key::CtrlC | Key::CtrlD | Key::CtrlZ if !vk.is_search_pending() => break LineNormalExit::Propagate(key),
            // Visual `Z`, `y`, `d`, `c`, `p`, `P` and Escape are handled
            // here before `vk.feed`, as in repl.rs: whether a selection
            // exists is `LineBuffer` state vimkeys never sees. Only at an
            // idle point, so mid-prefix they keep their ordinary meaning.
            // `Z` (single) commits the active selection and returns to
            // Normal mode, keeping selections highlighted for the next `v`.
            Key::Char('Z') if vk.is_idle() && vk.is_visual() => {
                let lb = LineBuffer { ed, marks: &mut marks, selections: &mut selections };
                if let Some(range) = active_visual_range_line(&vk, &lb) {
                    lb.selections.push(range);
                }
                let end_cursor = lb.cursor();
                vk.end_visual(end_cursor);
            }
            // Visual `y`: yanks every committed selection and the active
            // one as one register value, then clears them; reachable during
            // `v`/`V` or right after a `Z`. Otherwise ordinary `y`.
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
            // `d`: deletes them likewise, after writing them to a register.
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
            // `c`: like `d`, then insert mode, as vim's Visual `c`.
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
            // `p`/`P`: replaces each selection with the register's content
            // (see `put_over_selections`).
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
            // `S`: vim-surround's wrap, reading the delimiter key directly.
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
            // Escape: drops every selection, back to plain Normal mode.
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
                        // The motion is ignored on one line (see
                        // `indent_line`).
                        Op::Indent => indent_line(&mut lb),
                        Op::Outdent => outdent_line(&mut lb),
                        // One line has nothing to fold.
                        Op::Fold => {}
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
                        Op::Fold => {}
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
                    // `v`/`V`: arms Visual mode with the cursor as anchor
                    // (vimkeys can't read it). Highlighting and the
                    // selection keys are handled by the arms above.
                    KeyOutcome::EnterVisual(shape) => {
                        vk.begin_visual(shape, lb.cursor());
                    }
                    KeyOutcome::ReselectVisual => {
                        if let Some((shape, anchor, cursor)) = vk.last_visual() {
                            lb.set_cursor(cursor.0, cursor.1);
                            vk.begin_visual(shape, anchor);
                        }
                    }
                    KeyOutcome::SwapVisualEnds => {
                        if let Some((_, anchor)) = vk.visual_anchor() {
                            let cursor = lb.cursor();
                            lb.set_cursor(anchor.0, anchor.1);
                            vk.set_visual_anchor(cursor);
                        }
                    }
                    // Nothing to ask, nowhere to go, and nothing to fold on
                    // one line.
                    KeyOutcome::GotoDefinition(_)
                    | KeyOutcome::GotoReferences
                    | KeyOutcome::DocumentSymbols
                    | KeyOutcome::CodeActions
                    | KeyOutcome::Fold(..) => {}
                    KeyOutcome::Jump { forward } => {
                        let current = lb.cursor();
                        let target = if forward { vk.jump_forward(current) } else { vk.jump_back(current) };
                        if let Some((row, col)) = target {
                            lb.set_cursor(row, col);
                        }
                    }
                    // `<C-w>` keeps its vim meaning, a harmless no-op
                    // without panes, and `Join`/`OpenLine` are no-ops on
                    // one line.
                    KeyOutcome::AddSurround { target, ch } => add_surround(&mut lb, target, ch),
                    KeyOutcome::DeleteSurround { ch } => delete_surround(&mut lb, ch),
                    KeyOutcome::ChangeSurround { ch, replacement } => change_surround(&mut lb, ch, replacement),
                    KeyOutcome::ReplaceChar { ch, count } => replace_char(&mut lb, ch, count.unwrap_or(1).max(1)),
                    // `R` degrades to `i` at the cursor rather than
                    // overtyping: real Replace-mode typing would have to
                    // live in `read_line`'s typing loop, the shell's
                    // busiest path. The file editor's `run_insert_mode` has
                    // its own loop and does it properly.
                    KeyOutcome::EnterReplace => break LineNormalExit::ToInsert,
                    KeyOutcome::ToggleCase { count } => toggle_case(&mut lb, count.unwrap_or(1).max(1)),
                    KeyOutcome::AdjustNumber { delta } => adjust_number(&mut lb, delta),
                    // Not during Visual mode, as in repl.rs: vim's Visual
                    // `u`/`U` change case, which isn't implemented, so `u`
                    // does nothing rather than undo.
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
                    // `g-`/`g+` can land on another branch (see
                    // `UndoTree::time_travel_back`). Same Visual guard as
                    // `u`.
                    KeyOutcome::UndoSeq { forward, count } if !vk.is_visual() && lb.selections.is_empty() => {
                        for _ in 0..count.unwrap_or(1).max(1) {
                            let snap = if forward { undo.time_travel_forward() } else { undo.time_travel_back() };
                            let Some(snap) = snap else { break };
                            lb.ed.buf = snap.content.clone();
                            lb.ed.cursor = snap.cursor.1;
                        }
                    }
                    KeyOutcome::Undo(_) | KeyOutcome::Redo(_) | KeyOutcome::UndoSeq { .. } => {}
                    // This mode is line-local, and `Window` and `Opener`
                    // are about panes; Ctrl+T opens the opener at the
                    // prompt and in a pane's Normal mode.
                    KeyOutcome::Window(..)
                    | KeyOutcome::Opener
                    | KeyOutcome::Join { .. }
                    | KeyOutcome::OpenLine { .. }
                    | KeyOutcome::Pending
                    | KeyOutcome::None => {}
                }
            }
        }
        // Commits an undo node if this key changed `ed.buf`, once per key,
        // as repl.rs's `render_nav_frame` does (see
        // `TextBuffer::checkpoint_undo`).
        undo.checkpoint(&ed.buf, (0, ed.cursor));
        let (bar_text, matches) = normal_mode_prompt_and_matches(ed, &mut marks, &mut selections, &vk);
        let shown = if global_row_size.is_some() { decorated_prompt.clone() } else { bar_text.clone().unwrap_or_else(|| decorated_prompt.clone()) };
        redraw(&shown, ed, col_origin, width, ctx, &matches)?;
        draw_global_search_bar(global_row_size, bar_text.as_deref());
    };
    // Every `break` skips the loop's trailing `draw_global_search_bar`, so
    // erase the bar here, or it would stay on the global row.
    draw_global_search_bar(global_row_size, None);
    Ok(exit)
}

// Applies a motion, except a `Motion::TextObject` in Visual mode (`viw`,
// `va(`), which moves both ends: the anchor to the object's start, the
// cursor to its end. vimkeys can only return a plain `Motion` there.
// Also records `motion::is_jump` motions for `Ctrl-O`/`Ctrl-I` and the
// `'` mark (see `VimKeys::push_jump`), since every jump in every buffer
// context passes through here. An operator's text object never gets here
// (`motion::motion_range` handles it). Shared with repl.rs's ScreenBuffer
// Visual mode.
pub fn apply_motion_or_reselect(vk: &mut VimKeys, buf: &mut dyn crate::bishedit::Buffer, m: motion::Motion, count: Option<usize>) {
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
        // Applied before overwriting the `'` mark: `` ` ` `` and `''` read
        // that mark, and writing it first would make them jump to where
        // they already are.
        motion::apply_motion(buf, m, count);
        buf.set_mark('\'', pos);
        return;
    }
    motion::apply_motion(buf, m, count);
}

// The `y{motion}`/`yy` glue, shared by the line editor's modes and repl.rs's
// ScreenBuffer Normal mode (yank only there). `put` stays private: it
// works on a `LineEditor` directly.
pub fn yank_motion(
    buf: &mut dyn crate::bishedit::Buffer,
    registers: &mut Registers,
    motion: motion::Motion,
    count: Option<usize>,
    register: Option<char>,
) {
    let Some(range) = motion::motion_range(buf, motion, count) else {
        return;
    };
    let text = motion::extract_text(buf, &range);
    let shape = if range.shape == motion::MotionShape::Linewise { RegisterShape::Line } else { RegisterShape::Char };
    registers.record_yank(register, RegisterValue { text, shape });
}

pub fn yank_lines(buf: &dyn crate::bishedit::Buffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
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

// `d{motion}`/`c{motion}`: computes `yank_motion`'s range, writes it to a
// register, removes it, and leaves the cursor at its start, as vim does.
// Not generic: only a `LineEditor` can be edited. Returns whether anything
// was deleted, so a `c{motion}` whose target fails changes nothing and
// doesn't enter insert mode.
fn delete_motion(lb: &mut LineBuffer, registers: &mut Registers, motion: motion::Motion, count: Option<usize>, register: Option<char>) -> bool {
    let Some(range) = motion::motion_range(lb, motion, count) else {
        return false;
    };
    let text = motion::extract_text(lb, &range);
    let shape = if range.shape == motion::MotionShape::Linewise { RegisterShape::Line } else { RegisterShape::Char };
    registers.record_delete(register, RegisterValue { text, shape });
    let (from_col, to_col) = (range.from.1, range.to.1);
    match range.shape {
        // On a one-line buffer, linewise is the whole buffer, as for `yy`.
        motion::MotionShape::Linewise => {
            lb.ed.buf.clear();
            lb.ed.cursor = 0;
        }
        // A block on one line is that line's inclusive span.
        motion::MotionShape::Inclusive | motion::MotionShape::Blockwise => {
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

// `dd`/`cc`: the whole buffer (see `delete_motion`). Always succeeds, so
// `cc` always enters insert.
fn delete_lines(lb: &mut LineBuffer, registers: &mut Registers, count: Option<usize>, register: Option<char>) {
    let text = motion::whole_lines(lb, count.unwrap_or(1).max(1));
    registers.record_delete(register, RegisterValue { text, shape: RegisterShape::Line });
    lb.ed.buf.clear();
    lb.ed.cursor = 0;
}

// `>`/`<` and `>>`/`<<`: shift the one line by a shiftwidth; the count and
// motion have nothing more to reach. An empty line is left alone, as in
// `fileeditor::indent_rows`.
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

// Visual `y`: every selection in commit order, concatenated, as repl.rs's
// `yank_selections` does.
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

// Visual `d`: writes the selections to a register (vim's delete always
// yanks) and removes them. Returns whether anything was selected.
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

    // A `Linewise` selection covers the whole buffer and subsumes the rest.
    if lb.selections.iter().any(|r| r.shape == motion::MotionShape::Linewise) {
        lb.ed.buf.clear();
        lb.ed.cursor = 0;
        return true;
    }

    // Removed from the rightmost start column first, so pending ones keep
    // their columns; `leftmost` comes from the original ranges, whose
    // leftmost start never moves.
    let leftmost = lb.selections.iter().map(|r| r.from.1).min().unwrap_or(0);
    let mut ranges = lb.selections.clone();
    crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from.1));
    for range in &ranges {
        // Re-clamped each time, so even overlapping selections can't panic.
        let len = lb.ed.buf.len();
        let start = range.from.1.min(len);
        let end = (range.to.1 + 1).min(len).max(start);
        lb.ed.buf.drain(start..end);
    }
    lb.ed.cursor = leftmost.min(lb.ed.buf.len().saturating_sub(1));
    true
}

// Visual `S{ch}`: vim-surround's wrap, around every committed selection
// and the active one, rightmost first so pending ones keep their columns.
// The cursor lands on the leftmost new open delimiter.
fn surround_selections(lb: &mut LineBuffer, ch: char) {
    if lb.selections.is_empty() {
        return;
    }
    let Some((open, close)) = motion::surround_delims(ch) else {
        return;
    };
    let mut ranges = lb.selections.clone();
    crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from.1));
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

// Visual `p`/`P` (the same in Visual mode): replaces every selection with
// the register's content. vim swaps the replaced text into the register;
// with several selections, putting the same text into each is the useful
// reading, and the register is kept for the next put. Returns whether
// anything was replaced.
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
    crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from.1));
    let mut leftmost_insert_at = 0;
    for range in &ranges {
        let len = lb.ed.buf.len();
        let start = range.from.1.min(len);
        let end = (range.to.1 + 1).min(len).max(start);
        lb.ed.buf.splice(start..end, insert_chars.iter().copied());
        leftmost_insert_at = start;
    }
    // On the last character of the leftmost replacement, as `apply_put` does.
    lb.ed.cursor = (leftmost_insert_at + insert_chars.len()).saturating_sub(1).min(lb.ed.buf.len().saturating_sub(1));
    true
}

// The `ys`/`yss` target: a motion's range via `motion::motion_range`, or
// for `yss` the whole one-line buffer, linewise (see fileeditor.rs's
// multi-line `resolve_surround_target`).
fn resolve_surround_target(lb: &mut LineBuffer, target: &vimkeys::SurroundTarget) -> Option<motion::MotionRange> {
    match target {
        vimkeys::SurroundTarget::Motion(m, count) => motion::motion_range(lb, m.clone(), *count),
        vimkeys::SurroundTarget::Line(_) => Some(motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (0, 0) }),
    }
}

// `ys{motion}{ch}`/`yss{ch}`: wraps the range in `ch`'s pair, close first
// so the open column doesn't move (see `motion::surround_insert_points`).
// The cursor lands on the open delimiter, as in vim-surround.
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

// `ds{ch}`: removes the enclosing pair and the padding
// `motion::surround_delete_spans` picks, close side first. A no-op
// without a valid target or pair.
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

// `cs{ch}{replacement}`: replaces the pair's delimiters, leaving padding
// alone.
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

// `Ctrl-A`/`Ctrl-X`: adds `delta` to the number at or after the cursor,
// leaving the cursor on its last digit. A no-op without one.
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
        Op::Yank | Op::Delete | Op::Change | Op::Indent | Op::Outdent | Op::Fold => {
            unreachable!("case_kind_for_op is only ever called for Op::Lowercase/Uppercase/CaseToggle")
        }
    }
}

// `gu`, `gU`, `g~` with a motion: changes case over the range, cursor at
// its start. A no-op on a failed target.
fn case_operator_motion(lb: &mut LineBuffer, motion: motion::Motion, count: Option<usize>, kind: motion::CaseKind) {
    let Some(range) = motion::motion_range(lb, motion, count) else {
        return;
    };
    let (from_col, to_col) = (range.from.1, range.to.1);
    match range.shape {
        // Linewise is the whole buffer here.
        motion::MotionShape::Linewise => case_operator_lines(lb, kind),
        // A block on one line is that line's inclusive span.
        motion::MotionShape::Inclusive | motion::MotionShape::Blockwise => {
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

// `guu`, `gUU`, `g~~`: the whole one-line buffer.
fn case_operator_lines(lb: &mut LineBuffer, kind: motion::CaseKind) {
    for c in lb.ed.buf.iter_mut() {
        *c = motion::case_transform(*c, kind);
    }
    lb.ed.cursor = 0;
}

// `~`: toggles `count` characters' case and moves past them, clamped to
// the last character (see `KeyOutcome::ToggleCase`).
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

// `r{ch}`: replaces `count` characters with `ch`, staying in Normal mode.
// A no-op if fewer remain, as in vim.
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

// Removes a one- or two-character range for `ds`, without writing a
// register, as vim-surround doesn't.
fn delete_char_range_line(lb: &mut LineBuffer, range: &motion::MotionRange) {
    let from = range.from.1.min(lb.ed.buf.len());
    let to = (range.to.1 + 1).min(lb.ed.buf.len()).max(from);
    lb.ed.buf.drain(from..to);
}

// vim's rule that `cw`/`cW` act as `ce`/`cE` on a non-blank character:
// changing to the next word's start would also eat the whitespace before
// it. Only for `Op::Change`; `dw` still takes the whitespace.
fn redirect_cw_to_ce(lb: &LineBuffer, motion: &motion::Motion) -> motion::Motion {
    let on_word_char = matches!(lb.ed.buf.get(lb.ed.cursor), Some(c) if !c.is_whitespace());
    match motion {
        motion::Motion::WordForward if on_word_char => motion::Motion::WordEnd,
        motion::Motion::WordForwardBig if on_word_char => motion::Motion::WordEndBig,
        other => other.clone(),
    }
}

// Ctrl-O, vim's one-shot Normal command from insert: reads one whole
// command (`2f)` included) until `vk.feed` stops returning `Pending`,
// applies it, and returns to insert with the cursor where the command
// left it (`<C-o>b` stays at the previous word). `Some(key)` only for
// Ctrl-C/D/Z, which the caller handles like `Propagate`.
fn run_one_shot_normal_command(
    ed: &mut LineEditor,
    registers: &mut Registers,
    undo: &mut UndoTree<Vec<char>>,
    on_idle: &mut dyn FnMut(),
) -> io::Result<Option<Key>> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
    // Unused here (Visual mode isn't entered), but `LineBuffer` needs one.
    let mut selections: Vec<motion::MotionRange> = Vec::new();
    // Checkpoint ordinary typing first; see `run_line_normal_mode`.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    let result = loop {
        let key = match read_key_idle(on_idle)? {
            Some(k) => k,
            // End of input mid-command: the outer loop's next read sees it
            // too.
            None => break None,
        };
        match key {
            // A search being typed keeps Ctrl-C (see vimkeys.rs's
            // `feed_search`).
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
                    // Insert follows any outcome here, so `c{motion}`
                    // needn't check whether it deleted anything.
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
                            Op::Fold => {}
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
                            Op::Fold => {}
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
                    // `R` degrades to `i`, as in `run_line_normal_mode`.
                    KeyOutcome::EnterReplace => break None,
                    KeyOutcome::ToggleCase { count } => {
                        toggle_case(&mut lb, count.unwrap_or(1).max(1));
                        break None;
                    }
                    KeyOutcome::AdjustNumber { delta } => {
                        adjust_number(&mut lb, delta);
                        break None;
                    }
                    // No-ops here: Visual mode extends over several keys,
                    // which a one-shot command can't, and `Join`/`OpenLine`
                    // need more than one line.
                    KeyOutcome::Window(..)
                    | KeyOutcome::Opener
                    | KeyOutcome::EnterVisual(_)
                    | KeyOutcome::ReselectVisual
                    | KeyOutcome::SwapVisualEnds
                    | KeyOutcome::Join { .. }
                    | KeyOutcome::OpenLine { .. }
                    | KeyOutcome::Jump { .. }
                    // `gd` and `gr` need a language server and a file.
                    | KeyOutcome::GotoDefinition(_)
                    | KeyOutcome::GotoReferences
                    | KeyOutcome::DocumentSymbols
                    | KeyOutcome::CodeActions
                    | KeyOutcome::Fold(..)
                    | KeyOutcome::None => break None,
                    // `<C-o>u`, `<C-o>Ctrl-R`, `<C-o>g-`, `<C-o>g+`, as in
                    // vim. No Visual guard: Visual mode is never entered
                    // here.
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
    // Checkpoint once, before returning, since exactly one command ran.
    undo.checkpoint(&ed.buf, (0, ed.cursor));
    Ok(result)
}

// `LineEditor`'s single line as a `bishedit::Buffer`, enough for
// `motion::apply_motion` (one line, no vertical scrolling). Editing goes
// straight to `ed.buf`/`ed.cursor` (via `vimkeys::apply_insert_cmd`), not
// through the trait. `marks` start empty with each excursion.
struct LineBuffer<'a> {
    ed: &'a mut LineEditor,
    marks: &'a mut HashMap<char, (usize, usize)>,
    // Visual mode's committed selections (see repl.rs's
    // `ScreenBuffer::selections`), empty for each excursion.
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
        // SGR codes never count.
        assert_eq!(visible_len("\x1b[1;32mabc\x1b[0m"), 3);
        // A wide CJK char counts 2.
        assert_eq!(visible_len("a中b"), 4);
    }

    #[test]
    fn visible_len_is_grapheme_cluster_aware() {
        // A ZWJ family (7 codepoints) counts its cluster width, 2, not 8.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert_eq!(visible_len(family), 2);
        // Also with SGR codes around it and text either side.
        assert_eq!(visible_len(&format!("a\x1b[1;32m{family}\x1b[0mb")), 1 + 2 + 1);
    }

    #[test]
    fn truncate_visible_drops_a_wide_char_that_would_only_partially_fit() {
        // `a` plus `中` needs 3 columns; with 2 the wide char is dropped
        // whole.
        assert_eq!(truncate_visible("a中b", 2), "a");
        assert_eq!(truncate_visible("a中b", 3), "a中");
        // A leading SGR code is kept, and truncation stops at the wide char
        // that doesn't fit, without the reset after it.
        assert_eq!(truncate_visible("\x1b[1ma中\x1b[0m", 1), "\x1b[1ma");
    }

    // A cut inside a cluster would leave a different character, not a
    // narrower one.
    #[test]
    fn truncate_visible_never_cuts_a_cluster_in_half() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        // The family is two columns: a budget of two takes it, one takes
        // none of it.
        assert_eq!(truncate_visible(family, 2), family);
        assert_eq!(truncate_visible(family, 1), "");
        assert_eq!(truncate_visible(&format!("a{family}"), 2), "a", "one column left is not enough for the family");
        assert_eq!(truncate_visible(&format!("a{family}"), 3), format!("a{family}"));
        // A base and its mark are one cluster, one column.
        assert_eq!(truncate_visible("e\u{301}x", 1), "e\u{301}");
        assert_eq!(truncate_visible("ae\u{301}x", 2), "ae\u{301}");
    }

    // An SGR code inside a cluster doesn't split it.
    #[test]
    fn truncate_visible_keeps_a_cluster_whole_across_a_colour_change() {
        let coloured = "a\u{1f468}\x1b[1m\u{200d}\u{1f469}\x1b[0m";
        assert_eq!(truncate_visible(coloured, 3), coloured);
        assert_eq!(truncate_visible(coloured, 2), "a", "the pair does not fit, so none of it goes");
    }

    // The four kinds of report share an encoding: hover (DECSET 1003 only)
    // looks like a release except for the motion bit.
    #[test]
    fn a_motion_with_no_button_is_a_hover_and_nothing_else_is() {
        let ev = |button| MouseEvent { button, col: 1, row: 1, pressed: true };
        assert!(ev(35).is_hover(), "32 (motion) + 3 (no button)");
        assert!(!ev(35).is_left_click());
        assert!(!ev(35).is_left_drag());

        assert!(!ev(0).is_hover(), "a plain left press");
        assert!(ev(32).is_left_drag(), "32 is motion with the left button held");
        assert!(!ev(32).is_hover(), "...which is a drag, not a hover");
        assert!(!ev(64).is_hover(), "a wheel notch");
        // A release carries the same "no button" bits and no motion.
        assert!(!MouseEvent { button: 3, col: 1, row: 1, pressed: false }.is_hover());
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

    /// Ctrl-W is `unix-word-rubout` and takes `foo/bar` whole; Alt-Backspace
    /// is `backward-kill-word` and takes one component, as bash does.
    #[test]
    fn a_finished_prompt_paints_what_was_typed_and_not_the_suggestion() {
        let mut ed = LineEditor::new();
        for c in "echo alpha".chars() {
            ed.insert(c);
        }
        let ctx = HighlightContext::default();

        // The live prompt: what was typed plus a history entry's ghost.
        let live = compose_redraw("$ ", &ed, "-beta-gamma", 0, 40, ctx, &[]);
        assert!(live.contains("beta-gamma"), "the suggestion is on screen while editing");

        // What's left once submitted: anything more and the scrollback
        // records a command nobody ran.
        let finished = repaint_without_affordances("$ ", &ed, 0, 40, ctx);
        assert!(finished.contains("echo alpha"), "what was typed stays");
        assert!(!finished.contains("beta-gamma"), "what was only suggested does not");
    }

    #[test]
    fn a_finished_prompt_blanks_the_completion_row_under_it() {
        // Only the columns this prompt drew, and the cursor back at column
        // 0, so the caller's `\r\n` means what it did.
        let out = erase_menu_row_under_cursor(true, true, None, 3, 12);
        assert_eq!(out, "\x1b[4G            \r");
        assert!(!out.contains("\x1b[K"), "never to the end of the real line -- a split window has a neighbour there");
    }

    #[test]
    fn nothing_is_erased_when_no_menu_was_showing() {
        assert_eq!(erase_menu_row_under_cursor(true, false, None, 3, 12), "");
        assert_eq!(erase_menu_row_under_cursor(false, true, None, 3, 12), "");
    }

    #[test]
    fn grid_mode_leaves_the_row_to_the_compositor() {
        // In grid mode the pane is repainted wholesale, and on its last row
        // this would write onto the tab bar.
        assert_eq!(erase_menu_row_under_cursor(true, true, Some((5, 20)), 3, 12), "");
    }

    #[test]
    fn the_two_backward_kills_use_different_word_boundaries() {
        let typed = |text: &str| {
            let mut ed = LineEditor::new();
            for c in text.chars() {
                ed.insert(c);
            }
            ed
        };

        let mut ed = typed("cd foo/bar");
        ed.kill_word_backward();
        assert_eq!(ed.as_string(), "cd ", "Ctrl-W stops only at whitespace");

        let mut ed = typed("cd foo/bar");
        ed.kill_word_backward_to_punctuation();
        assert_eq!(ed.as_string(), "cd foo/", "Alt-Backspace stops at the slash");
        // Again: the separator goes with the component before it, as in bash.
        ed.kill_word_backward_to_punctuation();
        assert_eq!(ed.as_string(), "cd ");

        // A trailing separator is skipped before the word is found.
        let mut ed = typed("cd foo/bar/");
        ed.kill_word_backward_to_punctuation();
        assert_eq!(ed.as_string(), "cd foo/");

        // `.`, `-` and `_` end a word too: readline counts only letters and
        // digits.
        for (text, want) in [("echo a.b", "echo a."), ("echo a-b", "echo a-"), ("echo a_b", "echo a_")] {
            let mut ed = typed(text);
            ed.kill_word_backward_to_punctuation();
            assert_eq!(ed.as_string(), want, "{text}");
        }

        // With no punctuation in reach the two agree.
        let mut ed = typed("echo one two");
        ed.kill_word_backward_to_punctuation();
        assert_eq!(ed.as_string(), "echo one ");
    }

    /// Drives a real `bish` prompt over a pty and returns what it wrote, to
    /// check that a pasted script doesn't run itself, which only a real
    /// terminal can show. `None` without a built binary, or when the shell
    /// never reached a prompt (a busy machine, not a finding, as vimdiff
    /// decides).
    fn at_a_prompt(sends: &[&str]) -> Option<Vec<String>> {
        use std::io::{Read, Write};
        let exe = std::env::current_exe().ok()?;
        // Current, not whatever `cargo build` last left (see
        // `tests/the_binary_under_test_is_the_one_just_built.rs`).
        let bish = exe.parent()?.parent()?.join("bish");
        if !bish.exists() {
            return None;
        }
        let home = std::env::temp_dir().join(format!("bish-paste-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let pty = crate::pty::open().ok()?;
        let _ = crate::pty::set_size(std::os::fd::AsRawFd::as_raw_fd(&pty.master), 24, 100);
        let mut cmd = std::process::Command::new(&bish);
        cmd.env_clear();
        cmd.env("TERM", "xterm-256color");
        cmd.env("HOME", &home);
        cmd.env("PATH", "/usr/bin:/bin");
        cmd.current_dir(&home);
        let mut child = crate::pty::spawn_attached(cmd, &pty.slave_path).ok()?;
        let mut master = pty.master;
        crate::pty::set_nonblocking(std::os::fd::AsRawFd::as_raw_fd(&master));
        // One drained chunk per send, so a caller can say when a line
        // appeared.
        let settle = |master: &mut std::fs::File| -> String {
            let mut seen = String::new();
            let mut quiet = 0;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while quiet < 6 && std::time::Instant::now() < deadline {
                let mut buf = [0u8; 4096];
                match master.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                        quiet = 0;
                    }
                    _ => {
                        quiet += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }
            seen
        };
        if settle(&mut master).is_empty() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let mut chunks = Vec::new();
        for send in sends {
            if master.write_all(send.as_bytes()).is_err() {
                break;
            }
            chunks.push(settle(&mut master));
        }
        let _ = master.write_all(b"\x04");
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&home);
        Some(chunks)
    }

    /// What the terminal was told to draw, without the escapes.
    fn without_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            match chars.next() {
                // OSC: runs to a BEL or an ESC \.
                Some(']') => {
                    for c in chars.by_ref() {
                        if c == '\u{7}' || c == '\u{1b}' {
                            break;
                        }
                    }
                }
                // CSI: parameters, then one final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn a_pasted_script_does_not_run_itself() {
        // Two complete commands and the start of a third, as a paste. Each
        // prints something its source doesn't contain, so running and
        // echoing can't be confused.
        let paste = "\u{1b}[200~echo a$((1+1))\necho b$((2+2))\necho c$((3+3\u{1b}[201~";
        let Some(chunks) = at_a_prompt(&[paste, "))\r"]) else {
            return;
        };
        let during = without_escapes(&chunks[0]);
        assert!(during.contains("echo a$((1+1))"), "the paste never arrived: {during:?}");
        assert!(!during.contains("a2"), "the paste ran a command: {during:?}");
        assert!(!during.contains("b4"), "the paste ran a command: {during:?}");
        // ...and all three ran together on the Enter finishing the line the
        // paste ended in.
        let after = without_escapes(&chunks[1]);
        for want in ["a2", "b4", "c6"] {
            assert!(after.contains(want), "`{want}` never ran: {after:?}");
        }
    }

    #[test]
    fn alt_enter_decodes_from_either_line_ending() {
        // Like Alt-Backspace: the key's byte behind an Esc, in either Enter
        // spelling.
        assert_eq!(decode_escape_prefixed(b'\r'), Some(Key::AltEnter));
        assert_eq!(decode_escape_prefixed(b'\n'), Some(Key::AltEnter));
    }

    #[test]
    fn a_paste_without_newlines_is_an_ordinary_insertion() {
        // No lines means the caller inserts it as text, as a one-command
        // paste should be.
        let buf: Vec<char> = "echo ".chars().collect();
        let (lines, rest, cursor) = splice_paste(&buf, 5, "hello");
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!((rest.as_str(), cursor), ("", 0));
    }

    #[test]
    fn a_paste_splits_at_its_newlines_and_keeps_the_tail() {
        let (lines, rest, cursor) = splice_paste(&[], 0, "echo one\necho two\necho thr");
        assert_eq!(lines, vec!["echo one".to_string(), "echo two".to_string()]);
        // The tail is still being edited, the cursor after the paste, where
        // typing would leave it.
        assert_eq!((rest.as_str(), cursor), ("echo thr", 8));
    }

    #[test]
    fn a_paste_that_ends_in_a_newline_leaves_an_empty_line() {
        // Complete, and nothing has run: the next Enter submits it.
        let (lines, rest, cursor) = splice_paste(&[], 0, "echo x\necho y\n");
        assert_eq!(lines, vec!["echo x".to_string(), "echo y".to_string()]);
        assert_eq!((rest.as_str(), cursor), ("", 0));
    }

    #[test]
    fn a_paste_goes_in_at_the_cursor() {
        // `echo | wc` with the cursor after `echo `: the text right of the
        // cursor stays right of it, on the line the paste ended in.
        let buf: Vec<char> = "echo | wc".chars().collect();
        let (lines, rest, cursor) = splice_paste(&buf, 5, "a\nb");
        assert_eq!(lines, vec!["echo a".to_string()]);
        assert_eq!(rest, "b| wc");
        assert_eq!(cursor, 1, "after the pasted `b`, before what was already there");
    }

    #[test]
    fn alt_enter_splits_the_line_where_the_cursor_is() {
        // Alt-Enter: one newline, at the cursor.
        let buf: Vec<char> = "echo a echo b".chars().collect();
        let (lines, rest, cursor) = splice_paste(&buf, 7, "\n");
        assert_eq!(lines, vec!["echo a ".to_string()]);
        assert_eq!((rest.as_str(), cursor), ("echo b", 0));
    }

    #[test]
    fn alt_backspace_decodes_from_either_backspace_byte() {
        // A terminal may send DEL or BS for Backspace, behind an Esc when
        // Alt is held.
        assert_eq!(decode_escape_prefixed(0x7f), Some(Key::AltBackspace));
        assert_eq!(decode_escape_prefixed(0x08), Some(Key::AltBackspace));
        assert_eq!(decode_escape_prefixed(b'q'), None);
    }

    #[test]
    fn a_column_maps_to_the_character_under_it_by_display_width() {
        let mut ed = LineEditor::new();
        for c in "echo hi".chars() {
            ed.insert(c);
        }
        // The prompt is 3 columns, and the line starts at column 5.
        let at = |col| buf_index_at_column("$ >", &ed, 2, col);
        assert_eq!(at(4), None, "still inside the prompt");
        assert_eq!(at(5), Some(0), "first character of the line");
        assert_eq!(at(10), Some(5));

        // A wide character is two columns, so index and column part ways.
        let mut wide = LineEditor::new();
        for c in "\u{4f60}\u{597d}x".chars() {
            wide.insert(c);
        }
        assert_eq!(buf_index_at_column("", &wide, 0, 0), Some(0));
        assert_eq!(buf_index_at_column("", &wide, 0, 2), Some(1), "past one double-width char");
        assert_eq!(buf_index_at_column("", &wide, 0, 4), Some(2), "past both");
    }

    #[test]
    fn a_selection_renders_reversed_over_exactly_what_it_covers() {
        let mut ed = LineEditor::new();
        for c in "abcdef".chars() {
            ed.insert(c);
        }
        let spans = selection_layer(&ed, (1, 4));
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].start, spans[0].end), (1, 4));
        assert!(spans[0].attrs.reverse);
        // Dragged backwards, the same selection.
        assert_eq!(selection_layer(&ed, (4, 1)).first().map(|s| (s.start, s.end)), Some((1, 4)));
        // An empty one draws nothing.
        assert!(selection_layer(&ed, (2, 2)).is_empty());
        // ...and one past the end is clamped.
        assert_eq!(selection_layer(&ed, (4, 99)).first().map(|s| (s.start, s.end)), Some((4, 6)));
    }

    #[test]
    fn every_control_byte_decodes_to_a_key_of_its_own() {
        // 0x07, 0x11, 0x13 and 0x14 decode so mappings can name them (0x11
        // and 0x13, XON/XOFF, arrive because `derive_raw` clears IXON).
        assert_eq!(decode_solo_byte(0x07), Some(Key::CtrlG));
        assert_eq!(decode_solo_byte(0x11), Some(Key::CtrlQ));
        assert_eq!(decode_solo_byte(0x13), Some(Key::CtrlS));
        assert_eq!(decode_solo_byte(0x14), Some(Key::CtrlT));
        // Their neighbours are unchanged.
        assert_eq!(decode_solo_byte(0x06), Some(Key::CtrlF));
        assert_eq!(decode_solo_byte(0x12), Some(Key::CtrlR));
        assert_eq!(decode_solo_byte(0x15), Some(Key::CtrlU));
        // ...and so are the bytes that mean something else.
        assert_eq!(decode_solo_byte(0x09), Some(Key::Tab));
        assert_eq!(decode_solo_byte(0x0d), Option::None, "CR is Enter, but that arm reads no further byte either -- it stays in read_key");
    }

    #[test]
    fn decode_sgr_mouse_final_press_and_release() {
        assert_eq!(decode_sgr_mouse_final("0;12;5", b'M'), Key::Mouse(MouseEvent { button: 0, col: 12, row: 5, pressed: true }));
        assert_eq!(decode_sgr_mouse_final("0;12;5", b'm'), Key::Mouse(MouseEvent { button: 0, col: 12, row: 5, pressed: false }));
    }

    #[test]
    fn decode_sgr_mouse_final_carries_the_raw_button_code() {
        // Bit 5 (0x20) means motion with a button held, a drag. The decoder
        // leaves it to the accessors; this checks the code round-trips.
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
        // Neither is a click.
        assert!(!up.is_left_click());
        assert!(!down.is_left_click());
    }

    #[test]
    fn mouse_event_wheel_ignores_modifier_bits() {
        // Shift, Meta and Ctrl (bits 2-4) on a wheel notch: still a wheel
        // event.
        let shift_wheel_up = MouseEvent { button: 64 | 0x04, col: 1, row: 1, pressed: true };
        assert!(shift_wheel_up.is_scroll_up());
    }

    #[test]
    fn mouse_event_recognizes_the_horizontal_wheel() {
        let left = MouseEvent { button: 66, col: 1, row: 1, pressed: true };
        assert!(left.is_scroll_left());
        assert!(!left.is_scroll_right() && !left.is_scroll_up() && !left.is_scroll_down());
        let right = MouseEvent { button: 67, col: 1, row: 1, pressed: true };
        assert!(right.is_scroll_right());
        assert!(!right.is_scroll_left() && !right.is_scroll_up() && !right.is_scroll_down());
        // Modifiers are masked out here too.
        assert!(MouseEvent { button: 66 | 0x04, col: 1, row: 1, pressed: true }.is_scroll_left());
    }

    // The four are mutually exclusive, so a caller can test them in any
    // order.
    #[test]
    fn mouse_event_wheel_directions_never_overlap() {
        for button in [64u16, 65, 66, 67] {
            let ev = MouseEvent { button, col: 1, row: 1, pressed: true };
            let set = [ev.is_scroll_up(), ev.is_scroll_down(), ev.is_scroll_left(), ev.is_scroll_right()];
            assert_eq!(set.iter().filter(|b| **b).count(), 1, "button {button}");
        }
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
        // The (Indexed(8), dim) pair
        // `default_style(HighlightKind::Comment)` uses, which
        // `vt100::sgr_codes` writes as `0;2;90`.
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

    // With no ghost the output is exactly the plain buffer's: no stray dim
    // styling, no extra cursor movement.
    #[test]
    fn empty_ghost_changes_nothing() {
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, "", 0, 40, HighlightContext::default(), &[]);
        assert!(!out.contains("\x1b[0;2;90m"), "{out:?}");
        assert!(!out.contains('D'), "{out:?}");
    }

    #[test]
    fn ghost_longer_than_remaining_width_truncates_rather_than_wrapping() {
        // `$ ` (2 columns) in width 6 leaves 4; `ls` takes 2, so a 10-char
        // ghost is cut to 2.
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, "0123456789", 0, 6, HighlightContext::default(), &[]);
        assert!(out.contains("01"), "{out:?}");
        assert!(!out.contains("0123"), "{out:?}");
    }

    #[test]
    fn overlong_buffer_suppresses_the_ghost_entirely() {
        // The buffer alone exceeds the room, so the horizontal-scroll
        // branch runs, with no room for a ghost. Cursor at 24, the end of
        // the buffer.
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
        // Left at column 0 doesn't move, so `motion_range` gives `None`.
        let deleted = delete_motion(&mut lb, &mut registers, motion::Motion::Left, None, None);
        assert!(!deleted);
        assert_eq!(ed.buf.iter().collect::<String>(), "abc");
        assert_eq!(registers.read(None).text, "");
    }

    #[test]
    fn delete_motion_linewise_clears_the_whole_single_line_buffer() {
        // `GotoFirstLine` moves to the first non-blank, giving
        // `motion_range` a real Linewise range, which exercises the
        // whole-buffer branch.
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
        // The cursor lands on the leftmost selection's open delimiter.
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
        // Other motions pass through.
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
        // `cw` in the middle of `foo` changes only to the `o`, not through
        // the space, as vim's `cw` acts like `ce`.
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

    // Two ranges removed rightmost first, so the left one's columns hold,
    // with the register holding both in commit order whatever their
    // positions.
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
        // The register is untouched, unlike vim's swap, so the same text
        // can be put again.
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
        // The anchor moves to the word's start, as in vim's `viw`.
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
        // `GotoLastLine` on one line doesn't move, but is a jump, so (0, 0)
        // is recorded.
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

    // Plain expansions only, which never write the snippet slot.
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
        // `gco` is an argument to `echo` here, not the command, so it's left
        // alone.
        let mut ed = make_editor("echo gco", 8);
        let table = abbrs(&[("gco", "git checkout")]);
        assert!(!expand(&mut ed, &table));
        assert_eq!(ed.as_string(), "echo gco");
    }

    #[test]
    fn expand_abbr_at_cursor_only_matches_the_word_ending_exactly_at_the_cursor() {
        // The cursor is after `gc`, so the word at the cursor isn't `gco`.
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

    // `abbr` snippets. The key dispatch lives in `read_line` and needs a
    // terminal; this tests the seam between the model and the line editor.

    fn start_snippet(line: &str, cursor: usize, expansion: &str) -> (LineEditor, LiveSnippet) {
        let mut ed = make_editor(line, cursor);
        let table = vec![Abbr::new("foo", expansion)];
        let mut snippet = None;
        assert!(expand_abbr_at_cursor(&mut ed, &table, &mut snippet));
        (ed, snippet.expect("an expansion with tabstops is a snippet"))
    }

    fn type_text(ed: &mut LineEditor, state: &mut LiveSnippet, text: &str) {
        for c in text.chars() {
            state.snip.type_char(c);
            state.sync(ed);
        }
    }

    #[test]
    fn an_expansion_with_tabstops_splices_in_tentatively() {
        let (ed, state) = start_snippet("foo", 3, "bar -x $1 -y $2 | qoo");
        assert_eq!(ed.as_string(), "bar -x $1 -y $2 | qoo");
        assert_eq!(state.holes()[0].0, (0, 7), "the first tabstop starts after `bar -x `");
        // The caret parks at the first tabstop, not at the end.
        assert_eq!(ed.cursor, 7);
    }

    #[test]
    fn a_snippet_only_ever_rewrites_its_own_span_of_the_line() {
        let (mut ed, mut state) = start_snippet("foo", 3, "cd $1");
        // Expanded mid-line, as with text typed after the abbreviation.
        assert_eq!(ed.as_string(), "cd $1");
        type_text(&mut ed, &mut state, "src");
        assert_eq!(ed.as_string(), "cd src");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn accepting_leaves_the_cursor_where_typing_it_out_would_have() {
        let (mut ed, mut state) = start_snippet("foo", 3, "bar -x $1 -y $2 | qoo");
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
        let (mut ed, mut state) = start_snippet("echo hi; foo", 12, "cd $1");
        assert_eq!(ed.as_string(), "echo hi; cd $1");
        type_text(&mut ed, &mut state, "src");
        state.cancel(&mut ed);
        assert_eq!(ed.as_string(), "echo hi; foo");
        assert_eq!(ed.cursor, 12);
    }

    #[test]
    fn the_tabstop_layer_marks_the_active_one_differently() {
        let (_, state) = start_snippet("foo", 3, "a $1 b $2");
        let layer = snippet_layer(&state);
        assert_eq!(layer.len(), 2);
        assert!(layer[0].attrs.reverse, "the tabstop being typed into is the reverse-video one");
        assert!(!layer[1].attrs.reverse && layer[1].attrs.underline);
    }

    #[test]
    fn the_layer_is_offset_by_where_the_snippet_actually_sits() {
        let (_, state) = start_snippet("echo hi; foo", 12, "cd $1");
        let layer = snippet_layer(&state);
        // `echo hi; cd ` is 12 chars, then `$1`.
        assert_eq!((layer[0].start, layer[0].end), (12, 14));
    }

    #[test]
    fn numbering_decides_which_hole_is_filled_first() {
        let (mut ed, mut state) = start_snippet("foo", 3, "bar -x $2 -y $1");
        type_text(&mut ed, &mut state, "why");
        assert_eq!(ed.as_string(), "bar -x $2 -y why", "the caret started on the *second* hole, because it is `$1`");
        state.snip.advance(false);
        state.sync(&mut ed);
        type_text(&mut ed, &mut state, "ex");
        state.accept(&mut ed);
        assert_eq!(ed.as_string(), "bar -x ex -y why");
    }

    // A multi-line snippet folds to spaces on the one-line prompt, one space
    // per newline, so the model's offsets still line up.
    #[test]
    fn a_multi_line_expansion_folds_to_one_line_at_the_prompt() {
        let (mut ed, mut state) = start_snippet("foo", 3, "if $1; then\n\t$0\nfi");
        assert_eq!(ed.as_string(), "if $1; then \t fi");
        assert_eq!(ed.cursor, 3, "still parked on `$1`");
        type_text(&mut ed, &mut state, "x");
        assert_eq!(ed.as_string(), "if x; then \t fi");
        state.accept(&mut ed);
        assert_eq!(ed.as_string(), "if x; then \t fi");
        assert_eq!(ed.cursor, "if x; then \t".chars().count(), "and `$0` still takes the caret");
    }
}
