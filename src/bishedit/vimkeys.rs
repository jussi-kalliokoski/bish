// Turns `editor::Key` events into `motion::Motion`s and other outcomes.
// This layer knows vim's bindings and holds the state one key can't: a
// count, a pending prefix (`g`, `f`, `m`, ...) awaiting another key, a
// search being typed, macro recording and replay (see `next_key`). It
// never touches a Buffer, and reads the terminal only through the
// caller's `next_key` callback; callers apply motions with
// `motion::apply_motion`.

use std::collections::{HashMap, VecDeque};
use std::io;

use super::motion::{Motion, TextObjectKind};
use super::registers::RegisterShape;
use crate::editor::Key;

/// Which "where is this" question `KeyOutcome::GotoDefinition` asks: four
/// language-server requests that differ only in which is sent, each
/// followed by the same jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoKind {
    /// `gd`.
    Definition,
    /// `gy`: the definition of this thing's *type*, not of the thing.
    TypeDefinition,
    /// The implementations of an interface, trait or abstract method. No key
    /// reaches it yet; see `feed_g` on `gi`.
    Implementation,
    /// `gD`: where it was declared, which some languages separate from where
    /// it's defined.
    Declaration,
}

impl GotoKind {
    /// The request, and the capability saying whether the server answers it,
    /// checked first so an unsupported request costs nothing.
    pub fn request(self) -> (&'static str, &'static str) {
        match self {
            GotoKind::Definition => ("textDocument/definition", "definitionProvider"),
            GotoKind::TypeDefinition => ("textDocument/typeDefinition", "typeDefinitionProvider"),
            GotoKind::Implementation => ("textDocument/implementation", "implementationProvider"),
            GotoKind::Declaration => ("textDocument/declaration", "declarationProvider"),
        }
    }

    /// What to call it when there's none: "no type definition found".
    pub fn noun(self) -> &'static str {
        match self {
            GotoKind::Definition => "definition",
            GotoKind::TypeDefinition => "type definition",
            GotoKind::Implementation => "implementation",
            GotoKind::Declaration => "declaration",
        }
    }
}

/// Which `z` fold command, for `KeyOutcome::Fold`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldCmd {
    /// `zo`: open what hides the cursor's line, one level per count.
    Open,
    /// `zO`: every fold around the cursor's line, open.
    OpenAll,
    /// `zc`: close one level more around the cursor's line.
    Close,
    /// `zC`: every fold around the cursor's line, closed.
    CloseAll,
    /// `za`
    Toggle,
    /// `zA`
    ToggleAll,
    /// `zv`: open just enough to show the cursor's line.
    View,
    /// `zR`
    OpenEverything,
    /// `zM`
    CloseEverything,
    /// `zd`
    Delete,
    /// `zD`
    DeleteAll,
    /// `zE`
    Eliminate,
    /// `zi`: folding on or off, every fold kept either way.
    ToggleEnabled,
    /// `zF`: a fold over `count` lines from the cursor's.
    CreateLines,
    /// `zf` in Visual mode: a fold over the selection.
    CreateFromSelection,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyOutcome {
    /// A motion to apply, with the count typed before it, if any.
    Motion(Motion, Option<usize>),
    /// A Ctrl-W window command, applied by the caller (repl.rs's
    /// `apply_window_action`), not to a `Buffer`. The count usually
    /// repeats, typed before `<C-w>`: `2<C-w>n` is next window twice.
    /// `GotoFirstWindow`/`GotoLastWindow` are like `GotoFirstLine`: the
    /// count comes inside the sequence as an absolute tab position
    /// (`<C-w>5gg`), and without one they go to the first or last tab.
    Window(WindowCmd, Option<usize>),
    /// `<C-t>`: the fuzzy opener (repl.rs's `run_opener`) over open panes and
    /// git-tracked files. Window state like `Window`, but a way to get
    /// somewhere rather than window management, hence its own key, which vim
    /// uses for a tag stack bish doesn't have. Takes no count.
    Opener,
    /// `i`, `a`, `I`, `A`, `s`, `S`, `C`: leave navigation and edit text,
    /// with `apply_insert_cmd` deciding where and what changes first.
    /// Applied by the caller to whatever it treats as the buffer. A count
    /// before `i`/`a`/`I`/ `A` repeats the typed text, which `VimKeys` does
    /// itself as the insert ends (see `capture_insert_key`); before
    /// `s`/`S`/`C` it's dropped.
    EnterInsert(InsertCmd),
    /// `y{motion}` and the other operators on a motion's range. `register` is
    /// the `"x` prefix, if any (`None` is the unnamed register).
    Operator(Op, Motion, Option<usize>, Option<char>),
    /// `yy`, `Y`, `dd`: an operator on this line and `count - 1` more,
    /// linewise. Not an `Operator` with a made-up motion: vim defines the
    /// doubled form as "this operator, this line" (see
    /// `motion::whole_lines`).
    OperatorLines(Op, Option<usize>, Option<char>),
    /// `p`/`P`: put the register's contents after or before the cursor,
    /// `count` times.
    Put { before: bool, count: Option<usize>, register: Option<char> },
    /// `x`: delete `count` characters forward. Not `d` with `Motion::Right`,
    /// which won't move past the last character, so it would do nothing where
    /// `x` still deletes. `X` has no such edge, so it is a plain `Operator`.
    DeleteCharForward { count: Option<usize>, register: Option<char> },
    /// `J`/`gJ`: join `count` lines (at least 2). `with_space` is `J`'s
    /// whitespace-aware join; `gJ` concatenates.
    Join { count: Option<usize>, with_space: bool },
    /// `v`/`V`: enter Visual mode, charwise or linewise. Uses `RegisterShape`
    /// because the selection's shape becomes the yank's. The caller must call
    /// `begin_visual` with the cursor as anchor, since this layer can't read
    /// the buffer.
    EnterVisual(RegisterShape),
    /// `gv`: reselect the last Visual selection, if any (see
    /// `last_visual`). The caller sets the cursor, then calls
    /// `begin_visual` with the shape and anchor.
    ReselectVisual,
    /// Visual `o`: the cursor goes to the other end and the anchor to where
    /// the cursor was. The anchor lives here and the cursor in the buffer, so
    /// the caller reads `visual_anchor()`, moves the cursor there, and calls
    /// `set_visual_anchor` with where it was.
    SwapVisualEnds,
    /// `gd`: go to where the thing under the cursor is defined, as the
    /// language server knows it. vim's `gd` searches back for a local
    /// declaration; a server answers the same question better. A frontend
    /// with no server just doesn't handle it.
    GotoDefinition(GotoKind),
    /// `gr`: everywhere the thing under the cursor is used, listed in the
    /// location-list pane, since there are many answers.
    GotoReferences,
    /// `gO`: an outline of what this file declares, nested. vim's `gO`
    /// already means an outline in help and man pages.
    DocumentSymbols,
    /// `ga`: what the language server offers to do here: a quick fix, a
    /// refactor, an import. vim's `ga` (a character's code) isn't
    /// implemented, and `gr` can't take a third key.
    CodeActions,
    /// `zo`, `zc`, `za`, `zR`, `zM`, `zd`, `zE`, `zi` and the rest, applied
    /// by the buffer's owner. Not `zf{motion}`, which is
    /// `Operator(Op::Fold, ..)`, nor `zj`, `zk`, `[z`, `]z`, which are
    /// motions.
    Fold(FoldCmd, Option<usize>),
    /// `Ctrl-O`/`Ctrl-I`: step through the jump list. The caller calls
    /// `jump_back`/`jump_forward` with the cursor and moves there on `Some`.
    Jump { forward: bool },
    /// `u`: undo `count` changes, default 1, in the caller's undo tree (see
    /// `TextBuffer::undo`).
    Undo(Option<usize>),
    /// `Ctrl-R`: redo `count` changes, default 1.
    Redo(Option<usize>),
    /// `g-`/`g+`: step `count` positions through the undo tree's creation
    /// history, reaching branches `u` and `Ctrl-R` can't (see
    /// `UndoTree::time_travel_back`).
    UndoSeq { forward: bool, count: Option<usize> },
    /// `ys{motion}{ch}`, `yss{ch}` and Visual `S{ch}`: vim-surround's wrap.
    /// `target` is a motion's range or, for `yss`, the line from its first
    /// non-blank to its end. `ch` is the key pressed: `(` and `)` name one
    /// pair but pad differently (`motion::surround_delims`). Visual `S` is
    /// handled by the caller. Not an `Operator`: it needs the delimiter key
    /// too, so it appears only once that's read (see
    /// `Pending::SurroundChar`).
    AddSurround { target: SurroundTarget, ch: char },
    /// `ds{ch}`: removes the nearest enclosing pair `ch` names (see
    /// `motion::surround_target_kind`), with one padding space inside a
    /// bracket pair (`motion::surround_delete_spans`). A no-op with no such
    /// pair.
    DeleteSurround { ch: char },
    /// `cs{ch}{replacement}`: swaps the pair's delimiters for
    /// `replacement`'s, leaving padding alone.
    ChangeSurround { ch: char, replacement: char },
    /// `r{ch}`: replace `count` characters with `ch`, staying in Normal
    /// mode. The caller does nothing if fewer than `count` remain on the
    /// line, as in vim.
    ReplaceChar { ch: char, count: Option<usize> },
    /// `R`: Replace mode, where typing overwrites (extending the line past
    /// its end) instead of inserting. Not an `EnterInsert`: there's no
    /// repositioning, and the typing itself behaves differently for the whole
    /// mode, which `InsertCmd` can't express.
    EnterReplace,
    /// `~`: toggle the case of `count` characters (default 1) and move past
    /// them, clamped to the line's last character, as in vim.
    ToggleCase { count: Option<usize> },
    /// `Ctrl-A`/`Ctrl-X`: add `delta` (signed, magnitude `count`) to the
    /// number at or after the cursor on this line (see
    /// `motion::find_number`). A no-op without one.
    AdjustNumber { delta: i64 },
    /// `o`/`O`: open a blank line below or above and insert there. Not an
    /// `EnterInsert`, which only positions on an existing line: this
    /// creates a line, which only a multi-line buffer can (a no-op on the
    /// prompt, like `Join`). A count is dropped.
    OpenLine { above: bool },
    /// The key was part of a sequence still in progress (a count digit, a
    /// prefix); no outcome yet.
    Pending,
    /// The key isn't part of any binding. A pending count or prefix is
    /// dropped, as vim drops a command on an invalid continuation.
    None,
}

/// The default width of one indent, before the `shiftwidth`/`tabstop`
/// bishopts or an `.editorconfig` say otherwise (see
/// `TextBuffer::shiftwidth`). The prompt's `LineBuffer` uses it as is: a
/// command line has no file for an `.editorconfig` to be about.
// The *default* width of one indent -- what a buffer starts with before
// the `shiftwidth`/`tabstop` bishopts or a project's `.editorconfig`
// have said anything (see `TextBuffer::shiftwidth`, and repl.rs's own
// apply_shell_options/apply_editorconfig).
//
// A real `TextBuffer` reads its own width from itself now, so this is no
// longer the rule everything obeys -- but it is still the one place the
// number is written down, and `editor.rs`'s own single-line `LineBuffer`
// still uses it directly: a shell prompt is not a file, has no path for
// a `.editorconfig` to be about, and indenting one is a rare enough
// gesture that giving it its own configurable width would be inventing
// a setting nobody asked for.
pub const INDENT_WIDTH: usize = 4;

/// An operator awaiting a motion (`y{motion}`, `d{motion}`, `c{motion}`)
/// or its doubled form (`yy`, `dd`, `cc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Yank,
    Delete,
    /// `c{motion}`/`cc`: like `Delete`, then insert at the deletion point,
    /// unless the motion failed.
    Change,
    /// `gu{motion}`/`guu`: lowercase the target. Reached through `feed_g`.
    /// Writes no register; callers ignore the register given with these.
    Lowercase,
    /// `gU{motion}`/`gUU` -- uppercases every character in the target.
    Uppercase,
    /// `g~{motion}`/`g~~`: toggle the case of the target.
    CaseToggle,
    /// `>{motion}`/`>>`: shift the lines the target touches right by one
    /// shiftwidth. Always linewise, as in vim, whatever the motion's shape.
    Indent,
    /// `<{motion}`/`<<`: shift left by up to one shiftwidth.
    Outdent,
    /// `zf{motion}`: a closed fold over the lines the target touches. Changes
    /// no text, so no register and no undo step.
    Fold,
}

