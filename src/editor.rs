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

use crate::bishedit::completion::{CompletionCandidate, CompletionProvider, CompletionRequest};
use crate::bishedit::highlight::{self, BashHighlighter, Highlighter, HighlightContext, StyledSpan};
use crate::bishedit::motion;
use crate::bishedit::suggestion::{SuggestionProvider, SuggestionRequest};
use crate::bishedit::vimkeys::{self, KeyOutcome, VimKeys};
use crate::history::History;
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
        0x09 => Key::Tab,
        0x0b => Key::CtrlK,
        0x0c => Key::CtrlL,
        0x0e => Key::CtrlN,
        0x0f => Key::CtrlO,
        0x10 => Key::CtrlP,
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
        b'Z' => Key::BackTab,
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
fn redraw(prompt: &str, ed: &LineEditor, col_origin: usize, width: usize, ctx: HighlightContext) -> io::Result<()> {
    print!("{}", compose_redraw(prompt, ed, "", col_origin, width, ctx));
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
fn compose_redraw(prompt: &str, ed: &LineEditor, ghost: &str, col_origin: usize, width: usize, ctx: HighlightContext) -> String {
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
            let (fg, attrs) = highlight::default_style(s.kind);
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
    let cells = highlight::compose(&combined, &[&styled, &ghost_layer]);

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
// Relative movement nets to zero rows moved regardless of what
// scrolled in between, correct by construction. The row is always
// erased (even when `completion` is now None), clearing a stale row
// left over from the previous call. `\x1b[1A` restores the *row* but
// not the column redraw() already left the real cursor at -- the
// second compose_redraw call is the deliberately-simple way to fix
// that, rather than duplicating redraw()'s own column math here.
//
// The "move down" step is a literal `\n`, not `\x1b[1B` (Cursor Down):
// CUD is defined to *clamp* at the terminal's bottom margin rather than
// scroll, unlike a real linefeed. This function runs on every redraw
// once `menu_capable` is true (to erase a stale row even with no active
// completion), so the very first call after a prompt lands with zero
// rows of slack below it -- exactly what happens right after a
// command's own output scrolls the terminal -- has to guarantee a row
// actually exists below, scrolling for it if needed, not silently fail
// to move and let the subsequent `\x1b[1A` land back on the *previous*
// command's just-scrolled output row instead of a blank one. (Found via
// interactive testing: without this, that output row got silently
// cleared and overwritten by the next prompt.) `\x1b[1A` afterward stays
// correct either way -- by definition there's always a row above
// wherever the linefeed left the cursor.
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
fn redraw_with_completion_row(
    prompt: &str,
    ed: &LineEditor,
    ghost: &str,
    completion: Option<&CompletionState>,
    menu_capable: bool,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
) -> io::Result<()> {
    let mut out = compose_redraw(prompt, ed, ghost, col_origin, width, ctx);
    if menu_capable {
        out.push('\n');
        out.push_str(&format!("\x1b[{}G", col_origin + 1));
        out.push_str(&" ".repeat(width));
        out.push_str(&format!("\x1b[{}G", col_origin + 1));
        if let Some(state) = completion {
            let row = build_completion_row(state);
            let cells = highlight::compose(&row.chars, &[&row.bold, &row.selected]);
            let visible: &[vt100::Cell] = if row.chars.len() <= width {
                &cells
            } else {
                // Right-anchors the window on the selected candidate's
                // own end, same shape as redraw()'s own cursor-
                // visibility clamp for an overlong buffer -- a long
                // candidate list stays truncated/scrolled to `width`
                // rather than wrapping into a neighboring pane's row.
                let window_start = row.selected_range.end.saturating_sub(width).min(row.chars.len() - width);
                let window_end = (window_start + width).min(row.chars.len());
                &cells[window_start..window_end]
            };
            out.push_str(&highlight::render_styled(visible));
        }
        out.push_str("\x1b[1A");
        out.push_str(&compose_redraw(prompt, ed, ghost, col_origin, width, ctx));
    }

    print!("{}", out);
    io::stdout().flush()
}

// How many terminal columns `s` actually occupies once drawn, not
// counting invisible escape bytes -- `s` is always one of this crate's
// own prompt strings, which only ever embed `\x1b[...m` SGR (color)
// codes, so that's the only escape form this needs to recognize.
// pub(crate): repl.rs's own freeze-with-text helper (Ctrl+Space with
// in-progress text) reuses this to know how many *visible* columns a
// colored prompt occupies, so it can position the frozen row's
// ScreenBuffer cursor at the right column rather than guessing.
pub(crate) fn visible_len(s: &str) -> usize {
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
    mut on_idle: impl FnMut(),
) -> io::Result<ReadOutcome> {
    let mut guard = Some(term::RawGuard::enable(0)?);
    let mut ed = match initial {
        Some((text, cursor)) => {
            let mut e = LineEditor::new();
            e.buf = text.chars().collect();
            e.cursor = cursor.min(e.buf.len());
            e
        }
        None => LineEditor::new(),
    };

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
        menu_capable,
        col_origin,
        width,
        ctx,
    )?;

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
            Key::CtrlL if ctrl_l_reports => {
                drop(guard.take());
                return Ok(ReadOutcome::CtrlL);
            }
            Key::CtrlL => print!("\x1b[H\x1b[2J"),
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
            Key::CtrlE => match run_line_normal_mode(&mut ed, prompt, col_origin, width, ctx, &mut on_idle)? {
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
                if let Some(k) = run_one_shot_normal_command(&mut ed, &mut on_idle)? {
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
            Key::AltLeft | Key::AltRight | Key::AltUp | Key::CtrlY | Key::Unknown => {}
        }
        // Recomputed fresh every iteration -- see compute_suggestion's
        // own doc comment. Must happen after the match (so it reflects
        // whatever the just-dispatched key actually did to the buffer/
        // completion/browse state) and before the redraw that shows it.
        suggestion = compute_suggestion(&ed, suggestion_provider, completion.is_some(), browse.is_some());
        redraw_with_completion_row(prompt, &ed, suggestion.as_deref().unwrap_or(""), completion.as_ref(), menu_capable, col_origin, width, ctx)?;
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
fn run_line_normal_mode(
    ed: &mut LineEditor,
    prompt: &str,
    col_origin: usize,
    width: usize,
    ctx: HighlightContext,
    on_idle: &mut dyn FnMut(),
) -> io::Result<LineNormalExit> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
    // Reverse-video prompt: the mode indicator. Deliberately not a
    // terminal cursor-shape change (DECSCUSR) -- that's global terminal
    // state with no clean way to restore whatever the user's own
    // terminal had configured before this feature ever touched it, so a
    // forced "steady bar" on the way back out would itself be an
    // unwanted, sticky change to something this feature has no business
    // touching.
    let decorated_prompt = format!("\x1b[7m{}\x1b[0m", prompt);
    redraw(&decorated_prompt, ed, col_origin, width, ctx)?;
    let exit = loop {
        let key = match read_key_idle(on_idle)? {
            Some(k) => k,
            None => break LineNormalExit::Eof,
        };
        match key {
            Key::CtrlC | Key::CtrlD | Key::CtrlZ => break LineNormalExit::Propagate(key),
            _ => {
                let mut lb = LineBuffer { ed, marks: &mut marks };
                match vk.feed(key) {
                    KeyOutcome::Motion(m, count) => {
                        motion::apply_motion(&mut lb, m, count);
                    }
                    KeyOutcome::EnterInsert(cmd) => {
                        let (new_buf, new_cursor) = vimkeys::apply_insert_cmd(&lb.ed.buf, lb.ed.cursor, cmd);
                        lb.ed.buf = new_buf;
                        lb.ed.cursor = new_cursor;
                        break LineNormalExit::ToInsert;
                    }
                    // <C-w> is still vimkeys' own window-leader prefix
                    // here too, matching real vim's own Normal-mode
                    // Ctrl-W meaning -- intentionally not special-cased
                    // away, even though there's no window/pane state for
                    // it to act on in this context (a harmless no-op).
                    KeyOutcome::Window(..) | KeyOutcome::Pending | KeyOutcome::None => {}
                }
            }
        }
        redraw(&decorated_prompt, ed, col_origin, width, ctx)?;
    };
    Ok(exit)
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
fn run_one_shot_normal_command(ed: &mut LineEditor, on_idle: &mut dyn FnMut()) -> io::Result<Option<Key>> {
    let mut vk = VimKeys::new();
    let mut marks: HashMap<char, (usize, usize)> = HashMap::new();
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
            Key::CtrlC | Key::CtrlD | Key::CtrlZ => break Some(key),
            _ => {
                let mut lb = LineBuffer { ed, marks: &mut marks };
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
                    KeyOutcome::Window(..) | KeyOutcome::None => break None,
                    KeyOutcome::Pending => continue,
                }
            }
        }
    };
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
    fn decode_csi_final_unrecognized_final_byte_is_unknown() {
        assert_eq!(decode_csi_final("", b'Q'), Key::Unknown);
    }

    fn make_editor(text: &str, cursor: usize) -> LineEditor {
        LineEditor { buf: text.chars().collect(), cursor }
    }

    #[test]
    fn ghost_text_appears_in_the_composed_output() {
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, " -la", 0, 40, HighlightContext::default());
        assert!(out.contains(" -la"), "{out:?}");
    }

    #[test]
    fn ghost_uses_the_existing_dim_grey_convention() {
        // Same (Indexed(8), dim) pair default_style(HighlightKind::Comment)
        // already uses -- vt100::sgr_codes turns that into "0;2;90".
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, " -la", 0, 40, HighlightContext::default());
        assert!(out.contains("\x1b[0;2;90m"), "{out:?}");
    }

    #[test]
    fn cursor_walks_back_past_the_ghost_tail() {
        let ed = make_editor("ls", 2);
        let ghost = " -la";
        let out = compose_redraw("$ ", &ed, ghost, 0, 40, HighlightContext::default());
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
        let out = compose_redraw("$ ", &ed, "", 0, 40, HighlightContext::default());
        assert!(!out.contains("\x1b[0;2;90m"), "{out:?}");
        assert!(!out.contains('D'), "{out:?}");
    }

    #[test]
    fn ghost_longer_than_remaining_width_truncates_rather_than_wrapping() {
        // prompt "$ " (2 cols) + width 6 -> remaining 4; buffer "ls" (2
        // chars) leaves exactly 2 columns of room. A 10-char ghost must
        // be cut down to those 2 chars, not spill past the pane's width.
        let ed = make_editor("ls", 2);
        let out = compose_redraw("$ ", &ed, "0123456789", 0, 6, HighlightContext::default());
        assert!(out.contains("01"), "{out:?}");
        assert!(!out.contains("0123"), "{out:?}");
    }

    #[test]
    fn overlong_buffer_suppresses_the_ghost_entirely() {
        // The buffer alone already exceeds `remaining` -- compose_redraw
        // takes the horizontal-scroll branch, which never has any room
        // left for a ghost tail at all.
        let ed = make_editor("a very long command line", 25);
        let out = compose_redraw("$ ", &ed, "ignored-ghost", 0, 10, HighlightContext::default());
        assert!(!out.contains("ignored-ghost"), "{out:?}");
        assert!(!out.contains("\x1b[0;2;90m"), "{out:?}");
    }
}
