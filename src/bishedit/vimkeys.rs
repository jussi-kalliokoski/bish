// Translates a sequence of editor::Key events into bishedit::motion::Motion
// values. This layer knows vim's specific bindings and owns the state a
// single keypress can't (an accumulating count, a pending g/f/F/t/T/m/`/'
// prefix awaiting one more character, an in-progress search string) -- but
// it never touches a Buffer or a terminal. Motions themselves are applied
// by the caller via bishedit::motion::apply_motion.

use crate::editor::Key;
use super::motion::{Motion, TextObjectKind};
use super::registers::RegisterShape;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// A motion is ready to apply, with the raw count the user typed before
    /// it (if any).
    Motion(Motion, Option<usize>),
    /// A Ctrl-W window command is ready to run, with a count. Not a
    /// `Motion`: these act on the frontend's own window/pane state, not
    /// on a `Buffer`, so they're a separate outcome the caller applies
    /// however it applies window commands (repl.rs already has
    /// `apply_window_action` for exactly this). For most `WindowCmd`
    /// variants the count is a *repeat*, typed before `<C-w>` -- e.g.
    /// `2<C-w>n` is `WindowCmd::Next` with count `Some(2)`, "next window,
    /// twice". `GotoFirstWindow`/`GotoLastWindow` are the exception,
    /// mirroring `Motion::GotoFirstLine`/`GotoLastLine`: their count is
    /// typed *inside* the `<C-w>` sequence, after it, as an absolute
    /// 1-indexed tab position -- `<C-w>5gg` and `<C-w>5G` both mean "go
    /// to the 5th tab" (count `Some(5)`), while bare `<C-w>gg`/`<C-w>G`
    /// (count `None`) default to the first/last tab respectively.
    Window(WindowCmd, Option<usize>),
    /// `i`/`a`/`I`/`A`/`s`/`S`/`C`: vim's canonical normal-to-insert entry
    /// commands. Not a `Motion` -- these don't move a cursor by themselves,
    /// they tell the caller "stop navigating, resume editing text, and use
    /// `apply_insert_cmd` (below) to work out exactly where/what changes
    /// first" -- so, like `Window`, the caller applies this against
    /// whatever it considers "the buffer" (which may not even be the same
    /// `Buffer` a `Motion` was just applied to -- see apply_insert_cmd's own
    /// doc comment). Any count typed before one of these (`3i`) is silently
    /// discarded, same as it would be for a key `feed_fresh` doesn't
    /// otherwise recognize -- there's no insert-repeat-on-exit support yet.
    EnterInsert(InsertCmd),
    /// `y{motion}` -- an operator applied to a motion's resulting range.
    /// `register` is the explicit `"x` prefix if any (`None` means "use
    /// the unnamed register" -- see registers.rs's own doc comment on how
    /// that resolves).
    Operator(Op, Motion, Option<usize>, Option<char>),
    /// `yy` / `Y` -- an operator applied to the current line (and
    /// `count - 1` more below it), linewise. Kept distinct from
    /// `Operator` rather than inventing a synthetic `Motion` for "the
    /// current line": vim's own double-tap-the-operator shorthand isn't a
    /// cursor motion at all, it's defined operationally as "this
    /// operator, this line" (see `motion::whole_lines`'s own doc comment).
    OperatorLines(Op, Option<usize>, Option<char>),
    /// `p` / `P` -- put the named (or unnamed) register's contents after
    /// (`before: false`) or before (`before: true`) the cursor, `count`
    /// times.
    Put { before: bool, count: Option<usize>, register: Option<char> },
    /// `x` -- delete `count` characters forward from the cursor. Not an
    /// `Operator(Op::Delete, Motion::Right, ...)`: `Motion::Right`'s own
    /// clamping refuses to move onto/past the line's last character, so
    /// that would wrongly no-op exactly where vim's `x` still deletes.
    /// `X` (delete backward) has no such edge case -- `Motion::Left`
    /// already clamps correctly at column 0 -- so it stays a plain
    /// `Operator` and needs no variant of its own.
    DeleteCharForward { count: Option<usize>, register: Option<char> },
    /// `J`/`gJ` -- join `count` lines (minimum 2, matching vim: a bare `J`
    /// or an explicit `1J` both just join the current line with the next
    /// one). Not a `Motion` (it mutates rather than moves) or an
    /// `Operator` (there's no motion/target to combine with) -- its own
    /// outcome, same tier as `Put`/`DeleteCharForward`. `with_space`
    /// selects vim's default whitespace-aware join (`J`, strips the next
    /// line's leading whitespace and inserts one space) vs. `gJ`'s raw
    /// concatenation.
    Join { count: Option<usize>, with_space: bool },
    /// `v`/`V`: enters Visual mode, charwise (`RegisterShape::Char`) or
    /// linewise (`RegisterShape::Line`), anchored at the buffer's current
    /// cursor. Reuses `registers::RegisterShape` rather than a dedicated
    /// enum -- charwise/linewise is exactly the same two-way distinction a
    /// yank's own result shape already needs, and this selection's shape
    /// *becomes* that yank shape once it's committed. The caller must
    /// immediately call `begin_visual` with the buffer's own current
    /// cursor as the anchor -- this crate never touches a `Buffer` itself
    /// (see this file's own module doc comment), so it can't read that
    /// position on its own.
    EnterVisual(RegisterShape),
    /// `gv` -- reselects the last Visual selection, if any (`vk.last_
    /// visual()`'s own doc comment on when there is one). The caller must
    /// call `buf.set_cursor(cursor)` then `vk.begin_visual(shape, anchor)`
    /// with that tuple (this crate can't touch a `Buffer` itself -- same
    /// reasoning as `EnterVisual`'s own doc comment); a no-op if there's
    /// nothing to reselect yet.
    ReselectVisual,
    /// `Ctrl-O`/`Ctrl-I` -- step backward/forward through the jump list.
    /// The caller must call `vk.jump_back(buf.cursor())`/`vk.jump_forward
    /// (buf.cursor())` (this crate owns the jump-list state -- see
    /// `push_jump`'s own doc comment -- but never a `Buffer`) and, if it
    /// returns `Some`, move the cursor there; a no-op at either end of the
    /// list.
    Jump { forward: bool },
    /// `ys{motion}{ch}` / `yss{ch}` / Visual-mode `S{ch}` -- vim-surround's
    /// own "wrap this in a delimiter pair" command. `target` names what to
    /// wrap (a resolved motion's own range, or -- `yss`'s own shorthand --
    /// the current line from its first non-blank through its true end);
    /// `ch` is the delimiter character that was actually pressed (`(` and
    /// `)` both name the same pair but insert differently padded --
    /// `motion::surround_delims` resolves that). Visual-mode `S` never
    /// reaches this crate as a `Motion`-typed target at all (same reason
    /// `EnterVisual`'s own doc comment gives for `y`/`d`/`c`/`p` there): a
    /// caller that intercepts `S` itself builds a `SurroundTarget::Motion`
    /// wrapping a synthetic range instead, or applies the wrap directly.
    /// Not an `Operator`: nothing here is a real motion target lookup the
    /// way `y{motion}` is -- resolving one still needs one more raw key
    /// (this delimiter character) that no `Motion`/`Op` alone carries, so
    /// this outcome only appears once that key has already been read (see
    /// `Pending::SurroundChar`'s own doc comment for how the two-stage
    /// resolution gets there).
    AddSurround { target: SurroundTarget, ch: char },
    /// `ds{ch}`: removes the nearest enclosing delimiter pair named by
    /// `ch` (`motion::surround_target_kind`'s own doc comment lists which
    /// characters name which pair), stripping one adjacent padding space
    /// too for a bracket pair (quotes never pad -- see `motion::
    /// surround_delete_spans`'s own doc comment). A no-op if no such pair
    /// encloses the cursor, or if `ch` doesn't name a valid target.
    DeleteSurround { ch: char },
    /// `cs{ch}{replacement}`: like `DeleteSurround`, but replaces the
    /// found pair's own two delimiter characters with `replacement`'s
    /// pair (`motion::surround_delims`) instead of removing them --
    /// unlike `ds`, never touches any padding around them.
    ChangeSurround { ch: char, replacement: char },
    /// `r{ch}`: replaces `count` characters starting at the cursor with
    /// `ch` each, staying in Normal mode -- vim's own single-character
    /// replace. The caller refuses (no-op) if fewer than `count`
    /// characters remain on the line from the cursor onward, matching
    /// vim: `r` never crosses a line break or extends the buffer.
    ReplaceChar { ch: char, count: Option<usize> },
    /// `R`: enters Replace mode at the cursor -- like Insert mode, but
    /// each typed character overwrites the one already there (extending
    /// the line once past its end) instead of shifting it rightward.
    /// Not an `EnterInsert(InsertCmd)`: no repositioning is needed first
    /// (`R` always starts exactly at the cursor), and the *typing*
    /// itself behaves differently for as long as the mode lasts, which
    /// `InsertCmd` has no way to express (it only ever describes a
    /// one-time starting position/deletion, resolved once before an
    /// ordinary insert loop begins).
    EnterReplace,
    /// `~`: toggles the case of `count` characters starting at the
    /// cursor (default 1), then advances the cursor to just past the
    /// last one toggled -- clamped to the line's own last character if
    /// that would run past it, matching vim: never crosses a line break,
    /// never extends past what's already there.
    ToggleCase { count: Option<usize> },
    /// `Ctrl-A`/`Ctrl-X`: adds `delta` (already signed -- positive for
    /// `Ctrl-A`, negative for `Ctrl-X`, magnitude `count`) to the decimal
    /// number found at or after the cursor on the current line (see
    /// `motion::find_number`'s own doc comment for exactly how that's
    /// found). A no-op if there's no number on the line from the cursor
    /// onward.
    AdjustNumber { delta: i64 },
    /// The key was consumed as part of an in-progress sequence (a count
    /// digit, or a prefix awaiting its next character); no motion yet.
    Pending,
    /// The key isn't part of any recognized motion sequence. Any
    /// in-progress count/prefix is discarded, matching vim's behavior of
    /// dropping a pending command on an invalid continuation.
    None,
}

/// An operator awaiting a motion (`y{motion}`/`d{motion}`/`c{motion}`) or
/// its own double-tap shorthand (`yy`/`dd`/`cc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Yank,
    Delete,
    /// `c{motion}`/`cc` -- like `Delete`, but the caller also enters
    /// insert mode at the deletion point afterward (skipped if the
    /// motion target was invalid/empty, same as any other failed
    /// operator -- see `KeyOutcome::Operator`'s own doc comment on that).
    Change,
    /// `gu{motion}`/`guu` -- lowercases every character in the target.
    /// Reached via `feed_g` (see its own `u` arm), not `feed_fresh`
    /// directly -- these three are vim's own `g`-prefixed operators, the
    /// same tier as `gJ`, just taking a motion instead of resolving
    /// immediately. Never writes a register (neither yanks nor deletes
    /// anything); a caller's `Operator`/`OperatorLines` handling for
    /// these three simply ignores whatever register accompanies them.
    Lowercase,
    /// `gU{motion}`/`gUU` -- uppercases every character in the target.
    Uppercase,
    /// `g~{motion}`/`g~~` -- toggles the case of every character in the
    /// target (vim's own per-character `~`, generalized to a motion).
    CaseToggle,
}

/// `KeyOutcome::AddSurround`'s own target: either a motion's resolved
/// range (`ys{motion}`, carrying whatever count applied to it, mirroring
/// `KeyOutcome::Operator`'s own `(Motion, Option<usize>)` shape) or the
/// current line (`yss`'s own count -- `yss`/`2yss` wrap `count` lines
/// starting at the cursor, the same "this operator, this line, `count`
/// of them" shape `OperatorLines`'s own doc comment already establishes
/// for `yy`/`dd`/`cc`).
#[derive(Debug, Clone, PartialEq)]
pub enum SurroundTarget {
    Motion(Motion, Option<usize>),
    Line(Option<usize>),
}

impl Op {
    /// The key that both arms this operator and, pressed again at a fresh
    /// dispatch point, triggers its whole-line shorthand (`yy`/`dd`/`cc`).
    fn trigger_char(self) -> char {
        match self {
            Op::Yank => 'y',
            Op::Delete => 'd',
            Op::Change => 'c',
            // The character right after the `g` that armed these, not
            // `g` itself -- `guu`/`gUU`/`g~~` repeat *that* key, matching
            // real vim (mirrors `ys`'s own `s`-not-`y` trigger for the
            // same reason: whatever key resolved the arming, not the
            // leader in front of it).
            Op::Lowercase => 'u',
            Op::Uppercase => 'U',
            Op::CaseToggle => '~',
        }
    }
}

// `[count1]`, typed before the operator, and `[count2]`, typed before the
// motion (or as `yy`'s own repeat, `y[count2]y`), multiply together --
// matching vim (`2y3w` yanks 6 words). `None` stands in for "1" on both
// sides so a bare count on just one side is used as-is rather than being
// multiplied against a phantom 1 that would've been fine anyway; the
// `saturating_mul` just guards against a pathological huge-count product
// panicking rather than being a realistic scenario.
fn combine_counts(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.saturating_mul(y)),
    }
}

