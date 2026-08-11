// Translates a sequence of editor::Key events into bishedit::motion::Motion
// values. This layer knows vim's specific bindings and owns the state a
// single keypress can't (an accumulating count, a pending g/f/F/t/T/m/`/'
// prefix awaiting one more character, an in-progress search string) -- but
// it never touches a Buffer or a terminal. Motions themselves are applied
// by the caller via bishedit::motion::apply_motion.

use crate::editor::Key;
use super::motion::Motion;

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
    /// The key was consumed as part of an in-progress sequence (a count
    /// digit, or a prefix awaiting its next character); no motion yet.
    Pending,
    /// The key isn't part of any recognized motion sequence. Any
    /// in-progress count/prefix is discarded, matching vim's behavior of
    /// dropping a pending command on an invalid continuation.
    None,
}

/// An operator awaiting a motion (`y{motion}`) or its own double-tap
/// shorthand (`yy`). Single variant today, deliberately -- `Op::Delete`/
/// `Op::Change` are meant to extend this later by reusing the exact same
/// operator-pending plumbing in `VimKeys`, not by inventing a parallel one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Yank,
}

impl Op {
    /// The key that both arms this operator and, pressed again at a fresh
    /// dispatch point, triggers its whole-line shorthand (`yy`).
    fn trigger_char(self) -> char {
        match self {
            Op::Yank => 'y',
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
        InsertCmd::Before => (text.to_vec(), cursor),
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
    Word { forward: bool },
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
            pending_register: None,
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
                self.last_search = Some(LastSearch::Word { forward: true });
                self.emit(Motion::SearchWordForward)
            }
            Key::Char('#') => {
                self.last_search = Some(LastSearch::Word { forward: false });
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
            Key::Char('i') => self.emit_insert(InsertCmd::Before),
            Key::Char('a') => self.emit_insert(InsertCmd::After),
            Key::Char('I') => self.emit_insert(InsertCmd::LineStart),
            Key::Char('A') => self.emit_insert(InsertCmd::LineEnd),
            Key::Char('s') => self.emit_insert(InsertCmd::SubstituteChar),
            Key::Char('S') => self.emit_insert(InsertCmd::SubstituteLine),
            Key::Char('C') => self.emit_insert(InsertCmd::ChangeToEnd),
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
            Key::Char('p') => self.emit_put(false),
            Key::Char('P') => self.emit_put(true),
            Key::Char('"') => {
                self.pending = Pending::Register;
                KeyOutcome::Pending
            }
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
        match key {
            Key::Char(c) if c.is_ascii_lowercase() => self.emit(match kind {
                MarkKind::Set => Motion::SetMark(c),
                MarkKind::GotoExact => Motion::GotoMark(c),
                MarkKind::GotoLine => Motion::GotoMarkLine(c),
            }),
            _ => self.abort(),
        }
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
            Some(LastSearch::Word { forward }) => {
                let forward = if same_direction { forward } else { !forward };
                self.emit(if forward {
                    Motion::SearchWordForward
                } else {
                    Motion::SearchWordBackward
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
        // 'i' isn't a motion -- cancels the pending operator entirely,
        // consumed rather than also entering insert mode.
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::None);
        // and the next key starts completely fresh
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
}