/// `AddSurround`'s target: a motion's range (`ys{motion}`, with its count)
/// or `count` lines from the cursor's (`yss`), like `OperatorLines`.
#[derive(Debug, Clone, PartialEq)]
pub enum SurroundTarget {
    Motion(Motion, Option<usize>),
    Line(Option<usize>),
}

impl Op {
    /// The key that arms this operator and, pressed again, doubles it (`yy`).
    fn trigger_char(self) -> char {
        match self {
            Op::Yank => 'y',
            Op::Delete => 'd',
            Op::Change => 'c',
            // The key after the `g`, as vim's `guu` repeats it, not `g`
            // itself.
            Op::Lowercase => 'u',
            Op::Uppercase => 'U',
            Op::CaseToggle => '~',
            Op::Indent => '>',
            Op::Outdent => '<',
            // `zf` has no doubled form (`zfj` folds two lines; `zF` takes a
            // count).
            Op::Fold => '\0',
        }
    }
}

// The count before the operator and the one before the motion multiply,
// as in vim (`2y3w` yanks 6 words). `None` means 1.
fn combine_counts(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some(x),
        (Some(x), Some(y)) => Some(x.saturating_mul(y)),
    }
}

/// `i`, `a`, `I`, `A`, `s`, `S`, `C` (see `KeyOutcome::EnterInsert`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertCmd {
    /// `i`: insert before the cursor.
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
    /// `gi`: insert where Insert mode last ended (vim's `^` mark). Needs
    /// the buffer's marks, which `apply_insert_cmd` doesn't have, so there
    /// it acts like `Before`; fileeditor.rs's `resolve_insert_start`
    /// resolves it with `Buffer::get_mark('^')`.
    LastInsertPos,
}

/// What one `InsertCmd` does to a `[char]` line and cursor, shared by
/// every frontend that enters insert mode on one line: the prompt's Ctrl-E
/// mode on the live cursor, and repl.rs's Ctrl+G mode on a cursor the
/// excursion didn't move.
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

/// `p`/`P`: splices `insert_text` at the cursor (`P`) or one past it
/// (`p`), `count` copies back to back, as vim's `3p` does. The cursor ends
/// on the last inserted character. `insert_text` must already be one line
/// (see `RegisterValue::flatten_to_single_line`).
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

/// `x`: deletes up to `count` characters from the cursor, clamped to the
/// line. Not `d{count}l`: `l` won't move past the last character, so `dl`
/// there does nothing, while `x` deletes it. Returns the deleted text for
/// the register; empty means there was nothing to delete.
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

/// `<C-w>{cmd}`: the single-letter shortcuts of the `window` command (see
/// exec.rs's `run_window`), without the size commands or `fg <id>`, which
/// need more than a letter and a count.
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
    /// `<C-w>z` and `<C-w>o`: the focused pane fills the window until the
    /// next press restores the split.
    Zoom,
    /// `<C-w><Space>`: the next named layout.
    NextLayout,
    /// `<C-w>x`: exchange this pane with the next, as in vim.
    SwapPane,
    /// `<C-w>T`: move this pane to a tab of its own, as vim does with a
    /// window.
    BreakPane,
    Balance,
    /// `<C-w>_`: shrink this pane to its divider. Focus moves to a neighbour;
    /// focusing it again restores it.
    Minimize,
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
    // `i`/`a` with an operator armed, awaiting the object character (`w`,
    // `(`, `"`). Only entered with an operator armed (see `feed_fresh`).
    TextObject { around: bool },
    // `[` or `]`, awaiting the second character (`[(`, `])`, `[{`, `]}`,
    // `[[`, `]]`, `[]`, `][`).
    BracketOpen,
    BracketClose,
    // `ys{motion}`/`yss`, awaiting the delimiter once the target resolved.
    // Set by `feed`'s `surround_armed` handling, then resolved by
    // `feed_inner` like every other `Pending`.
    SurroundChar { target: SurroundTarget },
    // `ds`, awaiting its target character.
    DeleteSurroundTarget,
    // `cs` -- awaiting its target character.
    ChangeSurroundTarget,
    // `cs{ch}` -- awaiting the replacement character.
    ChangeSurroundChar { ch: char },
    // `r`, awaiting the replacement character.
    ReplaceChar,
    Z,
    Window,
    // `<C-w>g`, awaiting the second `g` of `<C-w>gg`.
    WindowG,
    Search { forward: bool, text: String },
    // `"`, awaiting a register name. Doesn't `emit()`: it stashes
    // `pending_register` and returns to `Pending::None` for the operator or
    // put that follows (see `feed_register`).
    Register,
}

#[derive(Debug, Clone, Copy)]
enum LastSearch {
    Pattern { forward: bool },
    // `*`/`#` search on word boundaries, `g*`/`g#` don't; `n`/`N` repeat the
    // kind used.
    Word { forward: bool, bounded: bool },
}

// A short label for `current_input`'s transcript, or `None` for keys
// without one (Delete, Alt+arrows), which are left out.
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

// A Visual selection's (shape, anchor, cursor): what `gv` remembers.
type VisualSelection = (RegisterShape, (usize, usize), (usize, usize));

pub struct VimKeys {
    count: Option<usize>,
    pending: Pending,
    last_find: Option<(char, bool, bool)>, // (ch, till, forward)
    last_search_text: String,
    // `:noh`. As in vim, the highlight stays off until the next search
    // command (`n`, `N` and `*` count), so it's cleared wherever
    // `last_search` is set.
    search_highlight_off: bool,
    last_search: Option<LastSearch>,
    // The operator awaiting a motion, and the count typed before it, kept
    // apart from `count` so a second count can accumulate for the motion:
    // in `2y3w`, `operator_count` is 2 when `y` arms and `count` collects 3
    // (see `feed`).
    active_operator: Option<Op>,
    operator_count: Option<usize>,
    // `ys{motion}`/`yss`: set when `s` comes where a motion or the doubled
    // `y` would, leaving `active_operator` as `Op::Yank` so a second `y`
    // still makes `yy`. Routes what follows (a motion, a text object, or
    // `yss`'s second `s`) into `Pending::SurroundChar` instead of an
    // `Operator` (see `KeyOutcome::AddSurround`).
    surround_armed: bool,
    // `"x`: the register for the next operator or put. Survives
    // `Pending::Register` returning to `Pending::None`, since it modifies
    // what follows, and is dropped by any other resolved outcome, as vim
    // ignores a register before a bare motion.
    pending_register: Option<char>,
    // The keys fed since the last resolved motion or aborted sequence (`20g`
    // while typing `20gg`, `/cher` during a search), for a status bar only.
    current_input: String,
    // `current_input` as it was when it last resolved (`20gg`), kept until
    // the next key so a frontend can flash what just happened.
    last_completed: String,
    // `.`'s memory: the keys of the command being typed, the keys of the last
    // command that changed the buffer, and whether an Insert excursion is
    // still adding to the first.
    //
    // Keys, not an edit description: a change is replayed through the macro
    // replay queue, so `.` covers every command without knowing what any of
    // them does. A repeat re-resolves against the current buffer: `dw`
    // repeated deletes the word under the cursor now, as in vim.
    current_change: Vec<Key>,
    last_change: Vec<Key>,
    /// Where a pending search's pattern starts in `current_input`. The
    /// transcript records keys by label (`<BS>`), but for a search it's the
    /// pattern being edited, so it's rebuilt from here on every key. Like
    /// `insert_text_start`: what comes before belongs to the opening command.
    // Where a pending search's pattern begins inside `current_input`.
    //
    // The transcript is appended to before a key is dispatched, so a
    // search's own editing keys land in it as their labels -- `<BS>`
    // rather than one character less. For a search the transcript *is*
    // the pattern being edited, so it is rebuilt from this point on every
    // keystroke instead. Same shape as `insert_text_start` below, and for
    // the same reason: the keys before it belong to the command that
    // opened this, and only what follows is the text.
    search_input_start: usize,
    /// How many copies of an open Insert excursion's text go in, and
    /// whether each copy after the first starts a new line: `3ihi<Esc>`,
    /// `3oX<Esc>`. Set by the command that opens it, spent by the escape.
    insert_repeat: Option<(usize, bool)>,
    /// Where in `current_change` the text typed during the open
    /// excursion starts.
    insert_text_start: usize,
    // Set when a command opens Insert mode, cleared when the closing escape
    // comes through `next_key`. Meanwhile the host's Insert-mode reads join
    // the change being recorded, so `cwfoo<Esc>` repeats whole.
    capturing_insert: bool,
    // Visual mode: the shape and the anchor given to `begin_visual`; `None`
    // in Normal mode. Motion resolution ignores it: a `Motion` is applied
    // as usual, and what Visual changes (drawing the selection, `y`, `Z`,
    // Escape) lives in the caller (see `is_idle`).
    visual: Option<(RegisterShape, (usize, usize))>,
    // `gv`'s memory: the last Visual selection, stashed by `end_visual`.
    last_visual: Option<VisualSelection>,
    // `Ctrl-O`/`Ctrl-I` history as two stacks, like a browser's back and
    // forward, not vim's fixed ring with deduplication. `push_jump` (before a
    // `motion::is_jump` motion) pushes onto `jump_back` and clears
    // `jump_forward`.
    jump_back: Vec<(usize, usize)>,
    jump_forward: Vec<(usize, usize)>,
    // `q{a-z}`, `qA-Z`, `q`, `@{a-z}`, `@@` (see `next_key`). Keyed by the
    // lowercase letter: `qA` appends to `qa`'s slot, as in vim.
    macros: HashMap<char, Vec<Key>>,
    recording: Option<(char, Vec<Key>)>,
    replay_queue: VecDeque<Key>,
    // `::bish map`. Empty without mappings; only `next_command_key` consults
    // it (see there for why not `next_key`).
    matcher: crate::keymap::Matcher,
    // Characters a speculative mapping put on screen that the caller must
    // take back first; see `take_pending_revert`.
    pending_revert: usize,
    last_macro_register: Option<char>,
}

impl VimKeys {
    pub fn new() -> Self {
        VimKeys {
            count: None,
            pending: Pending::None,
            last_find: None,
            last_search_text: String::new(),
            search_highlight_off: false,
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
            current_change: Vec::new(),
            last_change: Vec::new(),
            search_input_start: 0,
            insert_repeat: None,
            insert_text_start: 0,
            capturing_insert: false,
            macros: HashMap::new(),
            recording: None,
            replay_queue: VecDeque::new(),
            matcher: crate::keymap::Matcher::new(Vec::new()),
            pending_revert: 0,
            last_macro_register: None,
        }
    }

    /// What's been typed toward the command in progress; empty when nothing
    /// is pending. Display only.
    pub fn pending_display(&self) -> &str {
        &self.current_input
    }

    /// The keys of the last applied motion, kept until the next key starts a
    /// sequence. Display only.
    pub fn last_motion_display(&self) -> &str {
        &self.last_completed
    }

    /// The pattern of the last `/` or `?` search; empty if the last was
    /// `*`/`#` or there was none. Search highlighting combines this with
    /// `pending_display()` and, for a word search,
    /// `motion::word_under_cursor`, which recovers the searched word.
    pub fn last_search_text(&self) -> &str {
        &self.last_search_text
    }

    /// `:noh`: stop drawing the current search's matches until the next
    /// search.
    pub fn suppress_search_highlight(&mut self) {
        self.search_highlight_off = true;
    }

    /// Whether search matches should be drawn now.
    pub fn search_highlight_on(&self) -> bool {
        !self.search_highlight_off
    }