/// `i`/`a`/`I`/`A`/`s`/`S`/`C` -- see `KeyOutcome::EnterInsert`'s own doc
/// comment for why these are a distinct outcome rather than `Motion`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertCmd {
    /// `i` -- insert before the cursor (i.e. resume exactly where it is).
    Before,
    /// `a` -- insert after the cursor.
    After,
    /// `I` -- insert at the start of the line.
    LineStart,
    /// `A` -- insert at the end of the line.
    LineEnd,
    /// `s` -- delete the character under the cursor, insert in its place.
    SubstituteChar,
    /// `S` -- clear the whole line, insert from its (now empty) start.
    SubstituteLine,
    /// `C` -- delete from the cursor to the end of the line, insert there.
    ChangeToEnd,
    /// `gi` -- insert at wherever Insert mode last ended (vim's own `^`
    /// mark). Unlike every other `InsertCmd`, resolving this needs a
    /// buffer's own marks, which `apply_insert_cmd` (below) has no access
    /// to -- see its own doc comment on why it works over a bare `[char]`
    /// slice. `apply_insert_cmd` treats it the same as `Before` (insert
    /// right where the cursor already is): a reasonable fallback for the
    /// single-line contexts that call it (the shell's own line editor,
    /// the pane-scrollback Ctrl+Space excursion), where "the last insert
    /// position" isn't a concept those contexts track. `fileeditor.rs`'s
    /// own `TextBuffer`-aware `resolve_insert_start` resolves it for
    /// real, via `Buffer::get_mark('^')`.
    LastInsertPos,
}

/// The actual text/cursor transformation for one `InsertCmd`, against a
/// plain `[char]` slice -- deliberately not tied to any particular `Buffer`
/// impl (unlike `Motion`/`apply_motion`, `Buffer` has no mutation methods
/// yet -- see bishedit's own module doc comment) or to `editor::LineEditor`
/// specifically, so every frontend that wants real insert-entry semantics
/// (editor.rs's own line-local Ctrl-E mode, applied to the *live* cursor;
/// repl.rs's full-pane Ctrl+Space mode, applied to a *frozen original*
/// cursor a navigation excursion doesn't move) shares this one
/// implementation instead of each re-deriving the same seven cases.
pub fn apply_insert_cmd(text: &[char], cursor: usize, cmd: InsertCmd) -> (Vec<char>, usize) {
    let cursor = cursor.min(text.len());
    match cmd {
        InsertCmd::Before | InsertCmd::LastInsertPos => (text.to_vec(), cursor),
        InsertCmd::After => (text.to_vec(), (cursor + 1).min(text.len())),
        InsertCmd::LineStart => (text.to_vec(), 0),
        InsertCmd::LineEnd => (text.to_vec(), text.len()),
        InsertCmd::SubstituteChar => {
            let mut new_text = text.to_vec();
            if cursor < new_text.len() {
                new_text.remove(cursor);
            }
            (new_text, cursor)
        }
        InsertCmd::SubstituteLine => (Vec::new(), 0),
        InsertCmd::ChangeToEnd => {
            let mut new_text = text.to_vec();
            new_text.truncate(cursor);
            let len = new_text.len();
            (new_text, len)
        }
    }
}

/// `p`/`P`: splices `insert_text` into `text` at the cursor, `count` times
/// back-to-back. Mirrors `apply_insert_cmd`'s own shape exactly (a plain
/// `[char]` slice in, a new one out -- no `Buffer` involved, matching how
/// every real mutation in this crate works so far). `before` puts at the
/// cursor itself (`P`); otherwise one column after it (`p`), matching
/// vim's own after-the-cursor placement. The cursor ends on the last
/// inserted character, vim's own rule -- repeating the same char run
/// `count` times back-to-back rather than, say, leaving gaps, matches
/// vim's `3p` inserting three concatenated copies as a single block.
/// `insert_text` is expected to already be flattened to a single line by
/// the caller (see `RegisterValue::flatten_to_single_line`) -- this
/// function has no line concept of its own to preserve.
pub fn apply_put(text: &[char], cursor: usize, insert_text: &str, before: bool, count: usize) -> (Vec<char>, usize) {
    let insert_chars: Vec<char> = insert_text.chars().collect();
    if insert_chars.is_empty() || count == 0 {
        return (text.to_vec(), cursor);
    }
    let cursor = cursor.min(text.len());
    let insert_at = if before { cursor } else { (cursor + 1).min(text.len()) };
    let mut block = Vec::with_capacity(insert_chars.len() * count);
    for _ in 0..count {
        block.extend_from_slice(&insert_chars);
    }
    let mut new_text = text.to_vec();
    new_text.splice(insert_at..insert_at, block.iter().copied());
    let new_cursor = insert_at + block.len() - 1;
    (new_text, new_cursor)
}

/// `x`: deletes up to `count` characters starting at the cursor, clamped
/// to the end of the line -- vim's own primitive, not quite reducible to
/// `d{count}l`: `l`'s own motion refuses to move onto/past the last
/// character, so `dl` there is a no-op, but `x` on the last character
/// still deletes it (this clamps the *starting* cursor to the last valid
/// index first, rather than refusing to move past it the way a motion
/// would). Returns the deleted text too, so the caller can write it to a
/// register the same way `y`/`d{motion}` do -- empty text (nothing to
/// delete: an empty buffer) is the caller's own signal to skip that.
pub fn apply_delete_forward(text: &[char], cursor: usize, count: usize) -> (Vec<char>, usize, String) {
    if text.is_empty() || count == 0 {
        return (text.to_vec(), cursor, String::new());
    }
    let cursor = cursor.min(text.len() - 1);
    let end = (cursor + count).min(text.len());
    let deleted: String = text[cursor..end].iter().collect();
    let mut new_text = text.to_vec();
    new_text.drain(cursor..end);
    let new_cursor = cursor.min(new_text.len().saturating_sub(1));
    (new_text, new_cursor, deleted)
}

/// `<C-w>{cmd}`: the same single-letter window shortcuts the shell's own
/// `window` command exposes (see exec.rs's `run_window`), minus the size
/// commands (`+`/`-`/`size`) and `fg <id>` (needs an argument beyond a
/// single letter+count) -- matching plan.md's own scoping for this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCmd {
    Next,
    Previous,
    New,
    Close,
    Split,
    VSplit,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    Balance,
    /// `<C-w>gg` / `<C-w>{N}gg`: go to the first tab, or tab N.
    GotoFirstWindow,
    /// `<C-w>G` / `<C-w>{N}G`: go to the last tab, or tab N.
    GotoLastWindow,
}

#[derive(Debug, Clone)]
enum Pending {
    None,
    G,
    FindChar { till: bool, forward: bool },
    Mark,
    GotoMarkExact,
    GotoMarkLine,
    // `i`/`a` seen while an operator is armed -- awaiting the object
    // character (`w`, `(`, `"`, ...). See `feed_fresh`'s own `i`/`a` arms
    // for why this is only entered in that context, never standalone.
    TextObject { around: bool },
    // `[` / `]` -- vim's own bracket/section-motion leaders, awaiting the
    // second character (`[(`, `])`, `[{`, `]}`, `[[`, `]]`, `[]`, `][`).
    BracketOpen,
    BracketClose,
    // `ys{motion}`/`yss` -- awaiting the one delimiter character after
    // the target (a motion, or `yss`'s own current-line shorthand) has
    // already resolved. Reached via `feed`'s own `surround_armed`
    // handling, not through `feed_inner`'s ordinary dispatch table (see
    // that field's own doc comment) -- but still routed through
    // `feed_inner` like every other `Pending` variant once it's set, for
    // one uniform place that turns "the next key" into a resolved
    // outcome.
    SurroundChar { target: SurroundTarget },
    // `ds` -- awaiting its one target character (`ds(`, `ds"`, ...).
    DeleteSurroundTarget,
    // `cs` -- awaiting its target character.
    ChangeSurroundTarget,
    // `cs{ch}` -- awaiting the replacement character.
    ChangeSurroundChar { ch: char },
    // `r` -- awaiting its one replacement character.
    ReplaceChar,
    Z,
    Window,
    // <C-w>g -- awaiting the second 'g' of <C-w>gg, mirroring plain `gg`'s
    // own two-key shape one level under the window leader.
    WindowG,
    Search { forward: bool, text: String },
    // `"` -- awaiting exactly one register-name character
    // (a-z/A-Z/+/"/_). Unlike every other `Pending` variant, resolving
    // this one doesn't `emit()` anything: it stashes `pending_register`
    // and drops straight back to `Pending::None`, ready for whatever
    // operator or put comes next (see `feed_register`'s own doc comment).
    Register,
}

#[derive(Debug, Clone, Copy)]
enum LastSearch {
    Pattern { forward: bool },
    // `bounded` distinguishes `*`/`#` (word-boundary search) from `g*`/`g#`
    // (plain substring search) -- `n`/`N` need to repeat whichever kind was
    // actually used, not silently upgrade/downgrade it.
    Word { forward: bool, bounded: bool },
}

// A short display label for keys `feed` might reasonably want to echo
// into `current_input`'s transcript -- `None` for keys with no natural
// short label (Delete, AltLeft/Right/Up, ...), which just don't
// contribute to the transcript rather than being an error.
fn key_label(key: Key) -> Option<String> {
    Some(match key {
        Key::Char(c) => c.to_string(),
        Key::Left => "\u{2190}".to_string(),
        Key::Right => "\u{2192}".to_string(),
        Key::Up => "\u{2191}".to_string(),
        Key::Down => "\u{2193}".to_string(),
        Key::Home => "Home".to_string(),
        Key::End => "End".to_string(),
        Key::Enter => "<CR>".to_string(),
        Key::Backspace => "<BS>".to_string(),
        Key::Escape => "<Esc>".to_string(),
        Key::CtrlD => "^D".to_string(),
        Key::CtrlU => "^U".to_string(),
        Key::CtrlF => "^F".to_string(),
        Key::CtrlB => "^B".to_string(),
        Key::CtrlE => "^E".to_string(),
        Key::CtrlY => "^Y".to_string(),
        Key::CtrlW => "^W".to_string(),
        _ => return None,
    })
}

// A Visual selection's own (shape, anchor, cursor) -- `gv`'s memory of the
// last one, and what `end_visual` stashes there. A named alias rather than
// spelling the nested tuple out at each of its two use sites below.
type VisualSelection = (RegisterShape, (usize, usize), (usize, usize));

pub struct VimKeys {
    count: Option<usize>,
    pending: Pending,
    last_find: Option<(char, bool, bool)>, // (ch, till, forward)
    last_search_text: String,
    last_search: Option<LastSearch>,
    // The operator waiting for a motion (or its own double-tap
    // shorthand), and the count that had already accumulated in `count`
    // at the moment it was armed -- stashed separately from `count` so a
    // fresh count can accumulate for the motion that follows without the
    // two clobbering each other (`2y3w`: operator_count becomes Some(2)
    // when `y` arms, then `count` accumulates `3` fresh for `w`). See
    // `feed`'s own doc comment for how these two combine.
    active_operator: Option<Op>,
    operator_count: Option<usize>,
    // `ys{motion}`/`yss` -- set the moment `s` is seen right where `y`'s
    // own double-tap (`yy`) or a motion would otherwise be expected,
    // *without* touching `active_operator` itself (stays `Op::Yank`, so a
    // stray second `y` here still resolves as an ordinary `yy` via the
    // existing double-tap check). Consulted by `feed`'s own active-
    // operator resolution to route what comes next -- an ordinary motion/
    // text object, or `yss`'s own second `s` -- into `Pending::
    // SurroundChar` (one more raw key, the delimiter) instead of an
    // ordinary `Operator`/`OperatorLines`. See `KeyOutcome::AddSurround`'s
    // own doc comment for the outcome this eventually produces.
    surround_armed: bool,
    // `"x` -- the register the *next* operator or put should target.
    // Survives across `Pending::Register` resolving back to
    // `Pending::None` (unlike `count`/`pending`, a register selection
    // isn't itself a sub-prefix continuation, it's a modifier on whatever
    // comes after it), and is dropped by any successfully-resolved
    // outcome that isn't an operator or put -- matching vim silently
    // ignoring a register prefix in front of a bare motion.
    pending_register: Option<char>,
    // A human-readable transcript of the keys fed since the last resolved
    // motion (or the last aborted sequence) -- e.g. "20g" while typing
    // `20gg`, or "/cher" while typing a search. Exists purely for a
    // frontend's status-bar display (see repl.rs's normal-mode status
    // bar); has no effect on how keys are interpreted.
    current_input: String,
    // A snapshot of `current_input` taken at the moment it last resolved
    // into a motion -- e.g. "20gg" -- kept around (not cleared) until the
    // next key starts a new sequence, so a frontend can flash "here's what
    // that just did" for a beat after the motion applies.
    last_completed: String,
    // Visual mode's own state: the shape (`v`/`V`) and the anchor position
    // (the cursor at the moment Visual was entered) a caller supplied via
    // `begin_visual`. `None` means Normal mode. Deliberately not touched
    // by ordinary motion resolution -- a `Motion` outcome while this is
    // `Some` is still just a `Motion`, applied by the caller exactly as it
    // always is; only the *rendering* of what's between this anchor and
    // the buffer's current cursor, and what `y`/`Z`/Escape do, changes,
    // and all of that lives in the caller (repl.rs), not here -- see
    // `is_idle`'s own doc comment for why.
    visual: Option<(RegisterShape, (usize, usize))>,
    // `gv`'s own memory: the shape/anchor/cursor of the last Visual
    // selection, stashed by `end_visual` whenever one ends. `None` until
    // the first Visual selection ever ends.
    last_visual: Option<VisualSelection>,
    // `Ctrl-O`/`Ctrl-I`'s own back/forward navigation history -- a plain
    // two-stack model (same shape a browser's own back/forward history
    // uses), not vim's real fixed-size ring buffer with its own
    // deduplication/staleness rules. `push_jump` (called by a caller right
    // before applying a `motion::is_jump` motion) moves the pre-jump
    // position onto `jump_back` and clears `jump_forward` -- a fresh jump
    // discards any old "redo" history, same as a browser navigating to a
    // new page after going back does.
    jump_back: Vec<(usize, usize)>,
    jump_forward: Vec<(usize, usize)>,
}