    /// Whether the last search was `*`/`#` rather than `/`/`?` (see
    /// `last_search_text`).
    pub fn last_search_is_word(&self) -> bool {
        matches!(self.last_search, Some(LastSearch::Word { .. }))
    }

    /// Whether a `/` or `?` pattern is being typed, unlike
    /// `pending_display()`, which is also non-empty mid-count or
    /// mid-prefix. Tells a caller whether `CtrlC`/`CtrlD`/`CtrlZ` should
    /// leave the mode or reach `feed_search`, where `CtrlC` cancels just
    /// the search.
    pub fn is_search_pending(&self) -> bool {
        matches!(self.pending, Pending::Search { .. })
    }

    /// Whether Visual mode is currently active.
    pub fn is_visual(&self) -> bool {
        self.visual.is_some()
    }

    /// Visual mode's shape and anchor, `None` in Normal mode. With the
    /// buffer's cursor, that's the selection.
    pub fn visual_anchor(&self) -> Option<(RegisterShape, (usize, usize))> {
        self.visual
    }

    /// Moves the anchor without leaving Visual mode, for `SwapVisualEnds`.
    pub fn set_visual_anchor(&mut self, anchor: (usize, usize)) {
        if let Some((_, a)) = self.visual.as_mut() {
            *a = anchor;
        }
    }

    /// Whether the next key must arrive unmapped. `f{char}`, `m{mark}`,
    /// `r{char}` and the surround delimiters take it as a literal character:
    /// mapped, `fx` would find whatever `x` maps to. vim calls these states
    /// "pending an argument".
    pub fn wants_raw_key(&self) -> bool {
        !matches!(self.pending, Pending::None)
    }

    /// Which mode's mappings apply. Normal and Visual share this machine, so
    /// it's a property of its state.
    pub fn keymap_mode(&self) -> &'static str {
        if self.visual_anchor().is_some() { "visual" } else { "normal" }
    }

    /// Enters Visual mode on `KeyOutcome::EnterVisual`, with the cursor as
    /// `anchor`.
    pub fn begin_visual(&mut self, shape: RegisterShape, anchor: (usize, usize)) {
        self.visual = Some((shape, anchor));
    }

    /// Leaves Visual mode once the caller is done with the anchor. `cursor`
    /// is stashed with the shape and anchor as `last_visual` for `gv`, even
    /// on Escape, as in vim.
    pub fn end_visual(&mut self, cursor: (usize, usize)) {
        if let Some((shape, anchor)) = self.visual.take() {
            self.last_visual = Some((shape, anchor, cursor));
        }
    }

    /// `gv`'s target: the last Visual selection; `None` before one has ended.
    pub fn last_visual(&self) -> Option<VisualSelection> {
        self.last_visual
    }

    /// Records `pos`, the cursor before a `motion::is_jump` motion runs, and
    /// discards the forward history, as a browser does on new navigation.
    pub fn push_jump(&mut self, pos: (usize, usize)) {
        self.jump_back.push(pos);
        self.jump_forward.clear();
    }

    /// `Ctrl-O`: one entry back, if any. `current` goes onto the forward
    /// stack so `Ctrl-I` can return.
    pub fn jump_back(&mut self, current: (usize, usize)) -> Option<(usize, usize)> {
        let target = self.jump_back.pop()?;
        self.jump_forward.push(current);
        Some(target)
    }

    /// `Ctrl-I`, the same byte as Tab (see `feed_fresh`): the mirror of
    /// `jump_back`.
    pub fn jump_forward(&mut self, current: (usize, usize)) -> Option<(usize, usize)> {
        let target = self.jump_forward.pop()?;
        self.jump_back.push(current);
        Some(target)
    }

    /// Takes the register a `"x` prefix selected. `feed()` applies it to
    /// outcomes it resolves; this is for a caller that handles a key itself,
    /// like repl.rs's Visual `y`.
    pub fn take_pending_register(&mut self) -> Option<char> {
        self.pending_register.take()
    }

    /// Whether this is a fresh dispatch point: no count, no pending prefix,
    /// no armed operator. A caller handling a key itself before `feed()`
    /// (repl.rs does for `Z`, `:` and Visual `y`/Escape) checks this first,
    /// or it could steal an `f` target or a count digit.
    pub fn is_idle(&self) -> bool {
        matches!(self.pending, Pending::None) && self.count.is_none() && self.active_operator.is_none()
    }

    /// `is_idle` without the count check, for `@`: `[count]@a` repeats, so
    /// `@` must work with a count buffered (see `take_count`).
    pub fn is_idle_except_count(&self) -> bool {
        matches!(self.pending, Pending::None) && self.active_operator.is_none()
    }

    /// Takes the accumulated count, for a caller handling `@` itself.
    pub fn take_count(&mut self) -> Option<usize> {
        self.count.take()
    }

    /// The register being recorded into (lowercase), if recording.
    pub fn is_recording(&self) -> Option<char> {
        self.recording.as_ref().map(|(reg, _)| *reg)
    }

    /// `q{reg}`: starts recording into `reg`, lowercased. An uppercase `reg`
    /// starts from the register's existing keys, so new ones append, as in
    /// vim. Called while recording, it restarts rather than nesting.
    pub fn start_recording(&mut self, reg: char) {
        let lower = reg.to_ascii_lowercase();
        let seed = if reg.is_ascii_uppercase() { self.macros.get(&lower).cloned().unwrap_or_default() } else { Vec::new() };
        self.recording = Some((lower, seed));
    }

    /// Bare `q` while recording: stops and saves. A no-op when not recording.
    pub fn stop_recording(&mut self) {
        if let Some((reg, mut keys)) = self.recording.take() {
            // Drop the `q` that stopped the recording: `next_key` records
            // every key it reads, this one included. Replaying it would
            // start a new recording that swallows the next key as its
            // register name.
            if keys.last() == Some(&Key::Char('q')) {
                keys.pop();
            }
            self.macros.insert(reg, keys);
        }
    }

    /// Appends `key` to the recording, if any. Called by `next_key` for keys
    /// read from the terminal, never for replayed ones. `pub(crate)` so
    /// fileeditor.rs's macro tests can seed a macro without a terminal.
    pub(crate) fn record_key(&mut self, key: Key) {
        if let Some((_, keys)) = &mut self.recording {
            keys.push(key);
        }
    }

    /// `@{reg}`/`@@`: queues the register's keys, `count` times, ahead of
    /// anything queued, so a macro invoked inside another's replay runs at
    /// once. `'@'` means the last register `@` used. `false`, queueing
    /// nothing, for an empty or unknown register, quietly, like a failed `f`.
    pub fn queue_macro_replay(&mut self, reg: char, count: usize) -> bool {
        let target = if reg == '@' { self.last_macro_register } else { Some(reg.to_ascii_lowercase()) };
        let Some(target) = target else { return false };
        let Some(keys) = self.macros.get(&target).filter(|k| !k.is_empty()) else { return false };
        self.last_macro_register = Some(target);
        for _ in 0..count.max(1) {
            for key in keys.iter().rev() {
                self.replay_queue.push_front(*key);
            }
        }
        true
    }

    /// Puts `keys` at the front of the replay queue, so the host's next reads
    /// see them as typed. Used by `:normal`, `@{reg}` and `.`.
    pub fn queue_keys(&mut self, keys: impl DoubleEndedIterator<Item = Key>) {
        for key in keys.rev() {
            self.replay_queue.push_front(key);
        }
    }

    /// Installs this session's `::bish map` table, on entering Normal mode
    /// and after each command-mode command, since `::bish map` can change
    /// it from the colon line.
    pub fn set_mappings(&mut self, mappings: Vec<crate::keymap::Mapping>) {
        self.matcher = crate::keymap::Matcher::new(mappings);
    }

    /// How many characters the caller must remove before dispatching the
    /// key just returned. Insert mode shows a mapping's prefix as it's
    /// typed, so when `j` turns out to be `jk` it has to come back off. An
    /// Insert session is one undo step, so this leaves nothing in the undo
    /// tree.
    pub fn take_pending_revert(&mut self) -> usize {
        std::mem::take(&mut self.pending_revert)
    }

    /// Whether a key is already waiting to be delivered, from a macro
    /// replay or a mapping's expansion.
    ///
    /// Every loop that waits for input polls *stdin* before reading, so
    /// each one has to ask this first: a queued key is not a byte on
    /// stdin and the poll will never see it. Without the check the loop
    /// blocks with keys in hand, and they come out one per real
    /// keystroke -- which is what made a multi-key macro appear to
    /// replay only its first key.
    pub fn has_pending_keys(&self) -> bool {
        !self.replay_queue.is_empty()
    }

    /// Takes whatever a mapping has queued but not yet delivered.
    ///
    /// Needed where a mapped key hands control to a loop that does not
    /// read through this machine at all: `:` opens command mode, which
    /// is `read_line` on raw stdin and has no idea a replay queue
    /// exists. Without handing the rest over, a right-hand side like
    /// `<Esc>:w<CR>` would fire its `<Esc>` and `:` and then silently
    /// drop the `w<CR>` -- the mapping doing half its job, which is
    /// worse than doing none.
    pub fn take_replay_queue(&mut self) -> Vec<Key> {
        self.replay_queue.drain(..).collect()
    }

    /// Reads the next key *as a command*, applying `::bish map` to it.
    ///
    /// Deliberately separate from `next_key` rather than folded into it.
    /// `next_key` also serves Insert mode and the several places that
    /// read one raw character for their own purposes; running those
    /// through the keymap would remap text being typed and arguments
    /// being collected. Only a key that is about to be interpreted as a
    /// command belongs here.
    ///
    /// Mapped keys go out through `replay_queue`, which is popped at the
    /// top *before* the matcher runs -- so a mapping's own output can
    /// never be mapped again, and the non-recursion holds without a flag
    /// to get wrong. It is also why a macro replaying through this is
    /// unaffected: those keys were already resolved when recorded.
    ///
    /// The loop is the buffering: while a multi-key mapping is still
    /// being decided nothing is returned yet, so the caller simply waits
    /// on the next keystroke the way it waits on any other.
    pub fn next_command_key(&mut self, read: impl FnMut() -> io::Result<Option<Key>>) -> io::Result<Option<Key>> {
        let mode = self.keymap_mode();
        self.next_mapped_key(mode, read)
    }

    /// The same, for a mode that is not this machine's own -- Insert
    /// mode passes "insert", command mode "command". Kept explicit
    /// rather than inferred, because those modes are driven by other
    /// loops entirely and `keymap_mode` only knows about normal and
    /// visual.
    pub fn next_mapped_key(&mut self, mode: &str, mut read: impl FnMut() -> io::Result<Option<Key>>) -> io::Result<Option<Key>> {
        loop {
            if let Some(key) = self.replay_queue.pop_front() {
                return Ok(Some(self.capture_insert_key(key)));
            }
            let Some(key) = read()? else { return Ok(None) };
            self.record_key(key);
            // An argument-pending state takes the next keystroke as a
            // literal (`fx` finds an `x`), so it must not be remapped.
            if self.matcher.is_empty() || self.wants_raw_key() {
                return Ok(Some(self.capture_insert_key(key)));
            }
            // Insert mode can take a printable key back off the screen,
            // so it lets the matcher type one speculatively rather than
            // holding it; every other mode holds. See `Matcher::feed`.
            let mut out = self.matcher.feed(key, mode, mode == "insert");
            self.pending_revert += out.revert;
            if out.keys.is_empty() {
                continue;
            }
            let first = out.keys.remove(0);
            self.replay_queue.extend(out.keys);
            return Ok(Some(self.capture_insert_key(first)));
        }
    }

    /// Adds `key` to the change `.` will repeat, if an Insert-mode
    /// excursion is open, and closes that excursion out when the key is
    /// the one that ends it.
    ///
    /// Called from both read seams because Insert mode uses
    /// `next_mapped_key` (so that `::bish map -m insert` applies to what
    /// gets typed) while Normal-mode lookaheads use `next_key`, and the
    /// text typed during a change arrives through whichever one the host
    /// happens to be in. Normal-mode keys are recorded in `feed`, not
    /// here -- `capturing_insert` is false then, which is what keeps
    /// them from being counted twice.
    fn capture_insert_key(&mut self, key: Key) -> Key {
        // The escape that closes a counted excursion (`3ihi<Esc>`). The
        // text goes in again first, count - 1 more times, while the host
        // is still in Insert mode to type it -- after a line break each
        // time for `o`/`O` -- and the escape that really ends the
        // excursion comes last. The change `.` repeats is what was
        // typed, count included, so it is recorded here and the copies
        // are not. `Ctrl-C` leaves without repeating, as vim's does.
        if self.capturing_insert
            && key == Key::Escape
            && let Some((count, new_line)) = self.insert_repeat.take()
        {
            let typed = self.current_change[self.insert_text_start.min(self.current_change.len())..].to_vec();
            let mut again = Vec::new();
            for _ in 1..count {
                if new_line {
                    again.push(Key::Enter);
                }
                again.extend_from_slice(&typed);
            }
            again.push(Key::Escape);
            self.current_change.push(key);
            self.capturing_insert = false;
            self.last_change = std::mem::take(&mut self.current_change);
            for queued in again.into_iter().rev() {
                self.replay_queue.push_front(queued);
            }
            return self.replay_queue.pop_front().unwrap_or(key);
        }
        if self.capturing_insert {
            self.current_change.push(key);
            if matches!(key, Key::Escape | Key::CtrlC) {
                self.capturing_insert = false;
                self.last_change = std::mem::take(&mut self.current_change);
            }
        }
        key
    }

    /// The seam every host key read goes through (the main read, lookahead
    /// reads like `S`'s delimiter or `Z`'s second `Z`, and
    /// `fileeditor::run_insert_mode`'s), so macros need no bookkeeping in
    /// the hosts. A queued replay key is served first and never recorded:
    /// recording `@b` inside macro `a` records the keys `@` and `b`, not
    /// `b`'s expansion. Otherwise it calls `read` and records the key.
    pub fn next_key(&mut self, mut read: impl FnMut() -> io::Result<Option<Key>>) -> io::Result<Option<Key>> {
        let key = match self.replay_queue.pop_front() {
            Some(key) => Some(key),
            None => {
                let key = read()?;
                if let Some(key) = key {
                    self.record_key(key);
                }
                key
            }
        };
        Ok(key.map(|key| self.capture_insert_key(key)))
    }

    /// Whether `outcome` changed the buffer, and so is what `.` should
    /// repeat. Motions, scrolls, visual-mode bookkeeping and undo are
    /// not changes; vim draws the same line, and `u` in particular must
    /// not become the thing `.` repeats.
    fn outcome_is_a_change(outcome: &KeyOutcome) -> bool {
        match outcome {
            KeyOutcome::Operator(op, ..) | KeyOutcome::OperatorLines(op, ..) => *op != Op::Yank,
            KeyOutcome::Put { .. }
            | KeyOutcome::DeleteCharForward { .. }
            | KeyOutcome::Join { .. }
            | KeyOutcome::AddSurround { .. }
            | KeyOutcome::DeleteSurround { .. }
            | KeyOutcome::ChangeSurround { .. }
            | KeyOutcome::ReplaceChar { .. }
            | KeyOutcome::ToggleCase { .. }
            | KeyOutcome::AdjustNumber { .. }
            | KeyOutcome::OpenLine { .. }
            | KeyOutcome::EnterInsert(_)
            | KeyOutcome::EnterReplace => true,
            _ => false,
        }
    }

    /// Whether `outcome` leaves the host in Insert mode, so that the
    /// keys it is about to read are still part of this change.
    fn outcome_opens_insert(outcome: &KeyOutcome) -> bool {
        match outcome {
            KeyOutcome::Operator(op, ..) | KeyOutcome::OperatorLines(op, ..) => *op == Op::Change,
            KeyOutcome::EnterInsert(_) | KeyOutcome::EnterReplace | KeyOutcome::OpenLine { .. } => true,
            _ => false,
        }
    }

    pub fn feed(&mut self, key: Key) -> KeyOutcome {
        // A key arriving with nothing typed toward a command yet starts
        // a new one, which is where the previous command's keys stop
        // being interesting -- unless Insert mode is still open, in
        // which case they are not a new command at all.
        if self.current_input.is_empty() && !self.capturing_insert {
            self.current_change.clear();
        }
        // A count to repeat an excursion's text by belongs to the command
        // that opens it, which the emitters below set as they resolve.
        if !self.capturing_insert {
            self.insert_repeat = None;
        }
        self.current_change.push(key);
        // Wrapped rather than inlined below, because what a key resolves
        // to is not settled until every one of `feed_resolving`'s own
        // returns has been taken: `dd` leaves through an early
        // double-tap return near the top, and `dw` is still a bare
        // `Motion` until the armed-operator block at the very bottom
        // turns it into one. Asking anywhere but out here would record
        // some changes and miss others.
        let outcome = self.feed_resolving(key);
        if Self::outcome_is_a_change(&outcome) {
            if Self::outcome_opens_insert(&outcome) {
                // Not finished yet: the text still has to be typed, and
                // it arrives through `next_key`, which closes this out
                // on the escape.
                self.capturing_insert = true;
                self.insert_text_start = self.current_change.len();
            } else {
                self.last_change = self.current_change.clone();
            }
        }
        outcome
    }

    fn feed_resolving(&mut self, key: Key) -> KeyOutcome {
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

    // Unlike emit()/emit_window(), the count is not handed to the host:
    // for `i`/`a`/`I`/`A` it repeats what gets typed, and `VimKeys` does
    // that itself -- see KeyOutcome::EnterInsert's own doc comment.
    fn emit_insert(&mut self, cmd: InsertCmd) -> KeyOutcome {
        let count = self.count.take();
        if matches!(cmd, InsertCmd::Before | InsertCmd::After | InsertCmd::LineStart | InsertCmd::LineEnd) {
            self.insert_repeat = count.filter(|n| *n > 1).map(|n| (n, false));
        }
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterInsert(cmd)
    }

    // `v`/`V`: same shape as emit_insert -- but a count typed before
    // entering Visual mode has no effect yet. Doesn't touch
    // `self.visual` itself -- that only happens once the caller calls
    // `begin_visual` back with the anchor, which this function has no way
    // to know (see `KeyOutcome::EnterVisual`'s own doc comment).
    fn emit_visual(&mut self, shape: RegisterShape) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterVisual(shape)
    }

    // `2Rab<Esc>`: what is typed replaces, and then replaces again.
    fn emit_replace(&mut self) -> KeyOutcome {
        let count = self.count.take();
        self.insert_repeat = count.filter(|n| *n > 1).map(|n| (n, false));
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::EnterReplace
    }

    // Same shape as emit_insert, and a count the same: `3oX<Esc>` opens a
    // line three times, with `X` on each.
    //
    // In Visual mode neither `o` nor `O` is "open a line" at all: both
    // swap which end of the selection is being held. Deciding that here
    // rather than in the caller is what keeps it right for all four of
    // them -- `o` in Visual used to fall through as an ordinary
    // open-line, which left Visual mode, opened a line, and typed the
    // rest of the keys into it.
    //
    // `O` differs from `o` only in Blockwise Visual, where vim swaps the
    // corner horizontally rather than diagonally. Treating it as the
    // plain swap is exactly right for Charwise and Linewise, and for a
    // block it moves to a corner rather than opening a line, which is
    // the nearer of the two wrong answers by a distance.
    fn emit_open_line(&mut self, above: bool) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        if self.visual.is_some() {
            return KeyOutcome::SwapVisualEnds;
        }
        self.insert_repeat = count.filter(|n| *n > 1).map(|n| (n, true));
        KeyOutcome::OpenLine { above }
    }

    fn emit_opener(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Opener
    }

    fn emit_reselect_visual(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::ReselectVisual
    }

    fn emit_goto_definition(&mut self, kind: GotoKind) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::GotoDefinition(kind)
    }

    fn emit_goto_references(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::GotoReferences
    }

    fn emit_document_symbols(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::DocumentSymbols
    }

    fn emit_code_actions(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::CodeActions
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

    fn emit_undo(&mut self) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Undo(count)
    }

    fn emit_redo(&mut self) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Redo(count)
    }

    // `g-`/`g+` -- called from `feed_g`, which is why `self.count` still
    // holds whatever was typed *before* the leading `g` (`3g-`): entering
    // `Pending::G` never touches `self.count`, same as every other
    // `g`-prefixed command that reads a count this way (`gJ`, `gu{motion}`,
    // ...).
    fn emit_undo_seq(&mut self, forward: bool) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::UndoSeq { forward, count }
    }

    fn emit_put(&mut self, before: bool) -> KeyOutcome {
        let count = self.count.take();
        let register = self.pending_register.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Put { before, count, register }
    }

    /// `.`: replays the keys of the last change through the same queue
    /// `@{reg}` uses, so the repeat is the original command happening
    /// again rather than a second implementation of it.
    ///
    /// Returns `None` rather than an outcome of its own: nothing has
    /// happened yet at this point, and nothing needs to -- the host's
    /// next read comes out of the replay queue and drives the command
    /// through the ordinary path, which is also what leaves `.` itself
    /// repeatable afterwards.
    ///
    /// `[count].` repeats the change with its count in place of the one
    /// it was made with, as vim's does: `3dd` then `2.` deletes two lines,
    /// where a plain `.` would delete three again.
    fn emit_repeat_change(&mut self) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.pending_register = None;
        self.last_completed = std::mem::take(&mut self.current_input);
        let mut keys = self.last_change.clone();
        if let Some(count) = count {
            let digits = keys.iter().take_while(|k| matches!(k, Key::Char(c) if c.is_ascii_digit())).count();
            keys.splice(..digits, count.to_string().chars().map(Key::Char));
        }
        for key in keys.iter().rev() {
            self.replay_queue.push_front(*key);
        }
        KeyOutcome::None
    }

    fn abort(&mut self) -> KeyOutcome {
        self.count = None;
        self.pending = Pending::None;
        self.pending_register = None;
        self.current_input.clear();
        KeyOutcome::None
    }

    /// Starts a `/` or `?` search, remembering where its pattern begins
    /// in the transcript.
    fn open_search(&mut self, forward: bool) -> KeyOutcome {
        self.search_input_start = self.current_input.chars().count();
        self.show_search(forward, String::new())
    }

    /// Holds `text` as the pending search and makes the transcript show
    /// it, which for a search is the whole of what the status line has to
    /// say: whatever opened the search, then the pattern as it stands.
    fn show_search(&mut self, forward: bool, text: String) -> KeyOutcome {
        let opened: String = self.current_input.chars().take(self.search_input_start).collect();
        self.current_input = opened + &text;
        self.pending = Pending::Search { forward, text };
        KeyOutcome::Pending
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
            // The physical PageDown/PageUp keys are plain synonyms for
            // Ctrl-F/Ctrl-B, matching real vim's own convention.
            Key::CtrlF | Key::PageDown => self.emit(Motion::PageDown),
            Key::CtrlB | Key::PageUp => self.emit(Motion::PageUp),
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
            // The `/` or `?` is already in the transcript (every key is,
            // before it is dispatched), so the pattern starts right after
            // it -- and a count typed before it stays where it is.
            Key::Char('/') => self.open_search(true),
            Key::Char('?') => self.open_search(false),
            Key::Char('n') => self.emit_last_search(true),
            Key::Char('N') => self.emit_last_search(false),
            Key::Char('*') => {
                self.search_highlight_off = false;
                self.last_search = Some(LastSearch::Word { forward: true, bounded: true });
                self.emit(Motion::SearchWordForward)
            }
            Key::Char('#') => {
                self.search_highlight_off = false;
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
            Key::CtrlT => self.emit_opener(),
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
            Key::Char('o') => self.emit_open_line(false),
            Key::Char('O') => self.emit_open_line(true),
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
            // `>{motion}`/`>>`, `<{motion}`/`<<`: arms the same way any
            // other operator does -- the double-tap shorthand (`>>`/`<<`)
            // is already handled generically by `feed`'s own check above
            // (`c == op.trigger_char()`), nothing extra needed here.
            Key::Char('>') => self.emit_operator(Op::Indent),
            Key::Char('<') => self.emit_operator(Op::Outdent),
            // `u`/`Ctrl-R`: undo/redo -- see KeyOutcome::Undo/Redo's own
            // doc comments. `Key::CtrlR` already exists as an editor::Key
            // variant, but only ever handled by editor::read_line's own
            // Insert-mode typing loop today (real vim's own
            // `<C-r>{register}` paste-while-typing, a different context
            // entirely from Normal-mode redo) -- no conflict.
            Key::Char('u') => self.emit_undo(),
            Key::Char('.') => self.emit_repeat_change(),
            Key::CtrlR => self.emit_redo(),
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
            // `Ctrl-V`: a rectangle rather than a run. The third visual
            // shape, and the one that makes a column edit -- commenting
            // a block, stripping a prefix off twenty lines -- one
            // gesture instead of twenty.
            Key::CtrlV if self.visual.is_none() => self.emit_visual(RegisterShape::Block),
            _ => self.abort(),
        }
    }

    fn feed_register(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9' | '+' | '"' | '_' | '.' | '%' | ':')) => {
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
                self.search_highlight_off = false;
                self.last_search = Some(LastSearch::Word { forward: true, bounded: false });
                self.emit(Motion::SearchWordForwardUnbounded)
            }
            Key::Char('#') => {
                self.search_highlight_off = false;
                self.last_search = Some(LastSearch::Word { forward: false, bounded: false });
                self.emit(Motion::SearchWordBackwardUnbounded)
            }
            Key::Char('d') => self.emit_goto_definition(GotoKind::Definition),
            // The two questions next to "where is this defined" whose
            // keys were free to take. `gy` is unused in vim and is what
            // helix, VS Code and neovim all reach for; `gD` in vim
            // already means "find where this was declared", just by
            // scanning the file backwards rather than by asking someone
            // who knows -- the same relationship `gd` and `K` already
            // have with their own vim meanings.
            //
            // `textDocument/implementation`'s own settled binding is
            // `gi`, which is not free: it means "resume insert where I
            // left off" here and in vim. Not taken without asking.
            Key::Char('y') => self.emit_goto_definition(GotoKind::TypeDefinition),
            Key::Char('D') => self.emit_goto_definition(GotoKind::Declaration),
            Key::Char('r') => self.emit_goto_references(),
            Key::Char('O') => self.emit_document_symbols(),
            Key::Char('a') => self.emit_code_actions(),
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
            // `g-`/`g+`: undo-tree time travel -- see KeyOutcome::
            // UndoSeq's own doc comment.
            Key::Char('-') => self.emit_undo_seq(false),
            Key::Char('+') => self.emit_undo_seq(true),
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
            Key::Char('z') => self.emit(Motion::FoldStart),
            _ => self.abort(),
        }
    }

    fn feed_bracket_close(&mut self, key: Key) -> KeyOutcome {
        match key {
            Key::Char(')') => self.emit(Motion::UnmatchedCloseParen),
            Key::Char('}') => self.emit(Motion::UnmatchedCloseBrace),
            Key::Char(']') => self.emit(Motion::SectionForward),
            Key::Char('[') => self.emit(Motion::SectionForwardEnd),
            Key::Char('z') => self.emit(Motion::FoldEnd),
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
            // Folds. `zf` is the one that takes a motion, except in
            // Visual mode, where the selection already says which lines.
            Key::Char('f') if self.visual.is_some() => self.emit_fold(FoldCmd::CreateFromSelection),
            Key::Char('f') => {
                self.pending = Pending::None;
                self.emit_operator(Op::Fold)
            }
            Key::Char('F') => self.emit_fold(FoldCmd::CreateLines),
            Key::Char('o') => self.emit_fold(FoldCmd::Open),
            Key::Char('O') => self.emit_fold(FoldCmd::OpenAll),
            Key::Char('c') => self.emit_fold(FoldCmd::Close),
            Key::Char('C') => self.emit_fold(FoldCmd::CloseAll),
            Key::Char('a') => self.emit_fold(FoldCmd::Toggle),
            Key::Char('A') => self.emit_fold(FoldCmd::ToggleAll),
            Key::Char('v') => self.emit_fold(FoldCmd::View),
            Key::Char('R') => self.emit_fold(FoldCmd::OpenEverything),
            Key::Char('M') => self.emit_fold(FoldCmd::CloseEverything),
            Key::Char('d') => self.emit_fold(FoldCmd::Delete),
            Key::Char('D') => self.emit_fold(FoldCmd::DeleteAll),
            Key::Char('E') => self.emit_fold(FoldCmd::Eliminate),
            Key::Char('i') => self.emit_fold(FoldCmd::ToggleEnabled),
            Key::Char('j') => self.emit(Motion::FoldNextStart),
            Key::Char('k') => self.emit(Motion::FoldPreviousEnd),
            _ => self.abort(),
        }
    }

    fn emit_fold(&mut self, cmd: FoldCmd) -> KeyOutcome {
        let count = self.count.take();
        self.pending = Pending::None;
        self.last_completed = std::mem::take(&mut self.current_input);
        KeyOutcome::Fold(cmd, count)
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
            // Two letters, one command, because two vocabularies reach
            // for it: `z` is what tmux binds zoom to, and `o` is what
            // vim binds "make this the only window" to -- which is what
            // zoom looks like, except nothing is closed and the next
            // press puts it all back.
            Key::Char('z') | Key::Char('o') => self.emit_window(WindowCmd::Zoom),
            Key::Char(' ') => self.emit_window(WindowCmd::NextLayout),
            Key::Char('x') => self.emit_window(WindowCmd::SwapPane),
            Key::Char('T') => self.emit_window(WindowCmd::BreakPane),
            Key::Char('=') => self.emit_window(WindowCmd::Balance),
            // vim spells "make this pane as small as it goes" the same way.
            Key::Char('_') => self.emit_window(WindowCmd::Minimize),
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
                self.search_highlight_off = false;
                self.last_search = Some(LastSearch::Pattern { forward });
                self.emit(if forward { Motion::SearchForward(text) } else { Motion::SearchBackward(text) })
            }
            Key::Escape => self.abort(),
            // Cancels only the search, same as Escape -- callers gate
            // their own outer Ctrl-C/D/Z interception on
            // `is_search_pending()` precisely so this arm gets a chance
            // to run instead of the whole mode exiting first.
            Key::CtrlC => self.abort(),
            Key::Backspace if text.is_empty() => self.abort(),
            Key::Backspace => {
                text.pop();
                self.show_search(forward, text)
            }
            Key::Char(c) => {
                text.push(c);
                self.show_search(forward, text)
            }
            _ => {
                // Ignore anything else while typing a search string rather
                // than aborting it -- a stray unrecognized key shouldn't
                // discard what's already been typed. It leaves no mark on
                // the transcript either, which `show_search` is what makes
                // true.
                self.show_search(forward, text)
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
        // `n`/`N` reuse the last search rather than setting a new one, so
        // they need their own clear -- and they must have it: vim brings
        // the highlight back on `n` after a `:noh`, which is most of what
        // makes `:noh` safe to press.
        self.search_highlight_off = false;
        match self.last_search {
            Some(LastSearch::Pattern { forward }) => {
                let forward = if same_direction { forward } else { !forward };
                let text = self.last_search_text.clone();
                self.emit(if forward { Motion::SearchForward(text) } else { Motion::SearchBackward(text) })
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

/// Resolves a mapping's right-hand side against the *default* bindings
/// and names what it does, or says why it is not a usable right-hand
/// side.
///
/// A fresh `VimKeys` is the whole of what makes this noremap: it has no
/// keymap of its own to consult, so the resolution cannot chain through
/// another mapping however the table is arranged. Called when a mapping
/// is defined, both to reject a bad right-hand side up front and to
/// produce the description `::bish map` lists.
///
/// A sequence may resolve to more than one action -- `jj` is two -- and
/// is named as such, joined with " then ". Ending mid-action is an
/// error rather than a mapping that swallows keys and does nothing.
pub fn describe_key_sequence(keys: &[Key]) -> Result<String, String> {
    let mut vk = VimKeys::new();
    let mut actions = Vec::new();
    let mut last = KeyOutcome::Pending;
    for key in keys {
        last = vk.feed(*key);
        match &last {
            KeyOutcome::Pending => {}
            KeyOutcome::None => return Err("unrecognized".to_string()),
            outcome => actions.push(describe_outcome(outcome)),
        }
    }
    if matches!(last, KeyOutcome::Pending) {
        return Err("incomplete".to_string());
    }
    if actions.is_empty() {
        return Err("unrecognized".to_string());
    }
    Ok(actions.join(" then "))
}

/// The canonical name of a resolved key outcome, as `::bish map` prints
/// it.
///
/// Kebab-case of the variant, with whatever it carries appended -- the
/// same mechanical rule `motion::describe_motion` follows, and for the
/// same reason: a predictable vocabulary beats a memorable one, and a
/// derived name cannot drift from the variant it names.
///
/// A count is printed where it came from the mapping's own right-hand
/// side (`10j` lists as `down 10`). It is not the whole story at
/// dispatch -- a count typed before the mapped key composes with it,
/// because a mapping replays its keys into the live state rather than
/// producing a frozen outcome -- but it is what the mapping itself says.
pub fn describe_outcome(outcome: &KeyOutcome) -> String {
    use KeyOutcome::*;
    // `Some(3)` prints as " 3", `None` as nothing -- so every arm below
    // can append a count without repeating the match.
    fn n(count: &Option<usize>) -> String {
        count.map(|c| format!(" {c}")).unwrap_or_default()
    }
    fn reg(register: &Option<char>) -> String {
        register.map(|r| format!(" register {r:?}")).unwrap_or_default()
    }
    match outcome {
        Motion(m, count) => format!("{}{}", crate::bishedit::motion::describe_motion(m), n(count)),
        Window(cmd, count) => format!("window {}{}", describe_window_cmd(cmd), n(count)),
        Opener => "opener".to_string(),
        EnterInsert(cmd) => format!("insert {}", describe_insert_cmd(cmd)),
        Operator(op, m, count, register) => {
            format!("{} {}{}{}", describe_op(op), crate::bishedit::motion::describe_motion(m), n(count), reg(register))
        }
        OperatorLines(op, count, register) => format!("{} line{}{}", describe_op(op), n(count), reg(register)),
        Put { before, count, register } => {
            format!("put-{}{}{}", if *before { "before" } else { "after" }, n(count), reg(register))
        }
        DeleteCharForward { count, register } => format!("delete-char-forward{}{}", n(count), reg(register)),
        Join { count, with_space } => {
            format!("join{}{}", if *with_space { "" } else { "-without-space" }, n(count))
        }
        EnterVisual(shape) => format!("visual {}", describe_register_shape(shape)),
        ReselectVisual => "visual-reselect".to_string(),
        GotoDefinition(kind) => describe_goto_kind(kind).to_string(),
        GotoReferences => "goto-references".to_string(),
        DocumentSymbols => "document-symbols".to_string(),
        CodeActions => "code-actions".to_string(),
        Jump { forward } => format!("jump-{}", if *forward { "forward" } else { "backward" }),
        Undo(count) => format!("undo{}", n(count)),
        Redo(count) => format!("redo{}", n(count)),
        UndoSeq { forward, count } => {
            format!("undo-seq-{}{}", if *forward { "forward" } else { "backward" }, n(count))
        }
        AddSurround { target, ch } => format!("add-surround {} {ch:?}", describe_surround_target(target)),
        DeleteSurround { ch } => format!("delete-surround {ch:?}"),
        ChangeSurround { ch, replacement } => format!("change-surround {ch:?} {replacement:?}"),
        ReplaceChar { ch, count } => format!("replace-char {ch:?}{}", n(count)),
        EnterReplace => "replace-mode".to_string(),
        ToggleCase { count } => format!("toggle-case{}", n(count)),
        AdjustNumber { delta } => format!("adjust-number {delta}"),
        OpenLine { above } => format!("open-line-{}", if *above { "above" } else { "below" }),
        SwapVisualEnds => "visual-swap-ends".to_string(),
        Fold(cmd, count) => format!("fold-{}{}", describe_fold_cmd(cmd), n(count)),
        // Neither is a resolved action: `Pending` means the sequence
        // wants more keys, `None` that nothing recognized it. `::bish
        // map` refuses a right-hand side that ends on either rather than
        // storing a mapping that does nothing, so these are here for
        // exhaustiveness and to make the refusal message say which.
        Pending => "(incomplete)".to_string(),
        None => "(unrecognized)".to_string(),
    }
}

fn describe_op(op: &Op) -> &'static str {
    match op {
        Op::Yank => "yank",
        Op::Delete => "delete",
        Op::Change => "change",
        Op::Lowercase => "lowercase",
        Op::Uppercase => "uppercase",
        Op::CaseToggle => "case-toggle",
        Op::Indent => "indent",
        Op::Outdent => "outdent",
        Op::Fold => "fold",
    }
}

fn describe_fold_cmd(cmd: &FoldCmd) -> &'static str {
    match cmd {
        FoldCmd::Open => "open",
        FoldCmd::OpenAll => "open-all",
        FoldCmd::Close => "close",
        FoldCmd::CloseAll => "close-all",
        FoldCmd::Toggle => "toggle",
        FoldCmd::ToggleAll => "toggle-all",
        FoldCmd::View => "view",
        FoldCmd::OpenEverything => "open-everything",
        FoldCmd::CloseEverything => "close-everything",
        FoldCmd::Delete => "delete",
        FoldCmd::DeleteAll => "delete-all",
        FoldCmd::Eliminate => "eliminate",
        FoldCmd::ToggleEnabled => "toggle-enabled",
        FoldCmd::CreateLines => "create-lines",
        FoldCmd::CreateFromSelection => "create-from-selection",
    }
}

fn describe_insert_cmd(cmd: &InsertCmd) -> &'static str {
    match cmd {
        InsertCmd::Before => "before",
        InsertCmd::After => "after",
        InsertCmd::LineStart => "line-start",
        InsertCmd::LineEnd => "line-end",
        InsertCmd::SubstituteChar => "substitute-char",
        InsertCmd::SubstituteLine => "substitute-line",
        InsertCmd::ChangeToEnd => "change-to-end",
        InsertCmd::LastInsertPos => "last-insert-pos",
    }
}