impl VimKeys {
    pub fn new() -> Self {
        VimKeys {
            count: None,
            pending: Pending::None,
            last_find: None,
            last_search_text: String::new(),
            last_search: None,
            active_operator: None,
            operator_count: None,
            surround_armed: false,
            pending_register: None,
            visual: None,
            last_visual: None,
            jump_back: Vec::new(),
            jump_forward: Vec::new(),
            current_input: String::new(),
            last_completed: String::new(),
        }
    }

    /// What's been typed so far toward the motion/search/command in
    /// progress -- empty when nothing is pending. Display only.
    pub fn pending_display(&self) -> &str {
        &self.current_input
    }

    /// The keys that produced the most recently applied motion -- stays
    /// populated (for a frontend to flash briefly) until the next key
    /// starts a new sequence. Display only.
    pub fn last_motion_display(&self) -> &str {
        &self.last_completed
    }

    /// The pattern text of the last resolved `/`/`?` search -- empty if
    /// the last search (if any) was word-based (`*`/`#`) instead, or if
    /// there's been no search yet. A frontend rendering search-match
    /// highlighting combines this with `pending_display()` (for a search
    /// still being typed) and, when `last_search_is_word()` is true,
    /// `motion::word_under_cursor` at the buffer's own current cursor --
    /// see that function's own doc comment for why that reliably recovers
    /// a word-search's pattern without this needing to store it itself.
    pub fn last_search_text(&self) -> &str {
        &self.last_search_text
    }

    /// Whether the last resolved search was word-based (`*`/`#`) rather
    /// than pattern-based (`/`/`?`) -- see `last_search_text`'s own doc
    /// comment for why this matters to a caller.
    pub fn last_search_is_word(&self) -> bool {
        matches!(self.last_search, Some(LastSearch::Word { .. }))
    }

    /// Whether Visual mode is currently active.
    pub fn is_visual(&self) -> bool {
        self.visual.is_some()
    }

    /// Visual mode's own shape and anchor, if active -- `None` in Normal
    /// mode. A caller combines this with the buffer's own current cursor
    /// to know what's actually selected right now (this crate never reads
    /// a `Buffer` itself -- see the module's own doc comment).
    pub fn visual_anchor(&self) -> Option<(RegisterShape, (usize, usize))> {
        self.visual
    }

    /// Enters Visual mode: called by a caller that just received
    /// `KeyOutcome::EnterVisual(shape)`, with the buffer's own current
    /// cursor as `anchor`.
    pub fn begin_visual(&mut self, shape: RegisterShape, anchor: (usize, usize)) {
        self.visual = Some((shape, anchor));
    }

    /// Leaves Visual mode -- called by a caller once it's done whatever it
    /// needed the anchor for (committing a selection, yanking, or
    /// cancelling). `cursor` is the buffer's own current cursor at the
    /// moment Visual mode ends -- stashed (alongside the shape/anchor
    /// already known) as `last_visual`, so `gv` can restore the exact
    /// same selection later. Stashed even when leaving via Escape/cancel,
    /// matching vim: `gv` reselects the last Visual selection regardless
    /// of how it ended.
    pub fn end_visual(&mut self, cursor: (usize, usize)) {
        if let Some((shape, anchor)) = self.visual.take() {
            self.last_visual = Some((shape, anchor, cursor));
        }
    }

    /// `gv`'s own target, if any -- the shape/anchor/cursor of the last
    /// Visual selection, however it most recently ended. `None` before any
    /// Visual selection has ever ended yet. See `KeyOutcome::ReselectVisual`'s
    /// own doc comment for how a caller uses this.
    pub fn last_visual(&self) -> Option<VisualSelection> {
        self.last_visual
    }

    /// Records `pos` (the cursor's own position right before a
    /// `motion::is_jump` motion is about to run) as a jump-list entry --
    /// called by a caller ahead of applying such a motion (this crate
    /// never touches a `Buffer` itself). Discards any `jump_forward`
    /// history, same as a browser's own back/forward stack does once you
    /// navigate somewhere new instead of pressing "forward".
    pub fn push_jump(&mut self, pos: (usize, usize)) {
        self.jump_back.push(pos);
        self.jump_forward.clear();
    }

    /// `Ctrl-O`: steps one entry back through the jump list, if there is
    /// one. `current` (the cursor's own position right now) is pushed onto
    /// `jump_forward` so a following `Ctrl-I` can return here.
    pub fn jump_back(&mut self, current: (usize, usize)) -> Option<(usize, usize)> {
        let target = self.jump_back.pop()?;
        self.jump_forward.push(current);
        Some(target)
    }

    /// `Ctrl-I` (same raw byte as Tab -- see this crate's own `feed_fresh`
    /// doc comment on why Normal mode can safely claim it): the mirror of
    /// `jump_back`.
    pub fn jump_forward(&mut self, current: (usize, usize)) -> Option<(usize, usize)> {
        let target = self.jump_forward.pop()?;
        self.jump_back.push(current);
        Some(target)
    }

    /// Takes (consuming) whatever register a `"x` prefix selected, if any.
    /// `feed()` already applies this itself for any outcome it resolves
    /// internally (an `Operator`/`Put`/...); this exists for a caller that
    /// wants to consume it *without* going through `feed()` at all -- e.g.
    /// repl.rs's own Visual-mode `y` handling, which intercepts the key
    /// itself (see `is_idle`'s own doc comment for why) rather than
    /// feeding it through here.
    pub fn take_pending_register(&mut self) -> Option<char> {
        self.pending_register.take()
    }

    /// Whether this is a genuinely fresh dispatch point right now -- no
    /// in-progress count, sub-prefix (`f`/`g`/`m`/a search being typed/...),
    /// or armed operator. A caller wanting to intercept a key *itself*,
    /// ahead of `feed()` (repl.rs already does this for `Z`/`:` -- window
    /// management and quit aren't things this crate should own, per its
    /// own module doc comment; Visual mode's `y`/Escape join them for the
    /// same reason: "are there committed selections" is state this crate
    /// deliberately never sees), needs this first -- otherwise a target
    /// character mid `f`/`F`/`t`/`T`, or a count digit, could be wrongly
    /// stolen instead of reaching its own sub-prefix handler.
    pub fn is_idle(&self) -> bool {
        matches!(self.pending, Pending::None) && self.count.is_none() && self.active_operator.is_none()
    }

    pub fn feed(&mut self, key: Key) -> KeyOutcome {
        if let Some(label) = key_label(key) {
            self.current_input.push_str(&label);
        }
        // A second press of the *active* operator's own trigger key, seen
        // at a fresh dispatch point (not mid sub-prefix) -- vim's
        // double-tap shorthand (`yy`). Entirely orthogonal to feed_inner's
        // normal motion-resolution dispatch below, so it's checked first
        // and short-circuits it entirely.
        if matches!(self.pending, Pending::None) {
            // `ys{motion}`/`yss`: `s`, seen right where `y`'s own
            // double-tap or a motion would otherwise be expected. First
            // press arms `surround_armed` (still `Op::Yank` underneath,
            // so nothing else changes yet); a second `s` right after
            // that is `yss` itself, resolved immediately here since it
            // has no motion to wait for. See `surround_armed`'s own doc
            // comment for how this and the resolution path below (after
            // an ordinary motion resolves instead) connect.
            if let (Some(Op::Yank), Key::Char('s')) = (self.active_operator, key) {
                if self.surround_armed {
                    let count = combine_counts(self.operator_count.take(), self.count.take());
                    self.active_operator = None;
                    self.surround_armed = false;
                    self.pending_register = None;
                    self.pending = Pending::SurroundChar { target: SurroundTarget::Line(count) };
                    self.last_completed = std::mem::take(&mut self.current_input);
                    return KeyOutcome::Pending;
                }
                self.surround_armed = true;
                return KeyOutcome::Pending;
            }
            // `ds`/`cs`: `s`, seen right after `d`/`c` armed -- unlike
            // `ys`, these never take a motion at all, so they resolve
            // straight into their own dedicated `Pending` states instead
            // of reusing the active-operator machinery any further.
            if let (Some(Op::Delete), Key::Char('s')) = (self.active_operator, key) {
                self.active_operator = None;
                self.operator_count = None;
                self.pending_register = None;
                self.pending = Pending::DeleteSurroundTarget;
                return KeyOutcome::Pending;
            }
            if let (Some(Op::Change), Key::Char('s')) = (self.active_operator, key) {
                self.active_operator = None;
                self.operator_count = None;
                self.pending_register = None;
                self.pending = Pending::ChangeSurroundTarget;
                return KeyOutcome::Pending;
            }
            if let (Some(op), Key::Char(c)) = (self.active_operator, key) {
                if c == op.trigger_char() {
                    let count = combine_counts(self.operator_count.take(), self.count.take());
                    let register = self.pending_register.take();
                    self.active_operator = None;
                    self.last_completed = std::mem::take(&mut self.current_input);
                    return KeyOutcome::OperatorLines(op, count, register);
                }
            }
        }
        let outcome = self.feed_inner(key);
        if let Some(op) = self.active_operator {
            return match outcome {
                // The motion that resolves an operator: fold in the count
                // that had accumulated before the operator was armed and
                // whatever register was selected, then hand back an
                // Operator instead of a bare Motion. Note this doesn't
                // check whether `m` is actually a valid operator target
                // (see motion::motion_shape) -- an invalid one (e.g.
                // Ctrl-D) still becomes an `Operator`, just one that
                // later resolves to nothing when motion::motion_range
                // rejects it, which is behaviorally identical to
                // aborting here (no register write, no cursor move
                // either way) without vimkeys.rs needing to reach into
                // motion.rs's own classification.
                KeyOutcome::Motion(m, motion_count) => {
                    self.active_operator = None;
                    let count = combine_counts(self.operator_count.take(), motion_count);
                    // `ys{motion}`: the motion just resolved the *target*,
                    // not the whole command -- one more raw key (the
                    // delimiter) is still needed, so this becomes a
                    // `Pending::SurroundChar` instead of an `Operator`.
                    if self.surround_armed {
                        self.surround_armed = false;
                        self.pending_register = None;
                        self.pending = Pending::SurroundChar { target: SurroundTarget::Motion(m, count) };
                        return KeyOutcome::Pending;
                    }
                    let register = self.pending_register.take();
                    KeyOutcome::Operator(op, m, count, register)
                }
                // A sub-prefix (f/F/t/T/g/`/'/...) is still resolving --
                // stay armed, nothing to reinterpret yet.
                KeyOutcome::Pending => KeyOutcome::Pending,
                // Anything else (None/EnterInsert/Window) isn't a valid
                // motion for an operator -- cancel it, matching vim's own
                // "invalid operator continuation beeps and does nothing"
                // behavior (consumed, not forwarded: whatever that key
                // would have done standalone doesn't happen either).
                _ => {
                    self.active_operator = None;
                    self.operator_count = None;
                    self.pending_register = None;
                    self.surround_armed = false;
                    KeyOutcome::None
                }
            };
        }
        // No operator was pending, so `outcome` is whatever feed_inner
        // resolved on its own. A register prefix in front of a bare
        // motion/window-cmd/insert-entry is simply irrelevant (matches
        // vim silently ignoring it) -- but only once something actually
        // resolved: a `Pending` result means a sub-prefix (or the
        // register selection itself) is still being typed, and dropping
        // the register mid-sequence would lose it before it ever had a
        // chance to reach an operator or put. `emit_put` already
        // consumes it itself on the Put path, so this is a no-op there,
        // not a second, conflicting clear.
        if !matches!(outcome, KeyOutcome::Pending) {
            self.pending_register = None;
        }
        outcome
    }

    fn feed_inner(&mut self, key: Key) -> KeyOutcome {
        match std::mem::replace(&mut self.pending, Pending::None) {
            Pending::None => self.feed_fresh(key),
            Pending::G => self.feed_g(key),
            Pending::FindChar { till, forward } => self.feed_find_char(key, till, forward),
            Pending::Mark => self.feed_mark(key, MarkKind::Set),
            Pending::GotoMarkExact => self.feed_mark(key, MarkKind::GotoExact),
            Pending::GotoMarkLine => self.feed_mark(key, MarkKind::GotoLine),
            Pending::TextObject { around } => self.feed_text_object(key, around),
            Pending::BracketOpen => self.feed_bracket_open(key),
            Pending::BracketClose => self.feed_bracket_close(key),
            Pending::SurroundChar { target } => self.feed_surround_char(key, target),
            Pending::DeleteSurroundTarget => self.feed_delete_surround_target(key),
            Pending::ChangeSurroundTarget => self.feed_change_surround_target(key),
            Pending::ChangeSurroundChar { ch } => self.feed_change_surround_char(key, ch),
            Pending::ReplaceChar => self.feed_replace_char(key),
            Pending::Z => self.feed_z(key),
            Pending::Window => self.feed_window(key),
            Pending::WindowG => self.feed_window_g(key),
            Pending::Search { forward, text } => self.feed_search(key, forward, text),
            Pending::Register => self.feed_register(key),
        }
    }