fn describe_window_cmd(cmd: &WindowCmd) -> &'static str {
    match cmd {
        WindowCmd::Next => "next",
        WindowCmd::Previous => "previous",
        WindowCmd::New => "new",
        WindowCmd::Close => "close",
        WindowCmd::Split => "split",
        WindowCmd::VSplit => "vsplit",
        WindowCmd::FocusLeft => "focus-left",
        WindowCmd::FocusDown => "focus-down",
        WindowCmd::FocusUp => "focus-up",
        WindowCmd::FocusRight => "focus-right",
        WindowCmd::Zoom => "zoom",
        WindowCmd::NextLayout => "next-layout",
        WindowCmd::SwapPane => "swap-pane",
        WindowCmd::BreakPane => "break-pane",
        WindowCmd::Balance => "balance",
        WindowCmd::Minimize => "minimize",
        WindowCmd::GotoFirstWindow => "goto-first",
        WindowCmd::GotoLastWindow => "goto-last",
    }
}

fn describe_goto_kind(kind: &GotoKind) -> &'static str {
    match kind {
        GotoKind::Definition => "goto-definition",
        GotoKind::TypeDefinition => "goto-type-definition",
        GotoKind::Implementation => "goto-implementation",
        GotoKind::Declaration => "goto-declaration",
    }
}

fn describe_register_shape(shape: &RegisterShape) -> &'static str {
    match shape {
        RegisterShape::Char => "charwise",
        RegisterShape::Line => "linewise",
        RegisterShape::Block => "blockwise",
    }
}

fn describe_surround_target(target: &SurroundTarget) -> String {
    match target {
        SurroundTarget::Motion(m, count) => {
            let c = count.map(|c| format!(" {c}")).unwrap_or_default();
            format!("{}{c}", crate::bishedit::motion::describe_motion(m))
        }
        SurroundTarget::Line(count) => format!("line{}", count.map(|c| format!(" {c}")).unwrap_or_default()),
    }
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

    // Resolving a key sequence and naming what it resolved to is
    // exactly what `::bish map` does with a right-hand side, so these
    // go through the real path rather than constructing outcomes by
    // hand.
    fn describe_typed(text: &str) -> String {
        let keys = crate::keymap::parse_keys(text).expect("test key sequence should parse");
        let mut vk = VimKeys::new();
        describe_outcome(&last(&mut vk, &keys))
    }

    #[test]
    fn a_right_hand_side_is_named_by_what_it_resolves_to() {
        assert_eq!(describe_typed("j"), "down");
        assert_eq!(describe_typed("<C-d>"), "half-page-down");
        assert_eq!(describe_typed("gg"), "goto-first-line");
        // The count rides along, because it is part of what the mapping
        // says -- `10j` and `j` are not the same mapping.
        assert_eq!(describe_typed("10j"), "down 10");
        assert_eq!(describe_typed("$"), "line-end");
    }

    #[test]
    fn an_operator_names_both_halves() {
        assert_eq!(describe_typed("dw"), "delete word-forward");
        assert_eq!(describe_typed("yy"), "yank line");
        assert_eq!(describe_typed("2yy"), "yank line 2");
        assert_eq!(describe_typed("ciw"), "change text-object inner word");
        assert_eq!(describe_typed("da\""), "delete text-object around double-quote");
    }

    #[test]
    fn a_parameterized_action_says_what_it_carries() {
        // `find-char` without the character would not tell two mappings
        // apart in a listing.
        assert_eq!(describe_typed("fx"), "find-char 'x'");
        assert_eq!(describe_typed("Tx"), "till-char-backward 'x'");
        assert_eq!(describe_typed("ma"), "set-mark 'a'");
        assert_eq!(describe_typed("ra"), "replace-char 'a'");
    }

    #[test]
    fn the_non_actions_are_named_so_a_refusal_can_say_which() {
        // `::bish map` refuses a right-hand side ending on either rather
        // than storing a mapping that does nothing.
        assert_eq!(describe_typed("d"), "(incomplete)");
        assert_eq!(describe_typed("g"), "(incomplete)");
        assert_eq!(describe_outcome(&KeyOutcome::None), "(unrecognized)");
    }

    #[test]
    fn insert_visual_and_window_actions_are_named_too() {
        assert_eq!(describe_typed("i"), "insert before");
        assert_eq!(describe_typed("A"), "insert line-end");
        assert_eq!(describe_typed("v"), "visual charwise");
        assert_eq!(describe_typed("V"), "visual linewise");
        assert_eq!(describe_typed("<C-w>s"), "window split");
        assert_eq!(describe_typed("p"), "put-after");
        assert_eq!(describe_typed("u"), "undo");
    }

    #[test]
    fn the_q_that_stops_a_recording_is_not_part_of_it() {
        // Reading goes through `next_key`, which records every key it
        // reads -- including the `q` the host is about to interpret as
        // "stop". Left in, replaying the macro re-runs that `q`, starts
        // a fresh recording, and the next keystroke disappears into it
        // as a register name.
        let mut vk = VimKeys::new();
        vk.start_recording('a');
        for k in [Key::Char('x'), Key::Char('x'), Key::Char('q')] {
            vk.record_key(k);
        }
        vk.stop_recording();

        assert!(vk.queue_macro_replay('a', 1));
        let mut replayed = Vec::new();
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            replayed.push(key);
        }
        assert_eq!(replayed, vec![Key::Char('x'), Key::Char('x')], "the stop key is not replayed");
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
    fn pagedown_pageup_keys_alias_to_ctrl_f_ctrl_b() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::PageDown), KeyOutcome::Motion(Motion::PageDown, None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::PageUp), KeyOutcome::Motion(Motion::PageUp, None));
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
        assert_eq!(last(&mut vk, &[Key::Char('3'), Key::Char('g'), Key::Char('g')]), KeyOutcome::Motion(Motion::GotoFirstLine, Some(3)));

        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('g'), Key::Char('_')]), KeyOutcome::Motion(Motion::LineLastNonBlank, None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('g'), Key::Char('e')]), KeyOutcome::Motion(Motion::WordEndBackward, None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('g'), Key::Char('E')]), KeyOutcome::Motion(Motion::WordEndBackwardBig, None));
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
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None));
        // ';' repeats the same direction
        assert_eq!(vk.feed(Key::Char(';')), KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, None));
        // ',' repeats with direction flipped
        assert_eq!(vk.feed(Key::Char(',')), KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: false }, None));
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
        assert_eq!(last(&mut vk, &[Key::Char('m'), Key::Char('a')]), KeyOutcome::Motion(Motion::SetMark('a'), None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('`'), Key::Char('a')]), KeyOutcome::Motion(Motion::GotoMark('a'), None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('\''), Key::Char('a')]), KeyOutcome::Motion(Motion::GotoMarkLine('a'), None));
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
    fn gd_and_gr_emit_their_outcomes_without_disturbing_g_s_other_bindings() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::GotoDefinition(GotoKind::Definition));
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('r')), KeyOutcome::GotoReferences);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('O')), KeyOutcome::DocumentSymbols);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::CodeActions);
        // `a` alone is still append, not this.
        assert_eq!(vk.feed(Key::Char('a')), KeyOutcome::EnterInsert(InsertCmd::After));
        // `r` on its own is still replace-a-character, not this.
        assert_eq!(vk.feed(Key::Char('r')), KeyOutcome::Pending);
        vk.feed(Key::Escape);
        // The `g` family is crowded, and `d` is also the delete
        // operator's own key -- so check the neighbours still work.
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Motion(Motion::GotoFirstLine, None));
        // A bare `d` is still the delete operator, not this.
        assert_eq!(vk.feed(Key::Char('d')), KeyOutcome::Pending);
        vk.feed(Key::Escape);
    }

    // The two neighbours of `gd` that had a free key. Each is its own
    // request, so a `gy` that quietly emitted a plain definition lookup
    // would look like it worked and answer the wrong question.
    #[test]
    fn gy_and_gd_capital_ask_their_own_questions() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::GotoDefinition(GotoKind::TypeDefinition));
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('D')), KeyOutcome::GotoDefinition(GotoKind::Declaration));
        // Neither key lost what it already meant on its own: `y` is
        // still the yank operator, `D` still deletes to end of line.
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        vk.feed(Key::Escape);
        assert!(matches!(vk.feed(Key::Char('D')), KeyOutcome::Operator { .. } | KeyOutcome::OperatorLines { .. }));
        // And `gi` still resumes insert where it left off -- taking it
        // for `textDocument/implementation` is a decision nobody has
        // made yet.
        assert_eq!(vk.feed(Key::Char('g')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('i')), KeyOutcome::EnterInsert(InsertCmd::LastInsertPos));
    }

    // Each kind maps to its own request and says its own name -- the
    // one place a copy-paste slip would silently send `gy` to the
    // definition endpoint.
    #[test]
    fn every_goto_kind_has_its_own_request_and_noun() {
        let kinds = [GotoKind::Definition, GotoKind::TypeDefinition, GotoKind::Implementation, GotoKind::Declaration];
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for kind in kinds {
            let (method, capability) = kind.request();
            assert!(method.starts_with("textDocument/"), "{method}");
            assert!(capability.ends_with("Provider"), "{capability}");
            assert!(!kind.noun().is_empty());
            seen.push((method, capability));
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), kinds.len(), "two kinds share a request");
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
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('z')]), KeyOutcome::Motion(Motion::ScrollCenter, None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('t')]), KeyOutcome::Motion(Motion::ScrollTop, None));
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('b')]), KeyOutcome::Motion(Motion::ScrollBottom, None));
    }

    #[test]
    fn z_prefix_fold_commands() {
        let mut vk = VimKeys::new();
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('o')]), KeyOutcome::Fold(FoldCmd::Open, None));
        assert_eq!(last(&mut vk, &[Key::Char('3'), Key::Char('z'), Key::Char('F')]), KeyOutcome::Fold(FoldCmd::CreateLines, Some(3)));
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('f'), Key::Char('j')]), KeyOutcome::Operator(Op::Fold, Motion::Down, None, None));
        assert_eq!(
            last(&mut vk, &[Key::Char('d'), Key::Char('z'), Key::Char('j')]),
            KeyOutcome::Operator(Op::Delete, Motion::FoldNextStart, None, None)
        );
        assert_eq!(last(&mut vk, &[Key::Char('['), Key::Char('z')]), KeyOutcome::Motion(Motion::FoldStart, None));
        assert_eq!(last(&mut vk, &[Key::Char(']'), Key::Char('z')]), KeyOutcome::Motion(Motion::FoldEnd, None));
        // In Visual mode the selection is the target, so there is no
        // motion to wait for.
        vk.begin_visual(RegisterShape::Line, (0, 0));
        assert_eq!(last(&mut vk, &[Key::Char('z'), Key::Char('f')]), KeyOutcome::Fold(FoldCmd::CreateFromSelection, None));
    }

    // `:noh` is only safe to press because anything that searches again
    // brings the highlight back -- `n` and `*` included, neither of
    // which sets a *new* search.
    #[test]
    fn a_suppressed_search_highlight_comes_back_on_the_next_search() {
        let mut vk = VimKeys::new();
        assert!(vk.search_highlight_on(), "nothing suppressed to begin with");
        vk.suppress_search_highlight();
        assert!(!vk.search_highlight_on());

        // A fresh `/` search.
        for key in [Key::Char('/'), Key::Char('x'), Key::Enter] {
            vk.feed(key);
        }
        assert!(vk.search_highlight_on());

        // `n`, which reuses the last search rather than setting one.
        vk.suppress_search_highlight();
        vk.feed(Key::Char('n'));
        assert!(vk.search_highlight_on(), "`n` brings it back");

        // `N`, and `*` on a word.
        vk.suppress_search_highlight();
        vk.feed(Key::Char('N'));
        assert!(vk.search_highlight_on());
        vk.suppress_search_highlight();
        vk.feed(Key::Char('*'));
        assert!(vk.search_highlight_on());
    }

    #[test]
    fn search_forward_and_repeat() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('/'), Key::Char('f'), Key::Char('o'), Key::Char('o'), Key::Enter];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None));
        assert_eq!(vk.feed(Key::Char('n')), KeyOutcome::Motion(Motion::SearchForward("foo".to_string()), None));
        assert_eq!(vk.feed(Key::Char('N')), KeyOutcome::Motion(Motion::SearchBackward("foo".to_string()), None));
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
        assert_eq!(vk.feed(Key::Enter), KeyOutcome::Motion(Motion::SearchBackward("bar".to_string()), None));
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
    fn search_ctrl_c_cancels_only_the_search() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('/')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Pending);
        assert!(vk.is_search_pending());
        assert_eq!(vk.feed(Key::CtrlC), KeyOutcome::None);
        assert!(!vk.is_search_pending());
        // back to a clean state afterward, same as Escape
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn search_backspace_on_empty_pattern_cancels() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('/')), KeyOutcome::Pending);
        assert!(vk.is_search_pending());
        // Nothing typed yet -- Backspace here should exit search entirely,
        // not silently re-arm an unchanged empty pattern forever.
        assert_eq!(vk.feed(Key::Backspace), KeyOutcome::None);
        assert!(!vk.is_search_pending());
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn search_backspace_on_non_empty_pattern_still_just_edits() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('/')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Pending);
        // First Backspace just edits "x" down to an empty pattern --
        // still pending, not yet a cancel.
        assert_eq!(vk.feed(Key::Backspace), KeyOutcome::Pending);
        assert!(vk.is_search_pending());
        // Now that the pattern is empty, the *next* Backspace cancels.
        assert_eq!(vk.feed(Key::Backspace), KeyOutcome::None);
        assert!(!vk.is_search_pending());
    }

    #[test]
    fn is_search_pending_false_outside_of_search() {
        let mut vk = VimKeys::new();
        assert!(!vk.is_search_pending());
        assert_eq!(vk.feed(Key::Char('2')), KeyOutcome::Pending);
        // Mid-count is pending, but not a *search* pending.
        assert!(!vk.is_search_pending());
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
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Motion(Motion::FindChar { ch: 'x', till: false, forward: true }, Some(2)));
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

    // The status line shows what has been typed toward the command in
    // progress, and for a search that is the pattern itself -- which is
    // the thing being edited. Backspace used to *extend* it with `<BS>`:
    // the transcript is appended to before the key is dispatched, so the
    // pattern lost its character and the display gained three.
    #[test]
    fn a_searchs_display_is_the_pattern_being_typed() {
        let mut vk = VimKeys::new();
        vk.feed(Key::Char('/'));
        vk.feed(Key::Char('a'));
        vk.feed(Key::Char('b'));
        assert_eq!(vk.pending_display(), "/ab");

        vk.feed(Key::Backspace);
        assert_eq!(vk.pending_display(), "/a", "backspace shortens the pattern rather than naming itself");

        // A key the search ignores does not leave a mark either: it is
        // deliberately not an abort (nobody wants a stray Ctrl-F to
        // discard a half-typed pattern), so it must not be mistaken for
        // part of the pattern.
        vk.feed(Key::CtrlF);
        assert_eq!(vk.pending_display(), "/a");

        // And what the search is *for* still matches the display.
        vk.feed(Key::Char('c'));
        assert_eq!(vk.pending_display(), "/ac");
        assert_eq!(vk.feed(Key::Enter), KeyOutcome::Motion(Motion::SearchForward("ac".to_string()), None));

        // A count before the search is part of what was typed, and stays.
        let mut counted = VimKeys::new();
        counted.feed(Key::Char('3'));
        counted.feed(Key::Char('?'));
        counted.feed(Key::Char('x'));
        counted.feed(Key::Char('y'));
        counted.feed(Key::Backspace);
        assert_eq!(counted.pending_display(), "3?x");
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
            ('z', WindowCmd::Zoom),
            ('o', WindowCmd::Zoom),
            ('x', WindowCmd::SwapPane),
            ('T', WindowCmd::BreakPane),
            (' ', WindowCmd::NextLayout),
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
        // `?` is not a window command and is not likely to become one;
        // this test used to use `x`, which then became swap-pane.
        assert_eq!(vk.feed(Key::Char('?')), KeyOutcome::None);
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
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Operator(Op::Yank, Motion::WordForward, None, None));
    }

    #[test]
    fn operator_and_motion_counts_multiply() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('y'), Key::Char('3'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(6), None));
    }

    #[test]
    fn operator_with_only_a_pre_count_or_only_a_post_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('y'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(3), None));

        let mut vk = VimKeys::new();
        let keys = [Key::Char('y'), Key::Char('3'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::WordForward, Some(3), None));
    }

    #[test]
    fn operator_through_a_sub_prefix_motion() {
        // y then f then x -- the 'f' sub-prefix must stay armed as an
        // operator target, not get treated as its own bare motion.
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('y')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('f')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('x')), KeyOutcome::Operator(Op::Yank, Motion::FindChar { ch: 'x', till: false, forward: true }, None, None));
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
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::WordForward, None, Some('a')));

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
        assert_eq!(vk.feed(Key::Char('p')), KeyOutcome::Put { before: false, count: None, register: None });
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
        assert_eq!(vk.feed(Key::Char('?')), KeyOutcome::None);
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Motion(Motion::WordForward, None));
    }

    #[test]
    fn register_prefix_accepts_numbered_and_read_only_register_names() {
        for c in ['0', '5', '9', '.', '%', ':'] {
            let mut vk = VimKeys::new();
            let keys = [Key::Char('"'), Key::Char(c), Key::Char('p')];
            assert_eq!(last(&mut vk, &keys), KeyOutcome::Put { before: false, count: None, register: Some(c) });
        }
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
        assert_eq!(vk.feed(Key::Char('w')), KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, None));
    }

    #[test]
    fn change_operator_plus_motion() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('c')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::Char('e')), KeyOutcome::Operator(Op::Change, Motion::WordEnd, None, None));
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
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, Some('a')));

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
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::TextObject(TextObjectKind::Word, false), None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('d'), Key::Char('a'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Delete, Motion::TextObject(TextObjectKind::Word, true), None, None));
    }

    #[test]
    fn text_object_count_combines_on_either_side() {
        // count before the operator...
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('c'), Key::Char('i'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Change, Motion::TextObject(TextObjectKind::Word, false), Some(2), None));
        // ...and between the operator and the object prefix -- same result.
        let mut vk = VimKeys::new();
        let keys = [Key::Char('c'), Key::Char('2'), Key::Char('i'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Change, Motion::TextObject(TextObjectKind::Word, false), Some(2), None));
    }

    #[test]
    fn text_object_register_prefix_carries_through() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('"'), Key::Char('a'), Key::Char('y'), Key::Char('i'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Yank, Motion::TextObject(TextObjectKind::Word, false), None, Some('a')));
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

    // Ctrl+T asks for the opener from a pane's Normal mode, the way it
    // already does at the prompt -- the gesture should not depend on
    // which of the two you happen to be looking at.
    #[test]
    fn ctrl_t_asks_for_the_opener_and_takes_no_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlT), KeyOutcome::Opener);
        // A count typed before it is spent, not carried: there is no
        // "third opener" to ask for.
        assert_eq!(vk.feed(Key::Char('3')), KeyOutcome::Pending);
        assert_eq!(vk.feed(Key::CtrlT), KeyOutcome::Opener);
        assert_eq!(vk.take_count(), None);
        // And it leaves no half-typed command behind it.
        assert_eq!(vk.feed(Key::Char('j')), KeyOutcome::Motion(Motion::Down, None));
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

    // `o`/`O` in Visual mode are the one place these two keys do not
    // open a line -- see `emit_open_line`'s own note.
    #[test]
    fn o_and_big_o_swap_the_visual_ends_rather_than_opening_a_line() {
        for key in [Key::Char('o'), Key::Char('O')] {
            let mut vk = VimKeys::new();
            assert!(matches!(vk.feed(key), KeyOutcome::OpenLine { .. }), "{key:?} opens a line in Normal mode");
            let mut vk = VimKeys::new();
            vk.begin_visual(RegisterShape::Char, (0, 0));
            assert_eq!(vk.feed(key), KeyOutcome::SwapVisualEnds, "{key:?} in Visual mode");
        }
    }

    #[test]
    fn set_visual_anchor_moves_the_anchor_and_stays_in_visual_mode() {
        let mut vk = VimKeys::new();
        vk.begin_visual(RegisterShape::Char, (1, 2));
        vk.set_visual_anchor((4, 5));
        assert_eq!(vk.visual_anchor(), Some((RegisterShape::Char, (4, 5))));
        assert!(vk.is_visual());
        // Nothing to move when there is no selection, and it must not
        // invent one.
        let mut vk = VimKeys::new();
        vk.set_visual_anchor((4, 5));
        assert_eq!(vk.visual_anchor(), None);
    }

    /// Drives `.` the way a host does: feed the keys, then serve
    /// whatever `.` queued back out through `next_key` and feed those
    /// too, collecting what each resolved to.
    fn feed_then_repeat(keys: &[Key]) -> Vec<KeyOutcome> {
        let mut vk = VimKeys::new();
        for k in keys {
            vk.feed(*k);
        }
        vk.feed(Key::Char('.'));
        let mut out = Vec::new();
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            out.push(vk.feed(key));
        }
        out
    }

    #[test]
    fn dot_replays_the_last_change_and_ignores_everything_that_is_not_one() {
        assert_eq!(feed_then_repeat(&[Key::Char('x')]), vec![KeyOutcome::DeleteCharForward { count: None, register: None }]);
        // `dd` leaves through the double-tap shortcut and `dw` only
        // becomes an operator in the armed-operator block at the very
        // bottom of `feed` -- both have to be recorded anyway.
        assert_eq!(feed_then_repeat(&[Key::Char('d'), Key::Char('d')]), vec![KeyOutcome::Pending, KeyOutcome::OperatorLines(Op::Delete, None, None)]);
        assert_eq!(
            feed_then_repeat(&[Key::Char('d'), Key::Char('w')]),
            vec![KeyOutcome::Pending, KeyOutcome::Operator(Op::Delete, Motion::WordForward, None, None)]
        );
        // A motion is not a change, and neither is a yank or an undo --
        // `.` after any of them repeats the change before it.
        for after in [&[Key::Char('w')][..], &[Key::Char('y'), Key::Char('y')][..], &[Key::Char('u')][..]] {
            let mut keys = vec![Key::Char('x')];
            keys.extend_from_slice(after);
            assert_eq!(feed_then_repeat(&keys), vec![KeyOutcome::DeleteCharForward { count: None, register: None }], "after {after:?}");
        }
        // Nothing changed yet, so there is nothing to repeat.
        assert!(feed_then_repeat(&[Key::Char('w')]).is_empty());
    }

    #[test]
    fn dot_replays_the_text_typed_during_an_insert_excursion_too() {
        let mut vk = VimKeys::new();
        assert!(matches!(vk.feed(Key::Char('i')), KeyOutcome::EnterInsert(_)));
        // What the host's own Insert-mode loop does: read through the
        // same seam until the escape comes back.
        let typed = [Key::Char('h'), Key::Char('i'), Key::Escape];
        let mut feed_from = typed.iter().copied();
        while let Some(key) = vk.next_mapped_key("insert", || Ok(feed_from.next())).unwrap() {
            if key == Key::Escape {
                break;
            }
        }
        vk.feed(Key::Char('.'));
        let mut replayed = Vec::new();
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            replayed.push(key);
        }
        assert_eq!(replayed, vec![Key::Char('i'), Key::Char('h'), Key::Char('i'), Key::Escape]);
    }

    // Drives one Insert-mode excursion the way a host does -- `opening`
    // fed as Normal-mode keys, `typed` read through the insert seam until
    // an escape comes back -- and returns what the host got to insert.
    fn excursion(vk: &mut VimKeys, opening: &str, typed: &str) -> Vec<Key> {
        for c in opening.chars() {
            vk.feed(Key::Char(c));
        }
        let mut feed_from = typed.chars().map(Key::Char).chain([Key::Escape]);
        let mut inserted = Vec::new();
        while let Some(key) = vk.next_mapped_key("insert", || Ok(feed_from.next())).unwrap() {
            if key == Key::Escape {
                break;
            }
            inserted.push(key);
        }
        inserted
    }

    fn replay(vk: &mut VimKeys) -> Vec<Key> {
        let mut replayed = Vec::new();
        while let Some(key) = vk.next_key(|| Ok(None)).unwrap() {
            replayed.push(key);
        }
        replayed
    }

    #[test]
    fn a_counted_insert_types_its_text_that_many_times_and_dot_repeats_it_whole() {
        let mut vk = VimKeys::new();
        assert_eq!(excursion(&mut vk, "3i", "hi"), "hihihi".chars().map(Key::Char).collect::<Vec<_>>());
        vk.feed(Key::Char('.'));
        assert_eq!(replay(&mut vk), [Key::Char('3'), Key::Char('i'), Key::Char('h'), Key::Char('i'), Key::Escape], "the change, count and all");
    }

    #[test]
    fn a_counted_open_line_puts_each_copy_on_a_line_of_its_own() {
        let mut vk = VimKeys::new();
        assert_eq!(excursion(&mut vk, "3o", "x"), [Key::Char('x'), Key::Enter, Key::Char('x'), Key::Enter, Key::Char('x')]);
        // Only for the commands whose count vim reads that way.
        let mut vk = VimKeys::new();
        assert_eq!(excursion(&mut vk, "3s", "x"), [Key::Char('x')], "`3s` changes three characters in vim; the count is not a repeat");
    }

    #[test]
    fn a_count_before_dot_takes_the_place_of_the_one_the_change_had() {
        let mut vk = VimKeys::new();
        for c in "3dd".chars() {
            vk.feed(Key::Char(c));
        }
        vk.feed(Key::Char('2'));
        vk.feed(Key::Char('.'));
        assert_eq!(replay(&mut vk), "2dd".chars().map(Key::Char).collect::<Vec<_>>());
        vk.feed(Key::Char('.'));
        assert_eq!(replay(&mut vk), "3dd".chars().map(Key::Char).collect::<Vec<_>>(), "and a plain `.` still repeats it as it was made");
    }

    #[test]
    fn queue_keys_serves_them_in_order_ahead_of_a_real_read() {
        let mut vk = VimKeys::new();
        vk.queue_keys("dd".chars().map(Key::Char));
        assert_eq!(vk.next_key(|| Ok(Some(Key::Char('Q')))).unwrap(), Some(Key::Char('d')));
        assert_eq!(vk.next_key(|| Ok(Some(Key::Char('Q')))).unwrap(), Some(Key::Char('d')));
        assert_eq!(vk.next_key(|| Ok(Some(Key::Char('Q')))).unwrap(), Some(Key::Char('Q')));
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
    fn o_and_capital_o_resolve_to_open_line() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('o')), KeyOutcome::OpenLine { above: false });
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('O')), KeyOutcome::OpenLine { above: true });
    }

    #[test]
    fn open_line_discards_a_leading_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('o')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OpenLine { above: false });
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
    fn greater_and_less_arm_indent_outdent_resolved_by_a_motion() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('>'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Indent, Motion::WordForward, None, None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('<'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Outdent, Motion::WordForward, None, None));
    }

    #[test]
    fn greater_greater_and_less_less_double_tap_resolve_to_operator_lines() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('>'), Key::Char('>')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Indent, Some(3), None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('<'), Key::Char('<')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::OperatorLines(Op::Outdent, None, None));
    }

    #[test]
    fn u_and_ctrl_r_resolve_to_undo_redo_with_count() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::Char('u')), KeyOutcome::Undo(None));
        let mut vk = VimKeys::new();
        assert_eq!(vk.feed(Key::CtrlR), KeyOutcome::Redo(None));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('3'), Key::Char('u')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Undo(Some(3)));
        let mut vk = VimKeys::new();
        let keys = [Key::Char('5'), Key::CtrlR];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Redo(Some(5)));
    }

    #[test]
    fn g_minus_and_g_plus_resolve_to_undo_seq_with_count() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('-')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::UndoSeq { forward: false, count: None });
        let mut vk = VimKeys::new();
        let keys = [Key::Char('g'), Key::Char('+')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::UndoSeq { forward: true, count: None });
        // Count typed before the leading `g` -- `self.count` survives
        // entering Pending::G untouched, same as gJ/gu{motion}'s own count
        // handling.
        let mut vk = VimKeys::new();
        let keys = [Key::Char('4'), Key::Char('g'), Key::Char('-')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::UndoSeq { forward: false, count: Some(4) });
    }

    #[test]
    fn gu_text_object_and_counts_combine_like_any_other_operator() {
        let mut vk = VimKeys::new();
        let keys = [Key::Char('2'), Key::Char('g'), Key::Char('U'), Key::Char('i'), Key::Char('w')];
        assert_eq!(last(&mut vk, &keys), KeyOutcome::Operator(Op::Uppercase, Motion::TextObject(TextObjectKind::Word, false), Some(2), None));
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
        assert_eq!(last(&mut vk, &keys), KeyOutcome::AddSurround { target: SurroundTarget::Motion(Motion::WordForward, Some(3)), ch: '"' });
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

    #[test]
    fn start_stop_recording_round_trips_through_macros() {
        let mut vk = VimKeys::new();
        assert_eq!(vk.is_recording(), None);
        vk.start_recording('a');
        assert_eq!(vk.is_recording(), Some('a'));
        vk.record_key(Key::Char('j'));
        vk.record_key(Key::Char('k'));
        vk.stop_recording();
        assert_eq!(vk.is_recording(), None);
        // Nothing to replay it with yet at this layer (queue_macro_replay
        // is exercised separately below) -- this just confirms the
        // recorded keys actually landed in `macros`.
        assert!(vk.queue_macro_replay('a', 1));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('j')));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('k')));
    }

    #[test]
    fn uppercase_register_appends_to_the_existing_macro() {
        let mut vk = VimKeys::new();
        vk.start_recording('a');
        vk.record_key(Key::Char('x'));
        vk.stop_recording();
        vk.start_recording('A');
        vk.record_key(Key::Char('y'));
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('x')));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('y')));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), None);
    }

    #[test]
    fn queue_macro_replay_repeats_count_times_and_supports_at_at() {
        let mut vk = VimKeys::new();
        vk.start_recording('a');
        vk.record_key(Key::Char('x'));
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 3));
        for _ in 0..3 {
            assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('x')));
        }
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), None);
        // `@@` replays whatever `@` last targeted.
        assert!(vk.queue_macro_replay('@', 1));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('x')));
    }

    #[test]
    fn queue_macro_replay_is_a_no_op_for_an_empty_or_unset_register() {
        let mut vk = VimKeys::new();
        assert!(!vk.queue_macro_replay('z', 1)); // never recorded
        assert!(!vk.queue_macro_replay('@', 1)); // @@ with nothing yet
        vk.start_recording('a');
        vk.stop_recording(); // recorded, but empty
        assert!(!vk.queue_macro_replay('a', 1));
    }

    #[test]
    fn next_key_serves_the_replay_queue_before_a_real_read_and_never_records_it() {
        let mut vk = VimKeys::new();
        vk.start_recording('a');
        vk.record_key(Key::Char('x'));
        vk.stop_recording();
        assert!(vk.queue_macro_replay('a', 1));

        // Recording a *second* macro while the first's replay is queued:
        // the replayed 'x' must not land in this new recording, only a
        // real read would.
        vk.start_recording('b');
        assert_eq!(vk.next_key(|| Ok(Some(Key::Char('Q')))).unwrap(), Some(Key::Char('x')));
        vk.stop_recording();
        assert!(!vk.queue_macro_replay('b', 1)); // nothing real was ever read while 'b' recorded

        assert_eq!(vk.next_key(|| Ok(Some(Key::Char('Q')))).unwrap(), Some(Key::Char('Q')));
    }

    #[test]
    fn next_key_records_a_nested_macro_invocation_literally_not_its_expansion() {
        // Recording 'a' as: type '@', type 'b' (replays 'b', literally
        // just those two keystrokes recorded into 'a' -- not whatever
        // 'b' itself expands to, matching real vim).
        let mut vk = VimKeys::new();
        vk.start_recording('b');
        vk.record_key(Key::Char('z'));
        vk.stop_recording();

        vk.start_recording('a');
        vk.record_key(Key::Char('@'));
        vk.record_key(Key::Char('b'));
        assert!(vk.queue_macro_replay('b', 1));
        // Draining 'b's expansion here must not touch 'a's recording.
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('z')));
        vk.stop_recording();

        assert!(vk.queue_macro_replay('a', 1));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('@')));
        assert_eq!(vk.next_key(|| Ok(None)).unwrap(), Some(Key::Char('b')));
    }

    #[test]
    fn is_idle_except_count_ignores_count_but_not_pending_or_operator() {
        let mut vk = VimKeys::new();
        assert!(vk.is_idle_except_count());
        vk.feed(Key::Char('3'));
        assert!(vk.is_idle_except_count());
        assert_eq!(vk.take_count(), Some(3));
        assert_eq!(vk.take_count(), None);

        let mut vk = VimKeys::new();
        vk.feed(Key::Char('f')); // mid a sub-prefix
        assert!(!vk.is_idle_except_count());

        let mut vk = VimKeys::new();
        vk.feed(Key::Char('d')); // armed operator
        assert!(!vk.is_idle_except_count());
    }
}