    fn emit(&mut self, motion: Motion) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        // pending_register is deliberately *not* cleared here -- this
        // resolves a Motion, but `feed` (the only caller of feed_inner,
        // which is what actually calls this) doesn't yet know whether an
        // operator is waiting to fold this Motion into an Operator, which
        // still needs the register. `feed` itself drops a leftover
        // register after any outcome that turns out *not* to have been
        // claimed by an operator or put.
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Motion(motion, count)
    }

    fn emit_window(&mut self, cmd: WindowCmd) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Window(cmd, count)
    }

    // Unlike emit()/emit_window(), the count is dropped rather than
    // returned -- see KeyOutcome::EnterInsert's own doc comment on why a
    // leading count on an insert-entry command has no effect yet.
    fn emit_insert(&mut self, cmd: InsertCmd) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterInsert(cmd)
    }

    // `v`/`V`: same shape as emit_insert -- a count typed before entering
    // Visual mode has no effect yet, same "not supported in this pass" as
    // an insert-entry command's own leading count. Doesn't touch
    // `self.visual` itself -- that only happens once the caller calls
    // `begin_visual` back with the anchor, which this function has no way
    // to know (see `KeyOutcome::EnterVisual`'s own doc comment).
    fn emit_visual(&mut self, shape: RegisterShape) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterVisual(shape)
    }

    fn emit_replace(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterReplace
    }

    fn emit_reselect_visual(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::ReselectVisual
    }

    fn emit_jump(&mut self, forward: bool) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Jump { forward }
    }

    // `y{motion}`'s first half: stashes whatever count had already
    // accumulated (the `[count1]` in vim's `[count1]op[count2]motion`)
    // and arms `active_operator` -- the actual resolution (into
    // `Operator`/`OperatorLines`) happens in `feed`, above, once a motion
    // or the double-tap shorthand resolves.
    fn emit_operator(&mut self, op: Op) -> KeyOutcome {
        self.operator_count = self.count.take();
        self.active_operator = Some(op);
        KeyOutcome::Pending
    }

    // `D`/`X`'s own shape: an operator applied to a *fixed* motion
    // (`LineEnd`/`Left`), resolved immediately rather than arming a
    // pending operator -- the same "direct emission" shortcut `Y` already
    // uses for `yy`, just carrying an explicit `Motion` instead of going
    // through `OperatorLines`.
    fn emit_operator_direct(&mut self, op: Op, motion: Motion) -> KeyOutcome {
        let count = self.count.take();
        let register = self.pending_register.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Operator(op, motion, count, register)
    }

    fn emit_join(&mut self, with_space: bool) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Join { count, with_space }
    }

    fn emit_put(&mut self, before: bool) -> KeyOutcome {
        let count = self.count.take();
        let register = self.pending_register.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Put { before, count, register }
    }

    fn abort(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.pending_register = None;
        self.current_input.clear();
        KeyOutcome::None
    }

    fn feed_fresh(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(c) if c.is_ascii_digit() => {
                if c == '0' && self.count.is_none() {
                    return self.emit(Motion::LineStart);
                }
                let d = (c as u8 - b'0') as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                KeyOutcome::Pending
            }
            Key::Char('h') | Key::Left => self.emit(Motion::Left),
            Key::Char('l') | Key::Right => self.emit(Motion::Right),
            Key::Char('j') | Key::Down => self.emit(Motion::Down),
            Key::Char('k') | Key::Up => self.emit(Motion::Up),
            Key::Char('^') => self.emit(Motion::LineFirstNonBlank),
            Key::Char('$') | Key::End => self.emit(Motion::LineEnd),
            Key::Home => self.emit(Motion::LineStart),
            Key::Char('|') => self.emit(Motion::GotoColumn),
            Key::Char('G') => self.emit(Motion::GotoLastLine),
            Key::Char('w') => self.emit(Motion::WordForward),
            Key::Char('W') => self.emit(Motion::WordForwardBig),
            Key::Char('b') => self.emit(Motion::WordBackward),
            Key::Char('B') => self.emit(Motion::WordBackwardBig),
            Key::Char('e') => self.emit(Motion::WordEnd),
            Key::Char('E') => self.emit(Motion::WordEndBig),
            Key::Char('f') => {
                self.pending = Pending::FindChar { till: false, forward: true };
                KeyOutcome::Pending
            }
            Key::Char('F') => {
                self.pending = Pending::FindChar { till: false, forward: false };
                KeyOutcome::Pending
            }
            Key::Char('t') => {
                self.pending = Pending::FindChar { till: true, forward: true };
                KeyOutcome::Pending
            }
            Key::Char('T') => {
                self.pending = Pending::FindChar { till: true, forward: false };
                KeyOutcome::Pending
            }
            Key::Char(';') => self.emit_last_find(true),
            Key::Char(',') => self.emit_last_find(false),
            Key::Char('H') => self.emit(Motion::ScreenTop),
            Key::Char('M') => self.emit(Motion::ScreenMiddle),
            Key::Char('L') => self.emit(Motion::ScreenBottom),
            Key::CtrlD => self.emit(Motion::HalfPageDown),
            Key::CtrlU => self.emit(Motion::HalfPageUp),
            Key::CtrlF => self.emit(Motion::PageDown),
            Key::CtrlB => self.emit(Motion::PageUp),
            Key::CtrlE => self.emit(Motion::ScrollLineDown),
            Key::CtrlY => self.emit(Motion::ScrollLineUp),
            Key::Char('{') => self.emit(Motion::ParagraphBackward),
            Key::Char('}') => self.emit(Motion::ParagraphForward),
            Key::Char('(') => self.emit(Motion::SentenceBackward),
            Key::Char(')') => self.emit(Motion::SentenceForward),
            Key::Char('+') | Key::Enter => self.emit(Motion::NextLineNonBlank),
            Key::Char('-') => self.emit(Motion::PrevLineNonBlank),
            // A count in front of '%' means "go to that percentage of the
            // file" instead of vim's bare bracket-matching -- distinct
            // motions since they have nothing in common beyond the key.
            Key::Char('%') if self.count.is_some() => self.emit(Motion::GotoPercent),
            Key::Char('%') => self.emit(Motion::MatchPair),
            Key::Char('m') => {
                self.pending = Pending::Mark;
                KeyOutcome::Pending
            }
            Key::Char('`') => {
                self.pending = Pending::GotoMarkExact;
                KeyOutcome::Pending
            }
            Key::Char('\'') => {
                self.pending = Pending::GotoMarkLine;
                KeyOutcome::Pending
            }
            Key::Char('[') => {
                self.pending = Pending::BracketOpen;
                KeyOutcome::Pending
            }
            Key::Char(']') => {
                self.pending = Pending::BracketClose;
                KeyOutcome::Pending
            }
            Key::Char('/') => {
                self.pending = Pending::Search { forward: true, text: String::new() };
                KeyOutcome::Pending
            }
            Key::Char('?') => {
                self.pending = Pending::Search { forward: false, text: String::new() };
                KeyOutcome::Pending
            }
            Key::Char('n') => self.emit_last_search(true),
            Key::Char('N') => self.emit_last_search(false),
            Key::Char('*') => {
                self.last_search = Some(LastSearch::Word { forward: true, bounded: true });
                self.emit(Motion::SearchWordForward)
            }
            Key::Char('#') => {
                self.last_search = Some(LastSearch::Word { forward: false, bounded: true });
                self.emit(Motion::SearchWordBackward)
            }
            Key::Char('z') => {
                self.pending = Pending::Z;
                KeyOutcome::Pending
            }
            Key::Char('g') => {
                self.pending = Pending::G;
                KeyOutcome::Pending
            }
            Key::CtrlW => {
                self.pending = Pending::Window;
                KeyOutcome::Pending
            }
            Key::CtrlO => self.emit_jump(false),
            // `Ctrl-I` is indistinguishable from Tab at the raw-byte level
            // in a standard terminal (both are 0x09 -- editor.rs's own key
            // decoder maps that byte to `Key::Tab` unconditionally, same
            // as real vim treats them as the same key), so this claims
            // `Key::Tab` for jump-forward here. Safe: Tab only means
            // completion-cycling in the shell's own *typing* loop
            // (editor.rs's `read_line`), a separate code path this
            // Normal-mode dispatch is never reached from -- see
            // `run_line_normal_mode`'s own doc comment for how Ctrl-E's
            // excursion into here relates to that loop.
            Key::Tab => self.emit_jump(true),
            // `i`/`a` while an operator is armed (`diw`, `ca(`, ...) *or*
            // while in Visual mode (`viw`, `va(`, ...) name a text object
            // instead of entering insert mode -- vim's own dual meaning for
            // these two keys. Gated rather than checked inside
            // `feed_text_object` itself, so a bare `i`/`a` at a fresh
            // dispatch point keeps meaning insert-entry exactly as it
            // always has (this arm must stay ahead of the plain
            // `emit_insert` arms below -- first match wins). The Visual-mode
            // case still resolves to a plain `KeyOutcome::Motion` here (this
            // crate has no notion of "re-anchor the selection" -- see
            // `KeyOutcome::Motion`'s own doc comment); the caller is
            // responsible for special-casing `Motion::TextObject` while
            // `is_visual()` to move *both* ends instead of calling
            // `apply_motion` (which only moves the cursor).
            Key::Char('i') if self.active_operator.is_some() || self.visual.is_some() => {
                self.pending = Pending::TextObject { around: false };
                KeyOutcome::Pending
            }
            Key::Char('a') if self.active_operator.is_some() || self.visual.is_some() => {
                self.pending = Pending::TextObject { around: true };
                KeyOutcome::Pending
            }
            Key::Char('i') => self.emit_insert(InsertCmd::Before),
            Key::Char('a') => self.emit_insert(InsertCmd::After),
            Key::Char('I') => self.emit_insert(InsertCmd::LineStart),
            Key::Char('A') => self.emit_insert(InsertCmd::LineEnd),
            Key::Char('s') => self.emit_insert(InsertCmd::SubstituteChar),
            Key::Char('S') => self.emit_insert(InsertCmd::SubstituteLine),
            Key::Char('C') => self.emit_insert(InsertCmd::ChangeToEnd),
            Key::Char('r') => {
                self.pending = Pending::ReplaceChar;
                KeyOutcome::Pending
            }
            Key::Char('R') => self.emit_replace(),
            Key::Char('y') => self.emit_operator(Op::Yank),
            // `Y` is vim's own direct synonym for `yy` -- not "yank to end
            // of line" the way `D`/`C` work relative to their lowercase
            // motion-based forms, so it's resolved the same way the `yy`
            // double-tap is (in `feed`, above) rather than via
            // `emit_operator`, which only arms a *pending* operator.
            Key::Char('Y') => {
                let count = self.count.take();
                let register = self.pending_register.take();
                self.pending = Pending::None;
                self.last_completed = std::mem::take(&mut self.current_input);
                KeyOutcome::OperatorLines(Op::Yank, count, register)
            }
            Key::Char('d') => self.emit_operator(Op::Delete),
            Key::Char('c') => self.emit_operator(Op::Change),
            // `D`: delete from the cursor through end of line, staying in
            // Normal mode -- vim's own `d$` shorthand. Unlike `C` (the
            // same range, but entering insert afterward), nothing already
            // handled this, so it's new rather than a redirect onto an
            // existing command.
            Key::Char('D') => self.emit_operator_direct(Op::Delete, Motion::LineEnd),
            // `X`: delete backward -- vim's own `d{count}h` shorthand.
            // `Motion::Left` already clamps at column 0 exactly the way
            // `X` needs (a no-op there), so this needs no special casing
            // the way `x` (below) does.
            Key::Char('X') => self.emit_operator_direct(Op::Delete, Motion::Left),
            // `x`: delete forward -- *not* `emit_operator_direct(Delete,
            // Right)`. See `KeyOutcome::DeleteCharForward`'s own doc
            // comment on why `Motion::Right` can't stand in for this.
            Key::Char('x') => {
                let count = self.count.take();
                let register = self.pending_register.take();
                self.pending = Pending::None;
                self.last_completed = std::mem::take(&mut self.current_input);
                KeyOutcome::DeleteCharForward { count, register }
            }
            Key::Char('~') => {
                let count = self.count.take();
                self.pending = Pending::None;
                self.last_completed = std::mem::take(&mut self.current_input);
                KeyOutcome::ToggleCase { count }
            }
            Key::CtrlA => {
                let count = self.count.take().unwrap_or(1).max(1) as i64;
                self.pending = Pending::None;
                self.last_completed = std::mem::take(&mut self.current_input);
                KeyOutcome::AdjustNumber { delta: count }
            }
            Key::CtrlX => {
                let count = self.count.take().unwrap_or(1).max(1) as i64;
                self.pending = Pending::None;
                self.last_completed = std::mem::take(&mut self.current_input);
                KeyOutcome::AdjustNumber { delta: -count }
            }
            Key::Char('p') => self.emit_put(false),
            Key::Char('P') => self.emit_put(true),
            Key::Char('J') => self.emit_join(true),
            Key::Char('"') => {
                self.pending = Pending::Register;
                KeyOutcome::Pending
            }
            // Guarded on `self.visual.is_none()`: pressing `v`/`V` again
            // while already in Visual mode is simply ignored in this pass
            // (falls to `_ => self.abort()` below, same as any other
            // unrecognized key -- clears a stray count/prefix, leaves
            // Visual mode itself untouched) -- no shape-switch/re-anchor
            // vim nuance yet.
            Key::Char('v') if self.visual.is_none() => self.emit_visual(RegisterShape::Char),
            Key::Char('V') if self.visual.is_none() => self.emit_visual(RegisterShape::Line),
            _ => self.abort(),
        }
    }

    fn feed_register(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(c @ ('a'..='z' | 'A'..='Z' | '+' | '"' | '_')) => {
                self.pending_register = Some(c);
                // Deliberately *not* an `emit*` call: a register selection
                // isn't itself a resolved command, it's a modifier waiting
                // for whatever operator or put comes next. `self.pending`
                // is already `Pending::None` (set by the `mem::replace` in
                // `feed_inner` before this ran), so the very next key
                // dispatches through `feed_fresh` as if nothing happened,
                // except now with `pending_register` armed.
                KeyOutcome::Pending
            }
            _ => self.abort(),
        }
    }

    fn feed_g(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('g') => self.emit(Motion::GotoFirstLine),
            Key::Char('_') => self.emit(Motion::LineLastNonBlank),
            Key::Char('e') => self.emit(Motion::WordEndBackward),
            Key::Char('E') => self.emit(Motion::WordEndBackwardBig),
            Key::Char('*') => {
                self.last_search = Some(LastSearch::Word { forward: true, bounded: false });
                self.emit(Motion::SearchWordForwardUnbounded)
            }
            Key::Char('#') => {
                self.last_search = Some(LastSearch::Word { forward: false, bounded: false });
                self.emit(Motion::SearchWordBackwardUnbounded)
            }
            Key::Char('J') => self.emit_join(false),
            Key::Char('v') => self.emit_reselect_visual(),
            Key::Char('i') => self.emit_insert(InsertCmd::LastInsertPos),
            // `gu`/`gU`/`g~`: arms a case-transform operator, resolved
            // exactly like `y`/`d`/`c` from here on (an ordinary motion,
            // a text object, or the double-tap line shorthand via each
            // one's own `trigger_char`) -- `feed`'s own active-operator
            // handling neither knows nor cares that these three don't
            // write a register, so no changes were needed there.
            Key::Char('u') => self.emit_operator(Op::Lowercase),
            Key::Char('U') => self.emit_operator(Op::Uppercase),
            Key::Char('~') => self.emit_operator(Op::CaseToggle),
            _ => self.abort(),
        }
    }

    fn feed_find_char(&mut self, key: Key, till: bool, forward: bool) -> KeyOutcome {
        match key {
            Key::Char(c) => {
                self.last_find = Some((c, till, forward));
                self.emit(Motion::FindChar { ch: c, till, forward })
            }
            _ => self.abort(),
        }
    }

    fn feed_mark(&mut self, key: Key, kind: MarkKind) -> KeyOutcome {
        match (key, kind) {
            (Key::Char(c), MarkKind::Set) if c.is_ascii_lowercase() => self.emit(Motion::SetMark(c)),
            (Key::Char(c), MarkKind::GotoExact) if c.is_ascii_lowercase() => self.emit(Motion::GotoMark(c)),
            (Key::Char(c), MarkKind::GotoLine) if c.is_ascii_lowercase() => self.emit(Motion::GotoMarkLine(c)),
            // `^` -- vim's own last-insert-position mark (set
            // automatically, e.g. by fileeditor.rs's `run_insert_mode` --
            // see `gi`'s own doc comment); not settable by name (`m^`
            // isn't a thing in real vim either), so this only appears in
            // the two Goto arms.
            (Key::Char('^'), MarkKind::GotoExact) => self.emit(Motion::GotoMark('^')),
            (Key::Char('^'), MarkKind::GotoLine) => self.emit(Motion::GotoMarkLine('^')),
            // `.` -- vim's own last-change-position mark, set
            // automatically by every real mutation (see textbuffer.rs's
            // own `insert_text`/`delete_range`/`join_lines`); same
            // not-settable-by-name treatment as `^`.
            (Key::Char('.'), MarkKind::GotoExact) => self.emit(Motion::GotoMark('.')),
            (Key::Char('.'), MarkKind::GotoLine) => self.emit(Motion::GotoMarkLine('.')),
            // ``` ``` / `''` -- vim's own "position before the last jump"
            // -- see `VimKeys::push_jump`'s own doc comment for who
            // writes it (a plain `Buffer` mark, keyed by an apostrophe a
            // user can never set by name either).
            (Key::Char('`'), MarkKind::GotoExact) => self.emit(Motion::GotoMark('\'')),
            (Key::Char('\''), MarkKind::GotoLine) => self.emit(Motion::GotoMarkLine('\'')),
            _ => self.abort(),
        }
    }

    fn feed_bracket_open(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('(') => self.emit(Motion::UnmatchedOpenParen),
            Key::Char('{') => self.emit(Motion::UnmatchedOpenBrace),
            Key::Char('[') => self.emit(Motion::SectionBackward),
            Key::Char(']') => self.emit(Motion::SectionBackwardEnd),
            _ => self.abort(),
        }
    }

    fn feed_bracket_close(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(')') => self.emit(Motion::UnmatchedCloseParen),
            Key::Char('}') => self.emit(Motion::UnmatchedCloseBrace),
            Key::Char(']') => self.emit(Motion::SectionForward),
            Key::Char('[') => self.emit(Motion::SectionForwardEnd),
            _ => self.abort(),
        }
    }

    // `ys{motion}`/`yss`'s own final key: the delimiter character. Any
    // character `motion::surround_delims` doesn't recognize aborts, same
    // as an invalid operator continuation elsewhere in this file.
    fn feed_surround_char(&mut self, key: Key, target: SurroundTarget) -> KeyOutcome {
        let Key::Char(c) = key else {
            return self.abort();
        };
        if super::motion::surround_delims(c).is_none() {
            return self.abort();
        }
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::AddSurround { target, ch: c }
    }

    // `ds{ch}`'s own target character -- see `motion::surround_target_kind`
    // for which characters are valid here.
    fn feed_delete_surround_target(&mut self, key: Key) -> KeyOutcome {
        let Key::Char(c) = key else {
            return self.abort();
        };
        if super::motion::surround_target_kind(c).is_none() {
            return self.abort();
        }
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::DeleteSurround { ch: c }
    }

    // `cs{ch}...`'s own target character -- same validity rule as `ds`'s,
    // but stays pending for one more key (the replacement) instead of
    // resolving immediately.
    fn feed_change_surround_target(&mut self, key: Key) -> KeyOutcome {
        let Key::Char(c) = key else {
            return self.abort();
        };
        if super::motion::surround_target_kind(c).is_none() {
            return self.abort();
        }
        self.pending = Pending::ChangeSurroundChar { ch: c };
        KeyOutcome::Pending
    }

    // `cs{ch}{replacement}`'s own final key.
    fn feed_change_surround_char(&mut self, key: Key, ch: char) -> KeyOutcome {
        let Key::Char(replacement) = key else {
            return self.abort();
        };
        if super::motion::surround_delims(replacement).is_none() {
            return self.abort();
        }
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::ChangeSurround { ch, replacement }
    }

    // `r{ch}` -- any character (no validity restriction, unlike the
    // surround targets above: `r` can replace with literally anything).
    fn feed_replace_char(&mut self, key: Key) -> KeyOutcome {
        let Key::Char(c) = key else {
            return self.abort();
        };
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::ReplaceChar { ch: c, count }
    }

    // The object character after `i`/`a` while an operator is armed --
    // resolves to `Motion::TextObject`, folded into an `Operator` by
    // `feed`'s own existing active-operator handling exactly like any other
    // motion (nothing new needed there). `b`/`B` are vim's own aliases for
    // `(`/`{`; tag objects (`it`/`at`) aren't supported (see
    // `TextObjectKind`'s own doc comment).
    fn feed_text_object(&mut self, key: Key, around: bool) -> KeyOutcome {
        let kind = match key {
            Key::Char('w') => TextObjectKind::Word,
            Key::Char('W') => TextObjectKind::WordBig,
            Key::Char('s') => TextObjectKind::Sentence,
            Key::Char('p') => TextObjectKind::Paragraph,
            Key::Char('(') | Key::Char(')') | Key::Char('b') => TextObjectKind::Paren,
            Key::Char('{') | Key::Char('}') | Key::Char('B') => TextObjectKind::Brace,
            Key::Char('[') | Key::Char(']') => TextObjectKind::Bracket,
            Key::Char('<') | Key::Char('>') => TextObjectKind::Angle,
            Key::Char('"') => TextObjectKind::DoubleQuote,
            Key::Char('\'') => TextObjectKind::SingleQuote,
            Key::Char('`') => TextObjectKind::Backtick,
            _ => return self.abort(),
        };
        self.emit(Motion::TextObject(kind, around))
    }

    fn feed_z(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('z') => self.emit(Motion::ScrollCenter),
            Key::Char('t') => self.emit(Motion::ScrollTop),
            Key::Char('b') => self.emit(Motion::ScrollBottom),
            _ => self.abort(),
        }
    }

    fn feed_window(&mut self, key: Key) -> KeyOutcome {
        match key {
            // Unlike a bare `<C-w>`-less count (accumulated in feed_fresh,
            // which only applies to *repeating* a command), digits typed
            // here -- inside the leader, before its resolving key -- are
            // for `<C-w>{N}gg`/`<C-w>{N}G`'s absolute tab position. Stays
            // pending, mirroring feed_fresh's own digit arm.
            Key::Char(c) if c.is_ascii_digit() => {
                let d = (c as u8 - b'0') as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + d);
                self.pending = Pending::Window;
                KeyOutcome::Pending
            }
            Key::Char('n') => self.emit_window(WindowCmd::Next),
            Key::Char('p') => self.emit_window(WindowCmd::Previous),
            Key::Char('c') => self.emit_window(WindowCmd::New),
            Key::Char('q') => self.emit_window(WindowCmd::Close),
            Key::Char('s') => self.emit_window(WindowCmd::Split),
            Key::Char('v') => self.emit_window(WindowCmd::VSplit),
            Key::Char('h') => self.emit_window(WindowCmd::FocusLeft),
            Key::Char('j') => self.emit_window(WindowCmd::FocusDown),
            Key::Char('k') => self.emit_window(WindowCmd::FocusUp),
            Key::Char('l') => self.emit_window(WindowCmd::FocusRight),
            Key::Char('=') => self.emit_window(WindowCmd::Balance),
            Key::Char('g') => {
                self.pending = Pending::WindowG;
                KeyOutcome::Pending
            }
            Key::Char('G') => self.emit_window(WindowCmd::GotoLastWindow),
            _ => self.abort(),
        }
    }

    fn feed_window_g(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char('g') => self.emit_window(WindowCmd::GotoFirstWindow),
            _ => self.abort(),
        }
    }

    fn feed_search(&mut self, key: Key, forward: bool, mut text: String) -> KeyOutcome {
        match key {
            Key::Enter => {
                self.last_search_text = text.clone();
                self.last_search = Some(LastSearch::Pattern { forward });
                self.emit(if forward {
                    Motion::SearchForward(text)
                } else {
                    Motion::SearchBackward(text)
                })
            }
            Key::Escape => self.abort(),
            Key::Backspace => {
                text.pop();
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
            Key::Char(c) => {
                text.push(c);
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
            _ => {
                // Ignore anything else while typing a search string rather
                // than aborting it -- a stray unrecognized key shouldn't
                // discard what's already been typed.
                self.pending = Pending::Search { forward, text };
                KeyOutcome::Pending
            }
        }
    }

    fn emit_last_find(&mut self, same_direction: bool) -> KeyOutcome {
        match self.last_find {
            Some((ch, till, forward)) => {
                let forward = if same_direction { forward } else { !forward };
                self.emit(Motion::FindChar { ch, till, forward })
            }
            None => self.abort(),
        }
    }

    fn emit_last_search(&mut self, same_direction: bool) -> KeyOutcome {
        match self.last_search {
            Some(LastSearch::Pattern { forward }) => {
                let forward = if same_direction { forward } else { !forward };
                let text = self.last_search_text.clone();
                self.emit(if forward {
                    Motion::SearchForward(text)
                } else {
                    Motion::SearchBackward(text)
                })
            }
            Some(LastSearch::Word { forward, bounded }) => {
                let forward = if same_direction { forward } else { !forward };
                self.emit(match (forward, bounded) {
                    (true, true) => Motion::SearchWordForward,
                    (true, false) => Motion::SearchWordForwardUnbounded,
                    (false, true) => Motion::SearchWordBackward,
                    (false, false) => Motion::SearchWordBackwardUnbounded,
                })
            }
            None => self.abort(),
        }
    }
}

impl Default for VimKeys {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
enum MarkKind {
    Set,
    GotoExact,
    GotoLine,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(vk: &mut VimKeys, keys: &[Key]) -> Vec<KeyOutcome> {
        keys.iter().cloned().map(|k| vk.feed(k)).collect()
    }

    fn last(vk: &mut VimKeys, keys: &[Key]) -> KeyOutcome {
        feed_all(vk, keys).pop().unwrap()
    }

    #[test]
    fn simple_single_key_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('h')), KeyOutcome::Motion(Motion::Left, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('0')), KeyOutcome::Motion(Motion::LineStart, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('%')), KeyOutcome::Motion(Motion::MatchPair, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('$')), KeyOutcome::Motion(Motion::LineEnd, None));
    }

    #[test]
    fn arrow_and_home_end_alias_to_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Left), KeyOutcome::Motion(Motion::Left, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Right), KeyOutcome::Motion(Motion::Right, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Up), KeyOutcome::Motion(Motion::Up, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Down), KeyOutcome::Motion(Motion::Down, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Home), KeyOutcome::Motion(Motion::LineStart, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::End), KeyOutcome::Motion(Motion::LineEnd, None));
    }

    #[test]
    fn ctrl_keys_map_to_screen_motions() {
        let cases = [
            (Key::CtrlD, Motion::HalfPageDown),
            (Key::CtrlU, Motion::HalfPageUp),
            (Key::CtrlF, Motion::PageDown),
            (Key::CtrlB, Motion::PageUp),
            (Key::CtrlE, Motion::ScrollLineDown),
            (Key::CtrlY, Motion::ScrollLineUp),
        ];
        for (key, motion) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(key), KeyOutcome::Motion(motion, None));
        }
    }

    #[test]
    fn count_accumulates_across_digits() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, Some(3)));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('1')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('0')), KeyOutcome::Pending); // '0' after a digit isn't LineStart
        assert_eq!(vk.feed(Key::Char('j')), KeyOutcome::Motion(Motion::Down, Some(10)));
    }

    #[test]
    fn g_prefix_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Motion(Motion::GotoFirstLine, None));

        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('3'), Key::Char('g'), Key::Char('g')]),
            KeyOutcome::Motion(Motion::GotoFirstLine, Some(3))
        );

        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('_')]),
            KeyOutcome::Motion(Motion::LineLastNonBlank, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('e')]),
            KeyOutcome::Motion(Motion::WordEndBackward, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('g'), Key::Char('E')]),
            KeyOutcome::Motion(Motion::WordEndBackwardBig, None)
        );
    }

    #[test]
    fn unrecognized_continuation_aborts_pending_and_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None); // 'gx' isn't a thing
        // the aborted count/prefix must not leak into the next command
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn find_char_and_repeat() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('f')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('x')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None)
        );
        // ';' repeats the same direction
        assert_eq!(
            vk.feed(Key::Char(';')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None)
        );
        // ',' repeats with direction flipped
        assert_eq!(
            vk.feed(Key::Char(',')),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: false }, None)
        );
    }

    #[test]
    fn till_and_backward_find_char() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('t'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: true, forward: true }, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('T'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: true, forward: false }, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('F'), Key::Char('y')]),
            KeyOutcome::Motion(Motion::FindChar { ch: 'y', till: false, forward: false }, None)
        );
    }

    #[test]
    fn semicolon_with_no_prior_find_is_a_no_op() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char(';')), KeyOutcome::None);
    }

    #[test]
    fn marks() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('m'), Key::Char('a')]),
            KeyOutcome::Motion(Motion::SetMark('a'), None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('`'), Key::Char('a')]),
            KeyOutcome::Motion(Motion::GotoMark('a'), None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('\''), Key::Char('a')]),
            KeyOutcome::Motion(Motion::GotoMarkLine('a'), None)
        );
    }

    #[test]
    fn join_and_gjoin_with_and_without_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('J')), KeyOutcome::Join { count: None, with_space: true });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('J')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Join { count: Some(3), with_space: true });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('J')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Join { count: None, with_space: false });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('g'), Key::Char('J')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Join { count: Some(3), with_space: false });
    }

    #[test]
    fn join_as_operator_target_is_invalid_and_cancels_the_operator() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('J')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn percent_without_count_is_match_pair_with_count_is_goto_percent() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('%')), KeyOutcome::Motion(Motion::MatchPair, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('5'), Key::Char('0'), Key::Char('%')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoPercent, Some(50)));
    }

    #[test]
    fn ctrl_o_and_tab_resolve_to_jump_outcomes() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlO), KeyOutcome::Jump { forward: false });
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Tab), KeyOutcome::Jump { forward: true });
    }

    #[test]
    fn jump_back_and_forward_round_trip() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.jump_back((5, 0)), None); // nothing recorded yet
        vk.push_jump((0, 0));
        vk.push_jump((10, 0));
        assert_eq!(vk.jump_back((20, 0)), Some((10, 0)));
        assert_eq!(vk.jump_back((10, 0)), Some((0, 0)));
        assert_eq!(vk.jump_back((0, 0)), None); // exhausted
        // and back the other way
        assert_eq!(vk.jump_forward((0, 0)), Some((10, 0)));
        assert_eq!(vk.jump_forward((10, 0)), Some((20, 0)));
        assert_eq!(vk.jump_forward((20, 0)), None); // exhausted
    }

    #[test]
    fn push_jump_clears_forward_history() {
        let mut vk = VimKeys::new();
        vk.push_jump((0, 0));
        assert_eq!(vk.jump_back((5, 0)), Some((0, 0)));
        assert_eq!(vk.jump_forward((0, 0)), Some((5, 0))); // forward history exists
        vk.push_jump((5, 0));
        assert_eq!(vk.jump_forward((5, 0)), None); // ...until a fresh jump discards it
    }

    #[test]
    fn backtick_backtick_and_quote_quote_resolve_to_the_apostrophe_mark() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('`'), Key::Char('`')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMark('\''), None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('\''), Key::Char('\'')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMarkLine('\''), None));
    }

    #[test]
    fn caret_mark_goto_works_but_cannot_be_set_by_name() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('`'), Key::Char('^')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMark('^'), None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('\''), Key::Char('^')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMarkLine('^'), None));
        // `m^` isn't a thing -- aborts like any other invalid mark-set char
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('m')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('^')), KeyOutcome::None);
    }

    #[test]
    fn dot_mark_goto_works_but_cannot_be_set_by_name() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('`'), Key::Char('.')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMark('.'), None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('\''), Key::Char('.')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::GotoMarkLine('.'), None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('m')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('.')), KeyOutcome::None);
    }

    #[test]
    fn gi_resolves_to_last_insert_pos() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('i')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::EnterInsert(InsertCmd::LastInsertPos));
    }

    #[test]
    fn apply_insert_cmd_last_insert_pos_falls_back_to_the_cursor() {
        let text: Vec<char> = "hello".chars().collect();
        assert_eq!(apply_insert_cmd(&text, 3, InsertCmd::LastInsertPos), (text.clone(), 3));
    }

    #[test]
    fn bracket_leader_motions() {
        let cases: &[(&[Key], Motion)] = &[
            (&[Key::Char('['), Key::Char('(')], Motion::UnmatchedOpenParen),
            (&[Key::Char(']'), Key::Char(')')], Motion::UnmatchedCloseParen),
            (&[Key::Char('['), Key::Char('{')], Motion::UnmatchedOpenBrace),
            (&[Key::Char(']'), Key::Char('}')], Motion::UnmatchedCloseBrace),
            (&[Key::Char(']'), Key::Char(']')], Motion::SectionForward),
            (&[Key::Char(']'), Key::Char('[')], Motion::SectionForwardEnd),
            (&[Key::Char('['), Key::Char('[')], Motion::SectionBackward),
            (&[Key::Char('['), Key::Char(']')], Motion::SectionBackwardEnd),
        ];
        for (keys, motion) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(last(&mut vk, keys), KeyOutcome::Motion(motion.clone(), None), "{keys:?} should resolve to {motion:?}");
        }
    }

    #[test]
    fn bracket_leader_as_operator_target() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('['), Key::Char('(')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::UnmatchedOpenParen, None, None));
    }

    #[test]
    fn bracket_leader_invalid_continuation_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('[')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn z_prefix_scroll_motions() {
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('z')]),
            KeyOutcome::Motion(Motion::ScrollCenter, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('t')]),
            KeyOutcome::Motion(Motion::ScrollTop, None)
        );
        let mut vk = VimKeys::new();
        assert_eq!(
            last(&mut vk, &[Key::Char('z'), Key::Char('b')]),
            KeyOutcome::Motion(Motion::ScrollBottom, None)
        );
    }

    #[test]
    fn search_forward_and_repeat() {
        let mut vk = VimKeys::new();
        let keys = [
            Key::Char('/'),
            Key::Char('f'),
            Key::Char('o'),
            Key::Char('o'),
            Key::Enter,
        ];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None)
        );
        assert_eq!(
            vk.feed(Key::Char('n')),
            KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None)
        );
        assert_eq!(
            vk.feed(Key::Char('N')),
            KeyOutcome::Motion(Motion::SearchBackward("foo".to_string()), None)
        );
    }

    #[test]
    fn search_backward_and_backspace_editing() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('?')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('b')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('z')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Backspace), KeyOutcome::Pending); // "ba"
        assert_eq!(vk.feed(Key::Char('r')), KeyOutcome::Pending); // "bar"
        assert_eq!(
            vk.feed(Key::Enter),
            KeyOutcome::Motion(Motion::SearchBackward("bar".to_string()), None)
        );
    }

    #[test]
    fn search_escape_cancels() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('/')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Escape), KeyOutcome::None);
        // back to a clean state afterward
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn star_and_hash_word_search_with_repeat() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('*')), KeyOutcome::Motion(Motion::SearchWordForward, None));
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::Motion(Motion::SearchWordForward, None));
        assert_eq!(vk.feed(Key::Char('N')), KeyOutcome::Motion(Motion::SearchWordBackward, None));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('#')), KeyOutcome::Motion(Motion::SearchWordBackward, None));
    }

    #[test]
    fn g_star_and_g_hash_word_search_with_repeat() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('*')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::SearchWordForwardUnbounded, None));
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::Motion(Motion::SearchWordForwardUnbounded, None));
        assert_eq!(vk.feed(Key::Char('N')), KeyOutcome::Motion(Motion::SearchWordBackwardUnbounded, None));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('#')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::SearchWordBackwardUnbounded, None));
    }

    #[test]
    fn n_with_no_prior_search_is_a_no_op() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::None);
    }

    #[test]
    fn paragraph_and_sentence_and_line_motions() {
        let cases = [
            ('{', Motion::ParagraphBackward),
            ('}', Motion::ParagraphForward),
            ('(', Motion::SentenceBackward),
            (')', Motion::SentenceForward),
            ('+', Motion::NextLineNonBlank),
            ('-', Motion::PrevLineNonBlank),
            ('^', Motion::LineFirstNonBlank),
            ('|', Motion::GotoColumn),
            ('H', Motion::ScreenTop),
            ('M', Motion::ScreenMiddle),
            ('L', Motion::ScreenBottom),
            ('G', Motion::GotoLastLine),
        ];
        for (ch, motion) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(Key::Char(ch)), KeyOutcome::Motion(motion, None));
        }
    }

    #[test]
    fn enter_is_next_line_non_blank() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Enter), KeyOutcome::Motion(Motion::NextLineNonBlank, None));
    }

    #[test]
    fn count_survives_into_find_char_and_search() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('f'), Key::Char('x')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, Some(2))
        );
    }

    #[test]
    fn pending_display_shows_count_and_prefix_as_typed() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.pending_display(), "");
        vk.feed(Key::Char('2'));
        assert_eq!(vk.pending_display(), "2");
        vk.feed(Key::Char('0'));
        assert_eq!(vk.pending_display(), "20");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "20g");
        vk.feed(Key::Char('g'));
        // resolved into a motion -- nothing pending anymore
        assert_eq!(vk.pending_display(), "");
    }

    #[test]
    fn last_motion_display_flashes_the_completed_sequence() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.last_motion_display(), "");
        vk.feed(Key::Char('2'));
        vk.feed(Key::Char('0'));
        vk.feed(Key::Char('k'));
        assert_eq!(vk.last_motion_display(), "20k");
        // stays until the next sequence starts resolving
        vk.feed(Key::Char('j'));
        assert_eq!(vk.last_motion_display(), "j");
    }

    #[test]
    fn aborted_sequence_clears_pending_but_not_the_last_flash() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('h'));
        assert_eq!(vk.last_motion_display(), "h");
        vk.feed(Key::Char('g'));
        vk.feed(Key::Char('x')); // 'gx' isn't a thing -- aborts
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "h"); // unchanged by the abort
    }

    #[test]
    fn search_pending_display_shows_the_slash_and_typed_text() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('/'));
        assert_eq!(vk.pending_display(), "/");
        vk.feed(Key::Char('f'));
        vk.feed(Key::Char('o'));
        vk.feed(Key::Char('o'));
        assert_eq!(vk.pending_display(), "/foo");
        vk.feed(Key::Enter);
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "/foo<CR>");
    }

    #[test]
    fn window_leader_commands() {
        let cases = [
            ('n', WindowCmd::Next),
            ('p', WindowCmd::Previous),
            ('c', WindowCmd::New),
            ('q', WindowCmd::Close),
            ('s', WindowCmd::Split),
            ('v', WindowCmd::VSplit),
            ('h', WindowCmd::FocusLeft),
            ('j', WindowCmd::FocusDown),
            ('k', WindowCmd::FocusUp),
            ('l', WindowCmd::FocusRight),
            ('=', WindowCmd::Balance),
        ];
        for (ch, cmd) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
            assert_eq!(vk.feed(Key::Char(ch)), KeyOutcome::Window(cmd, None));
        }
    }

    #[test]
    fn window_leader_command_with_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::CtrlW, Key::Char('n')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::Next, Some(2)));
    }

    #[test]
    fn window_leader_unrecognized_continuation_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None);
        // aborted cleanly -- next key starts fresh
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn window_goto_first_and_last_bare() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('g'), Key::Char('g')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoFirstWindow, None));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('G')), KeyOutcome::Window(WindowCmd::GotoLastWindow, None));
    }

    #[test]
    fn window_goto_nth_tab() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('5'), Key::Char('g'), Key::Char('g')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoFirstWindow, Some(5)));

        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('5'), Key::Char('G')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoLastWindow, Some(5)));
    }

    #[test]
    fn window_goto_nth_tab_multi_digit_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::CtrlW, Key::Char('1'), Key::Char('2'), Key::Char('G')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Window(WindowCmd::GotoLastWindow, Some(12)));
    }

    #[test]
    fn window_g_unrecognized_continuation_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlW), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::None); // '<C-w>gx' isn't a thing
        assert_eq!(vk.feed(Key::Char('h')), KeyOutcome::Motion(Motion::Left, None));
    }

    #[test]
    fn window_pending_display_shows_the_leader_and_count() {
        let mut vk = VimKeys::new();
        vk.feed(Key::CtrlW);
        assert_eq!(vk.pending_display(), "^W");
        vk.feed(Key::Char('5'));
        assert_eq!(vk.pending_display(), "^W5");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "^W5g");
        vk.feed(Key::Char('g'));
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "^W5gg");
    }

    #[test]
    fn insert_entry_commands() {
        let cases = [
            ('i', InsertCmd::Before),
            ('a', InsertCmd::After),
            ('I', InsertCmd::LineStart),
            ('A', InsertCmd::LineEnd),
            ('s', InsertCmd::SubstituteChar),
            ('S', InsertCmd::SubstituteLine),
            ('C', InsertCmd::ChangeToEnd),
        ];
        for (ch, cmd) in cases {
            let mut vk = VimKeys::new();
            assert_eq!(vk.feed(Key::Char(ch)), KeyOutcome::EnterInsert(cmd));
        }
    }

    #[test]
    fn insert_entry_discards_a_leading_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::EnterInsert(InsertCmd::Before));
        // and doesn't leak into whatever comes next either
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn insert_entry_resets_pending_state_the_same_as_emit() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('2')); // count prefix
        assert_eq!(vk.feed(Key::Char('A')), KeyOutcome::EnterInsert(InsertCmd::LineEnd));
        assert_eq!(vk.pending_display(), "");
        assert_eq!(vk.last_motion_display(), "2A");
    }

    #[test]
    fn apply_insert_cmd_before_and_after_only_move_the_cursor() {
        let text: Vec<char> = "hello".chars().collect();
        assert_eq!(apply_insert_cmd(&text, 2, InsertCmd::Before), (text.clone(), 2));
        assert_eq!(apply_insert_cmd(&text, 2, InsertCmd::After), (text.clone(), 3));
        // After at the very end clamps rather than running past it
        assert_eq!(apply_insert_cmd(&text, 5, InsertCmd::After), (text.clone(), 5));
    }

    #[test]
    fn apply_insert_cmd_line_start_and_end() {
        let text: Vec<char> = "hello".chars().collect();
        assert_eq!(apply_insert_cmd(&text, 3, InsertCmd::LineStart), (text.clone(), 0));
        assert_eq!(apply_insert_cmd(&text, 1, InsertCmd::LineEnd), (text.clone(), 5));
    }

    #[test]
    fn apply_insert_cmd_substitute_char_removes_one_char_at_cursor() {
        let text: Vec<char> = "hello".chars().collect();
        let (result, cursor) = apply_insert_cmd(&text, 1, InsertCmd::SubstituteChar);
        assert_eq!(result.iter().collect::<String>(), "hllo");
        assert_eq!(cursor, 1);
        // at the end, nothing to remove -- cursor stays put, text unchanged
        let (result, cursor) = apply_insert_cmd(&text, 5, InsertCmd::SubstituteChar);
        assert_eq!(result.iter().collect::<String>(), "hello");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn apply_insert_cmd_substitute_line_clears_everything() {
        let text: Vec<char> = "hello".chars().collect();
        let (result, cursor) = apply_insert_cmd(&text, 3, InsertCmd::SubstituteLine);
        assert!(result.is_empty());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn apply_insert_cmd_change_to_end_truncates_from_cursor() {
        let text: Vec<char> = "hello".chars().collect();
        let (result, cursor) = apply_insert_cmd(&text, 2, InsertCmd::ChangeToEnd);
        assert_eq!(result.iter().collect::<String>(), "he");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn apply_insert_cmd_clamps_an_out_of_range_cursor() {
        let text: Vec<char> = "hi".chars().collect();
        // cursor well past the end of a short line shouldn't panic
        let (result, cursor) = apply_insert_cmd(&text, 99, InsertCmd::SubstituteChar);
        assert_eq!(result.iter().collect::<String>(), "hi");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn operator_plus_motion_resolves_with_no_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('w')),
            KeyOutcome::Operator(Op::Yank, Motion::WordForward, None, None)
        );
    }

    #[test]
    fn operator_and_motion_counts_multiply() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('y'), Key::Char('3'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(6), None)
        );
    }

    #[test]
    fn operator_with_only_a_pre_count_or_only_a_post_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('y'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(3), None)
        );

        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('3'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(3), None)
        );
    }

    #[test]
    fn operator_through_a_sub_prefix_motion() {
        // y then f then x -- the 'f' sub-prefix must stay armed as an
        // operator target, not get treated as its own bare motion.
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('f')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('x')),
            KeyOutcome::Operator(Op::Yank, Motion::FindChar { ch: 'x', till: false, forward: true }, None, None)
        );
    }

    #[test]
    fn yy_double_tap_resolves_to_operator_lines() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::OperatorLines(Op::Yank, None, None));
    }

    #[test]
    fn yy_and_y_capital_and_counted_variants_all_agree() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('Y')), KeyOutcome::OperatorLines(Op::Yank, None, None));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('y'), Key::Char('y')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Yank, Some(3), None));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('3'), Key::Char('y')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Yank, Some(3), None));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('Y')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Yank, Some(3), None));
    }

    #[test]
    fn operator_invalid_continuation_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        // 'q' isn't a motion (or a text-object prefix) -- cancels the
        // pending operator entirely.
        assert_eq!(vk.feed(Key::Char('q')), KeyOutcome::None);
        // and the next key starts completely fresh
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn operator_i_a_prefix_invalid_continuation_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        // 'i' while an operator is armed is a text-object prefix now, not
        // an immediate abort -- see the dedicated `text_object_*` tests
        // for the valid continuations. An unrecognized object character
        // still aborts the whole thing, same as any other invalid
        // operator continuation.
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('q')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn operator_on_a_non_motion_target_still_resolves_but_is_inert_downstream() {
        // Ctrl-D is a real, successfully-resolving Motion (HalfPageDown) as
        // far as vimkeys.rs is concerned -- vimkeys.rs has no dependency on
        // motion::motion_shape's classification, so it still wraps this
        // into an Operator. motion::motion_range is what actually rejects
        // Ctrl-D as an invalid operator target (see its own
        // motion_range_returns_none_for_non_motion_targets test), making
        // this behaviorally a no-op downstream regardless -- no register
        // write, no cursor move -- without vimkeys.rs needing to know
        // motion.rs's own classification rules.
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::CtrlD), KeyOutcome::Operator(Op::Yank, Motion::HalfPageDown, None, None));
    }

    #[test]
    fn register_prefix_threads_into_operator_and_operator_lines() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('a'), Key::Char('y'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Yank, Motion::WordForward, None, Some('a'))
        );

        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('A'), Key::Char('y'), Key::Char('y')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Yank, None, Some('A')));
    }

    #[test]
    fn register_prefix_on_a_bare_motion_is_silently_dropped() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('"')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
        // and it doesn't leak into a later put that never asked for one
        assert_eq!(
            vk.feed(Key::Char('p')),
            KeyOutcome::Put { before: false, count: None, register: None }
        );
    }

    #[test]
    fn register_prefix_accepts_plus_quote_and_underscore() {
        for c in ['+', '"', '_'] {
            let mut vk = VimKeys::new();
            let keys = [Key::Char('"'), Key::Char(c), Key::Char('p')];
            assert_eq!(last(&mut vk, &keys), KeyOutcome::Put { before: false, count: None, register: Some(c) });
        }
    }

    #[test]
    fn register_prefix_with_an_invalid_name_aborts() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('"')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('1')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn put_before_and_after_with_count_and_register() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('p')), KeyOutcome::Put { before: false, count: None, register: None });

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('P')), KeyOutcome::Put { before: true, count: None, register: None });

        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('"'), Key::Char('b'), Key::Char('p')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Put { before: false, count: Some(3), register: Some('b') });
    }

    #[test]
    fn apply_put_after_cursor_repeated_and_cursor_on_last_inserted_char() {
        let text: Vec<char> = "ac".chars().collect();
        let (result, cursor) = apply_put(&text, 0, "b", false, 2);
        assert_eq!(result.iter().collect::<String>(), "abbc");
        assert_eq!(cursor, 2); // last of the two inserted 'b's
    }

    #[test]
    fn apply_put_before_cursor() {
        let text: Vec<char> = "ac".chars().collect();
        let (result, cursor) = apply_put(&text, 1, "b", true, 1);
        assert_eq!(result.iter().collect::<String>(), "abc");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn apply_put_empty_text_or_zero_count_is_a_no_op() {
        let text: Vec<char> = "ac".chars().collect();
        assert_eq!(apply_put(&text, 0, "", false, 3), (text.clone(), 0));
        assert_eq!(apply_put(&text, 0, "xyz", false, 0), (text.clone(), 0));
    }

    #[test]
    fn apply_put_on_an_empty_buffer() {
        let text: Vec<char> = Vec::new();
        let (result, cursor) = apply_put(&text, 0, "hi", false, 1);
        assert_eq!(result.iter().collect::<String>(), "hi");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn last_search_text_empty_before_any_search() {
        let vk = VimKeys::new();
        assert_eq!(vk.last_search_text(), "");
        assert!(!vk.last_search_is_word());
    }

    #[test]
    fn last_search_text_after_a_pattern_search() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('/'), Key::Char('f'), Key::Char('o'), Key::Char('o'), Key::Enter];
        last(&mut vk, &keys);
        assert_eq!(vk.last_search_text(), "foo");
        assert!(!vk.last_search_is_word());
    }

    #[test]
    fn last_search_text_after_a_backward_pattern_search() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('?'), Key::Char('b'), Key::Char('a'), Key::Char('r'), Key::Enter];
        last(&mut vk, &keys);
        assert_eq!(vk.last_search_text(), "bar");
        assert!(!vk.last_search_is_word());
    }

    #[test]
    fn last_search_is_word_after_star_or_hash() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('*'));
        assert!(vk.last_search_is_word());

        let mut vk = VimKeys::new();
        vk.feed(Key::Char('#'));
        assert!(vk.last_search_is_word());
    }

    #[test]
    fn last_search_text_survives_n_and_capital_n_repeats() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('/'), Key::Char('x'), Key::Char('y'), Key::Enter];
        last(&mut vk, &keys);
        vk.feed(Key::Char('n'));
        assert_eq!(vk.last_search_text(), "xy");
        vk.feed(Key::Char('N'));
        assert_eq!(vk.last_search_text(), "xy");
    }

    #[test]
    fn last_search_text_stays_word_after_n_repeats_a_word_search() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('*'));
        vk.feed(Key::Char('n'));
        assert!(vk.last_search_is_word());
    }

    #[test]
    fn delete_operator_plus_motion() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('w')),
            KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, None)
        );
    }

    #[test]
    fn change_operator_plus_motion() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('c')), KeyOutcome::Pending);
        assert_eq!(
            vk.feed(Key::Char('e')),
            KeyOutcome::Operator(Op::Change, Motion::WordEnd, None, None)
        );
    }

    #[test]
    fn dd_and_cc_double_tap_resolve_to_operator_lines() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::OperatorLines(Op::Delete, None, None));

        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('c')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('c')), KeyOutcome::OperatorLines(Op::Change, None, None));
    }

    #[test]
    fn dd_and_cc_counts_multiply_like_yy() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('d'), Key::Char('3'), Key::Char('d')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Delete, Some(6), None));
    }

    #[test]
    fn capital_d_resolves_directly_to_delete_line_end() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('D')), KeyOutcome::Operator(Op::Delete, Motion::LineEnd, None, None));
    }

    #[test]
    fn capital_x_resolves_directly_to_delete_left() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('X')), KeyOutcome::Operator(Op::Delete, Motion::Left, None, None));
    }

    #[test]
    fn lowercase_x_resolves_to_delete_char_forward() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::DeleteCharForward { count: None, register: None });

        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('x')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::DeleteCharForward { count: Some(3), register: None });
    }

    #[test]
    fn register_prefix_threads_into_delete_change_and_delete_char_forward() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('a'), Key::Char('d'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, Some('a'))
        );

        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('b'), Key::Char('D')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::LineEnd, None, Some('b')));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('c'), Key::Char('x')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::DeleteCharForward { count: None, register: Some('c') });
    }

    #[test]
    fn delete_on_a_non_motion_target_still_resolves_but_is_inert_downstream() {
        // Same reasoning as the equivalent yank test: vimkeys.rs doesn't
        // consult motion::motion_shape, so this still becomes an
        // Operator -- motion::motion_range is what actually rejects it.
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::CtrlD), KeyOutcome::Operator(Op::Delete, Motion::HalfPageDown, None, None));
    }

    #[test]
    fn delete_invalid_continuation_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('q')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn text_object_only_recognized_while_an_operator_is_armed() {
        // Bare 'i'/'a' at a fresh dispatch point still mean insert-entry --
        // the text-object meaning only kicks in once an operator is armed.
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::EnterInsert(InsertCmd::Before));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::EnterInsert(InsertCmd::After));
    }

    #[test]
    fn text_object_also_recognized_in_visual_mode_with_no_operator() {
        let mut vk = VimKeys::new();
        vk.begin_visual(RegisterShape::Char, (0, 0));
        let keys = [Key::Char('i'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::TextObject(TextObjectKind::Word, false), None));
    }

    #[test]
    fn diw_and_daw_resolve_to_word_text_objects() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('i'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Delete, Motion::TextObject(TextObjectKind::Word, false), None, None)
        );
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('a'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Delete, Motion::TextObject(TextObjectKind::Word, true), None, None)
        );
    }

    #[test]
    fn text_object_count_combines_on_either_side() {
        // count before the operator...
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('c'), Key::Char('i'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Change, Motion::TextObject(TextObjectKind::Word, false), Some(2), None)
        );
        // ...and between the operator and the object prefix -- same result.
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('2'), Key::Char('i'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Change, Motion::TextObject(TextObjectKind::Word, false), Some(2), None)
        );
    }

    #[test]
    fn text_object_register_prefix_carries_through() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('a'), Key::Char('y'), Key::Char('i'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Yank, Motion::TextObject(TextObjectKind::Word, false), None, Some('a'))
        );
    }

    #[test]
    fn text_object_kind_aliases_and_full_set() {
        let cases: &[(char, TextObjectKind)] = &[
            ('w', TextObjectKind::Word),
            ('W', TextObjectKind::WordBig),
            ('s', TextObjectKind::Sentence),
            ('p', TextObjectKind::Paragraph),
            ('(', TextObjectKind::Paren),
            (')', TextObjectKind::Paren),
            ('b', TextObjectKind::Paren),
            ('{', TextObjectKind::Brace),
            ('}', TextObjectKind::Brace),
            ('B', TextObjectKind::Brace),
            ('[', TextObjectKind::Bracket),
            (']', TextObjectKind::Bracket),
            ('<', TextObjectKind::Angle),
            ('>', TextObjectKind::Angle),
            ('"', TextObjectKind::DoubleQuote),
            ('\'', TextObjectKind::SingleQuote),
            ('`', TextObjectKind::Backtick),
        ];
        for (ch, kind) in cases {
            let mut vk = VimKeys::new();
            let keys = [Key::Char('d'), Key::Char('i'), Key::Char(*ch)];
            assert_eq!(
                last(&mut vk, &keys),
                KeyOutcome::Operator(Op::Delete, Motion::TextObject(*kind, false), None, None),
                "di{ch} should resolve to {kind:?}"
            );
        }
    }

    #[test]
    fn text_object_invalid_object_char_aborts_operator() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('9')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn apply_delete_forward_basic_and_counted() {
        let text: Vec<char> = "abcdef".chars().collect();
        let (result, cursor, deleted) = apply_delete_forward(&text, 1, 1);
        assert_eq!(result.iter().collect::<String>(), "acdef");
        assert_eq!(cursor, 1);
        assert_eq!(deleted, "b");

        let (result, cursor, deleted) = apply_delete_forward(&text, 0, 3);
        assert_eq!(result.iter().collect::<String>(), "def");
        assert_eq!(cursor, 0);
        assert_eq!(deleted, "abc");
    }

    #[test]
    fn apply_delete_forward_deletes_the_last_character_unlike_a_clamped_motion() {
        let text: Vec<char> = "abc".chars().collect();
        let (result, cursor, deleted) = apply_delete_forward(&text, 2, 1);
        assert_eq!(result.iter().collect::<String>(), "ab");
        assert_eq!(cursor, 1);
        assert_eq!(deleted, "c");
    }

    #[test]
    fn apply_delete_forward_count_clamped_to_end_of_buffer() {
        let text: Vec<char> = "abc".chars().collect();
        let (result, cursor, deleted) = apply_delete_forward(&text, 1, 10);
        assert_eq!(result.iter().collect::<String>(), "a");
        assert_eq!(cursor, 0);
        assert_eq!(deleted, "bc");
    }

    #[test]
    fn apply_delete_forward_on_empty_buffer_is_a_no_op() {
        let text: Vec<char> = Vec::new();
        let (result, cursor, deleted) = apply_delete_forward(&text, 0, 1);
        assert!(result.is_empty());
        assert_eq!(cursor, 0);
        assert_eq!(deleted, "");
    }

    #[test]
    fn apply_delete_forward_zero_count_is_a_no_op() {
        let text: Vec<char> = "abc".chars().collect();
        let (result, cursor, deleted) = apply_delete_forward(&text, 1, 0);
        assert_eq!(result.iter().collect::<String>(), "abc");
        assert_eq!(cursor, 1);
        assert_eq!(deleted, "");
    }

    #[test]
    fn v_and_shift_v_enter_visual_mode_charwise_and_linewise() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('v')), KeyOutcome::EnterVisual(RegisterShape::Char));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('V')), KeyOutcome::EnterVisual(RegisterShape::Line));
    }

    #[test]
    fn v_again_while_already_visual_is_ignored() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('v')), KeyOutcome::EnterVisual(RegisterShape::Char));
        vk.begin_visual(RegisterShape::Char, (2, 3));
        // A caller's own begin_visual call is what actually arms
        // `self.visual` -- feed() alone never does (see EnterVisual's own
        // doc comment) -- so this exercises the real "already in Visual,
        // v/V pressed again" case a live caller would hit.
        assert_eq!(vk.feed(Key::Char('v')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('V')), KeyOutcome::None);
        // Still in Visual mode -- an ignored `v`/`V` doesn't cancel it.
        assert!(vk.is_visual());
        assert_eq!(vk.visual_anchor(), Some((RegisterShape::Char, (2, 3))));
    }

    #[test]
    fn visual_accessors_round_trip() {
        let mut vk = VimKeys::new();
        assert!(!vk.is_visual());
        assert_eq!(vk.visual_anchor(), None);
        vk.begin_visual(RegisterShape::Line, (5, 0));
        assert!(vk.is_visual());
        assert_eq!(vk.visual_anchor(), Some((RegisterShape::Line, (5, 0))));
        assert_eq!(vk.last_visual(), None);
        vk.end_visual((7, 2));
        assert!(!vk.is_visual());
        assert_eq!(vk.visual_anchor(), None);
        assert_eq!(vk.last_visual(), Some((RegisterShape::Line, (5, 0), (7, 2))));
    }

    #[test]
    fn gv_reselects_the_last_visual_selection() {
        let mut vk = VimKeys::new();
        // Before any Visual selection has ever ended, gv is a no-op.
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('v')), KeyOutcome::ReselectVisual);
        assert_eq!(vk.last_visual(), None);

        vk.begin_visual(RegisterShape::Char, (1, 2));
        vk.end_visual((3, 4));
        let keys = [Key::Char('g'), Key::Char('v')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::ReselectVisual);
        assert_eq!(vk.last_visual(), Some((RegisterShape::Char, (1, 2), (3, 4))));
    }

    #[test]
    fn take_pending_register_consumes_a_quote_prefix() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('"')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::Pending);
        assert_eq!(vk.take_pending_register(), Some('a'));
        // Consumed -- a second take sees nothing left.
        assert_eq!(vk.take_pending_register(), None);
    }

    #[test]
    fn is_idle_is_false_mid_count_prefix_or_armed_operator() {
        let mut vk = VimKeys::new();
        assert!(vk.is_idle());
        vk.feed(Key::Char('3'));
        assert!(!vk.is_idle(), "mid a count");
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('f'));
        assert!(!vk.is_idle(), "mid a find-char sub-prefix");
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('d'));
        assert!(!vk.is_idle(), "an operator is armed, awaiting its motion");
    }

    #[test]
    fn r_resolves_to_replace_char_with_the_typed_character_and_count() {
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('r'), Key::Char('x')]), KeyOutcome::ReplaceChar { ch: 'x', count: None });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('r'), Key::Char('x')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::ReplaceChar { ch: 'x', count: Some(3) });
    }

    #[test]
    fn r_accepts_any_character_including_punctuation() {
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('r'), Key::Char(' ')]), KeyOutcome::ReplaceChar { ch: ' ', count: None });
    }

    #[test]
    fn r_with_a_non_char_key_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('r')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Left), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn capital_r_resolves_to_enter_replace() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('R')), KeyOutcome::EnterReplace);
    }

    #[test]
    fn tilde_resolves_to_toggle_case_with_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('~')), KeyOutcome::ToggleCase { count: None });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('4'), Key::Char('~')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::ToggleCase { count: Some(4) });
    }

    #[test]
    fn gu_gu_gtilde_arm_case_operators_resolved_by_a_motion() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('u'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Lowercase, Motion::WordForward, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('U'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Uppercase, Motion::WordForward, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('~'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::CaseToggle, Motion::WordForward, None, None));
    }

    #[test]
    fn guu_guu_capital_and_gtilde_tilde_double_tap_resolve_to_operator_lines() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('u'), Key::Char('u')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Lowercase, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('U'), Key::Char('U')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Uppercase, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('~'), Key::Char('~')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::CaseToggle, None, None));
    }

    #[test]
    fn gu_text_object_and_counts_combine_like_any_other_operator() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('g'), Key::Char('U'), Key::Char('i'), Key::Char('w')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::Operator(Op::Uppercase, Motion::TextObject(TextObjectKind::Word, false), Some(2), None)
        );
    }

    #[test]
    fn gu_invalid_continuation_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('u'), Key::Escape];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn ctrl_a_and_ctrl_x_resolve_to_signed_adjust_number() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlA), KeyOutcome::AdjustNumber { delta: 1 });
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlX), KeyOutcome::AdjustNumber { delta: -1 });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('5'), Key::CtrlA];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AdjustNumber { delta: 5 });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('5'), Key::CtrlX];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AdjustNumber { delta: -5 });
    }

    #[test]
    fn ys_motion_resolves_to_add_surround_with_the_motion_target() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('s'), Key::Char('i'), Key::Char('w'), Key::Char('(')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::AddSurround { target: SurroundTarget::Motion(Motion::TextObject(TextObjectKind::Word, false), None), ch: '(' }
        );
    }

    #[test]
    fn ys_with_a_simple_motion_and_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('s'), Key::Char('3'), Key::Char('w'), Key::Char('"')];
        assert_eq!(
            last(&mut vk, &keys),
            KeyOutcome::AddSurround { target: SurroundTarget::Motion(Motion::WordForward, Some(3)), ch: '"' }
        );
    }

    #[test]
    fn yss_resolves_to_add_surround_with_the_line_target() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('s'), Key::Char('s'), Key::Char(')')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AddSurround { target: SurroundTarget::Line(None), ch: ')' });
    }

    #[test]
    fn yss_carries_a_leading_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('y'), Key::Char('s'), Key::Char('s'), Key::Char('}')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AddSurround { target: SurroundTarget::Line(Some(2)), ch: '}' });
    }

    #[test]
    fn ys_invalid_delimiter_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('s'), Key::Char('i'), Key::Char('w'), Key::Char(' ')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        // The aborted sequence must not leak into the next command.
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn ys_invalid_motion_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        // 'q' isn't a motion (or a text-object prefix) -- same invalid
        // continuation `operator_invalid_continuation_aborts_and_does_
        // not_leak` already exercises for a plain operator.
        let keys = [Key::Char('y'), Key::Char('s'), Key::Char('q')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn plain_yy_and_y_motion_are_unaffected_by_surround_support() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('y')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Yank, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::WordForward, None, None));
    }

    #[test]
    fn plain_s_and_capital_s_are_unaffected_outside_an_armed_yank() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('s')), KeyOutcome::EnterInsert(InsertCmd::SubstituteChar));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('S')), KeyOutcome::EnterInsert(InsertCmd::SubstituteLine));
    }

    #[test]
    fn ds_resolves_to_delete_surround() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('s'), Key::Char('(')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::DeleteSurround { ch: '(' });
    }

    #[test]
    fn ds_invalid_target_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('s'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn plain_dd_and_d_motion_are_unaffected_by_surround_support() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('d')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Delete, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, None));
    }

    #[test]
    fn cs_resolves_to_change_surround() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('s'), Key::Char('"'), Key::Char('\'')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::ChangeSurround { ch: '"', replacement: '\'' });
    }

    #[test]
    fn cs_invalid_target_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('s'), Key::Char('x')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn cs_invalid_replacement_aborts_and_does_not_leak() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('s'), Key::Char('('), Key::Char(' ')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn plain_cc_and_c_motion_are_unaffected_by_surround_support() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('c')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Change, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Change, Motion::WordForward, None, None));
    }

    #[test]
    fn ys_register_prefix_is_dropped_not_carried_through() {
        // Unlike y{motion}, ys/yss never write a register -- a `"x`
        // prefix in front of one is simply irrelevant, matching vim's own
        // "register prefix in front of something that doesn't use one is
        // silently ignored" rule.
        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('a'), Key::Char('y'), Key::Char('s'), Key::Char('s'), Key::Char(')')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AddSurround { target: SurroundTarget::Line(None), ch: ')' });
        assert_eq!(vk.take_pending_register(), None);
    }
}
