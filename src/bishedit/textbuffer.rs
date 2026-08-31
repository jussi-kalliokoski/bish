// The mutable multi-line buffer -- the "once real editing lands" half of
// `Buffer`'s own module doc comment. Everything built before this
// (`editor.rs`'s `LineBuffer`) only ever mutated a single flat `Vec<char>`
// line; a real file has many. Navigation goes through the shared `Buffer`
// trait exactly like `ScreenBuffer`/`LineBuffer` already do; mutation is
// deliberately *not* added to that trait (mutation has never gone through
// it, even for `LineBuffer` -- keeping it bespoke per concrete type is the
// established pattern here, not a shortcut).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::lint;
use super::motion;
use super::registers::{RegisterShape, RegisterValue, Registers};
use super::snippet;
use super::undo::UndoTree;
use super::Buffer;

// One placeholder of a live `abbr` snippet, in this buffer's own
// (line, column) space. `active` is the one being typed into, which the
// renderer marks differently from the rest -- see
// `fileeditor::build_editor_frame`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnippetHole {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub active: bool,
}

pub struct TextBuffer {
    // Always at least one line, matching a real file (an empty file is
    // one empty line, same as vim's own "new, empty buffer" convention --
    // see `new_unnamed`/`delete_range`'s own Linewise branch, which
    // restores this invariant after deleting every line).
    lines: Vec<Vec<char>>,
    cursor: (usize, usize),
    vtop: usize,
    vheight: usize,
    // Horizontal counterpart to vtop -- how many columns of the current
    // line are scrolled off to the left, so `set_cursor`/motions moving
    // past whatever width the pane last rendered don't just get clipped
    // off-screen with no way back (see Buffer::viewport_left's own doc
    // comment). Kept in sync by fileeditor::scroll_to_show_cursor,
    // exactly the way vtop is.
    hleft: usize,
    marks: HashMap<char, (usize, usize)>,
    // Visual mode's own committed selections -- same field, same shape,
    // same reasoning as `repl.rs`'s `ScreenBuffer::selections`.
    pub selections: Vec<motion::MotionRange>,
    // A live `abbr` snippet's own placeholders, for whoever draws this
    // buffer -- same field, same "view state the buffer carries so every
    // frame showing it agrees" reasoning as `selections` just above,
    // which also gets a detached editor pane's frozen frame right for
    // free. Written by fileeditor::run_insert_mode while a snippet is
    // live and cleared the moment it ends; empty every other time.
    pub snippet_holes: Vec<SnippetHole>,
    // `:diag`'s own last result (see fileeditor::diagnose_buffer) -- rides
    // along with the buffer exactly like `selections` does (survives a
    // Ctrl+Space detach/reattach, since both live on the one thing that
    // does), cleared by `:diag clear` or implicitly by any real edit (see
    // insert_text/delete_range/join_lines below): a lint::Diagnostic's
    // start/end are char offsets into this buffer's *current* text, so
    // keeping a stale one around past the edit that invalidated its
    // position would just be showing the user a lie.
    pub diagnostics: Vec<lint::Diagnostic>,
    // `:git blame`'s own toggle state (see fileeditor::toggle_git_blame,
    // and GUTTER_COLUMNS's blame column, which is what actually reads
    // this) -- `None` means off (the gutter's blame column collapses to
    // zero width entirely), `Some` is one entry per buffer line, indexed
    // the same way `lines` is. The *inner* `None` is a line git had
    // nothing to say about: typed since the revision being blamed, or
    // simply not in it (see fileeditor::toggle_git_blame, which lines the
    // two up rather than assuming they match). Cleared on any real edit
    // for the same reason `diagnostics` is: a line-indexed snapshot of
    // "who last touched this line" goes stale the instant a line shifts
    // or its content changes, and re-running `git blame` is the caller's
    // job (`:git blame` again), not something a single-line edit could
    // patch up correctly on its own.
    // How this view lays out a line wider than the pane -- vim's `wrap`
    // family, set from bishopt by whoever is driving this buffer (see
    // repl.rs's `wrap_options`). A *view* setting rather than a document
    // one, which is why it lives beside `vtop`/`hleft` rather than
    // anywhere near the text.
    pub wrap: crate::bishedit::wrap::Options,
    // The delimiter this buffer's columns line up on, for a language
    // that has a tabular form and whose language the `tabular` bishopt's
    // glob matches. `None` for everything else, which is almost every
    // buffer. A *display* setting like `wrap`: the text is untouched,
    // only where padding is drawn between it changes.
    pub tabular: Option<crate::bishedit::tabular::Style>,
    // The `hyperlinks` bishopt: whether links are drawn as real OSC 8
    // hyperlinks. Only affects rendering -- the link spans are computed
    // either way, since the styling that marks them comes from the same
    // pass.
    pub hyperlinks: bool,
    // The `relativenumber` bishopt: whether the gutter numbers each line
    // by its distance from the cursor's instead of absolutely.
    pub relativenumber: bool,
    /// The `cursorshape` bishopt: whether the terminal's cursor changes
    /// shape to show the mode.
    /// This session's resolved `ui_col_*` colours (see theme.rs) --
    /// carried on the buffer for the same reason `wrap` and `tabular`
    /// are: every frame showing this buffer has to agree, including a
    /// detached pane's frozen one, and the gutter's own renderers are
    /// plain functions of a `TextBuffer`.
    pub colors: Option<crate::theme::UiColors>,
    pub cursorshape: bool,
    /// The `mouse` bishopt, carried here so the frame that draws this
    /// buffer and the loop that reads keys for it agree.
    pub mouse: bool,
    /// How this buffer indents: `expandtab` off inserts a literal tab,
    /// `shiftwidth` is what `>>` shifts by and what Tab advances to,
    /// `tabstop` is how wide a literal tab draws.
    pub expandtab: bool,
    pub shiftwidth: usize,
    pub tabstop: usize,
    /// Whether `:w` strips whitespace from the end of every line.
    pub trim_trailing_whitespace: bool,
    /// Whether the file on disk ends in a newline. Detected from what
    /// was loaded, so a file without one keeps not having one -- and
    /// overridden by `fixendofline`/`insert_final_newline`.
    pub final_newline: bool,
    /// The line ending this file is written back with. Detected on load,
    /// so opening and saving a CRLF file leaves it a CRLF file.
    pub eol: crate::editorconfig::Eol,
    // Which visual row of `vtop`'s own line the viewport starts at. Only
    // ever non-zero with wrapping on, and only for a line tall enough to
    // exceed the pane by itself -- without it, a minified file would
    // scroll to the right line and then be unable to reach the cursor
    // inside it.
    vtop_sub: usize,
    pub blame: Option<Vec<Option<crate::git::BlameLine>>>,
    // `:git diff`'s own toggle state -- same shape/lifecycle as `blame`
    // just above (`None` off, gutter column collapses to zero width;
    // `Some` on), except sparse: only lines crate::git::diff actually
    // marked appear as keys (0-indexed, matching `lines`/`blame`), most
    // lines have no entry at all rather than an explicit "unchanged"
    // marker.
    pub diff: Option<std::collections::HashMap<usize, crate::git::DiffMark>>,
    // `:dbg`'s own breakpoint set (see GUTTER_COLUMNS's breakpoint column
    // in fileeditor.rs, which is what actually reads this) -- 1-based
    // line numbers, matching how the debugger itself reports lines.
    // Empty (the common case: an ordinary `e`-opened buffer never
    // touches this) collapses the gutter column to zero width, same
    // convention `blame`/`diff` already use. Deliberately *not* cleared
    // on edit the way diagnostics/blame/diff are -- a buffer with any
    // breakpoints is always `readonly` too (see that field's own doc
    // comment) for the whole time it could have any set, so there's
    // never an edit to invalidate a line-indexed breakpoint set against
    // in the first place.
    pub breakpoints: std::collections::BTreeSet<usize>,
    // Set while a `:dbg` session is attached to this buffer (repl.rs) --
    // every mutating `KeyOutcome` arm in run_normal_mode_navigation
    // already gates itself on `NavBuffer::Editable(tb)` individually (no
    // single chokepoint -- see that function's own doc comment), so this
    // is consulted as one extra condition at each of those sites rather
    // than needing a new `NavBuffer` variant: same buffer type, same
    // rendering/navigation/command-mode, just mutation refused. Doesn't
    // affect `dirty`/`save` at all -- a buffer opened for debugging was
    // never dirty to begin with (`:dbg` itself refuses to attach to one
    // that is), and nothing here can make it dirty since nothing can
    // mutate it while this is set.
    readonly: bool,
    // `u`/`Ctrl-R` -- a real branching tree, not a linear undo/redo stack
    // (see bishedit::undo's own module doc comment for why). Rides along
    // with the buffer exactly like `selections`/`diagnostics` (survives a
    // Ctrl+Space detach/reattach), seeded with this buffer's own starting
    // content in `new_unnamed`/`open` below. Checkpointed by
    // `checkpoint_undo` -- see its own doc comment for exactly when that
    // gets called and why that's what defines one undo-able "group".
    undo: UndoTree<Vec<Vec<char>>>,
    // The undo-tree node id that was on disk as of the last successful
    // `save` (or the buffer's own starting content, if never saved) --
    // `undo()`/`redo()`/`time_travel` compare against this to decide
    // whether landing on a given node means "this exact content is
    // already on disk," clearing `dirty` precisely the way real vim's own
    // undo-tree-aware `modified` flag does. Ordinary edits (insert_text/
    // delete_range/join_lines) don't consult this at all -- they always
    // set `dirty = true` directly, which is already correct for them (an
    // edit either just happened or it didn't; there's no "did this
    // particular edit happen to reproduce the saved content" question
    // worth asking there the way there is for undo/redo/time-travel,
    // which can legitimately land back exactly on it).
    saved_node: usize,
    dirty: bool,
    // Bumped by `content_changed` on every real edit, and never reset --
    // a monotonic "which revision of this buffer is this" stamp, as
    // against `dirty`'s "does it differ from disk" (which undo can and
    // does clear again).
    //
    // Exists for anything that computes something *from* the text and
    // gets an answer back later, when the text may have moved on: a
    // language server's diagnostics arrive asynchronously and carry the
    // version they describe, so a reply about a revision this buffer has
    // already left can be dropped instead of drawn at offsets that no
    // longer mean anything. `dirty` cannot answer that question and
    // `diagnostics.clear()` alone can't either -- clearing says the old
    // answer is gone, not which new one is still worth waiting for.
    version: u64,
    path: Option<PathBuf>,
}

impl TextBuffer {
    pub fn new_unnamed(vheight: usize) -> TextBuffer {
        let lines = vec![Vec::new()];
        TextBuffer {
            undo: UndoTree::new(lines.clone(), (0, 0)),
            saved_node: 0,
            lines,
            cursor: (0, 0),
            vtop: 0,
            vheight: vheight.max(1),
            hleft: 0,
            marks: HashMap::new(),
            selections: Vec::new(),
            wrap: crate::bishedit::wrap::Options::default(),
            tabular: None,
            hyperlinks: true,
            relativenumber: false,
            colors: None,
            cursorshape: true,
            mouse: true,
            expandtab: true,
            shiftwidth: crate::bishedit::vimkeys::INDENT_WIDTH,
            tabstop: crate::bishedit::vimkeys::INDENT_WIDTH,
            trim_trailing_whitespace: false,
            final_newline: true,
            eol: crate::editorconfig::Eol::Lf,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            breakpoints: std::collections::BTreeSet::new(),
            readonly: false,
            dirty: false,
            version: 0,
            path: None,
        }
    }

    // A nonexistent path opens as a fresh unnamed-but-pathed buffer --
    // vim's own ":e newfile" behavior (the file is created on first
    // `:w`, not on open).
    pub fn open(path: &Path, vheight: usize) -> io::Result<TextBuffer> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(TextBuffer::from_text(path, &text, vheight))
    }

    // A buffer holding `text` but named `path` -- for content that has a
    // path worth showing and a place worth saving to, but doesn't come
    // from reading that path directly. Today that's compressed content
    // (a gzip'd file, a member inside a zip), which is why every caller
    // also marks the result readonly; see fileeditor::compressed_text.
    pub fn from_text(path: &Path, text: &str, vheight: usize) -> TextBuffer {
        // What this file's own line ending is, before anything is
        // normalized away -- remembered so `save` writes the same one
        // back. Without it, opening and saving a CRLF file silently
        // rewrote every line of it.
        let eol = detect_eol(text);
        // Normalized to "\n" for everything above this line: the buffer,
        // every motion, every highlighter and every offset in this
        // codebase assume one character per line break, and a `\r`
        // riding along on the end of each line would be a character in
        // the buffer that the file's own text does not have.
        let normalized = match eol {
            crate::editorconfig::Eol::Lf => std::borrow::Cow::Borrowed(text),
            crate::editorconfig::Eol::Crlf => std::borrow::Cow::Owned(text.replace("\r\n", "\n")),
            crate::editorconfig::Eol::Cr => std::borrow::Cow::Owned(text.replace('\r', "\n")),
        };
        // A trailing newline is the normal case -- stripped here so it
        // doesn't show up as a phantom trailing empty line; *whether* it
        // was there is remembered instead, so a file without one keeps
        // not having one.
        let final_newline = normalized.is_empty() || normalized.ends_with('\n');
        let text = normalized.strip_suffix('\n').unwrap_or(&normalized);
        let lines: Vec<Vec<char>> = text.split('\n').map(|l| l.chars().collect()).collect();
        let lines = if lines.is_empty() { vec![Vec::new()] } else { lines };
        TextBuffer {
            undo: UndoTree::new(lines.clone(), (0, 0)),
            saved_node: 0,
            lines,
            cursor: (0, 0),
            vtop: 0,
            vheight: vheight.max(1),
            hleft: 0,
            marks: HashMap::new(),
            selections: Vec::new(),
            wrap: crate::bishedit::wrap::Options::default(),
            tabular: None,
            hyperlinks: true,
            relativenumber: false,
            colors: None,
            cursorshape: true,
            mouse: true,
            eol,
            final_newline,
            expandtab: true,
            shiftwidth: crate::bishedit::vimkeys::INDENT_WIDTH,
            tabstop: crate::bishedit::vimkeys::INDENT_WIDTH,
            trim_trailing_whitespace: false,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            breakpoints: std::collections::BTreeSet::new(),
            readonly: false,
            dirty: false,
            version: 0,
            path: Some(path.to_path_buf()),
        }
    }

    // See `vtop_sub`. Cleared whenever the top line itself moves, since
    // an offset into a different line means nothing.
    pub fn viewport_sub(&self) -> usize {
        self.vtop_sub
    }

    pub fn set_viewport_sub(&mut self, sub: usize) {
        self.vtop_sub = sub;
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// This buffer's current revision -- see the `version` field.
    pub fn version(&self) -> u64 {
        self.version
    }

    // The one thing every real edit does: bump the revision, and drop
    // everything derived from the old text. Called by insert_text/
    // delete_range/join_lines and by restore_snapshot (undo/redo/time
    // travel), which are between them the only four places this
    // buffer's content actually changes.
    //
    // Deliberately does not touch `dirty`: the three edit methods set it
    // unconditionally, while restore_snapshot's is save-aware (landing
    // back on the node that's on disk clears it). That difference is
    // real, so it stays at the call sites rather than being averaged
    // into something wrong for one of them here.
    fn content_changed(&mut self) {
        self.version += 1;
        // Positions in any of these are char offsets into the text that
        // just changed, so keeping them would be showing a lie.
        self.diagnostics.clear();
        self.blame = None;
        self.diff = None;
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    // `:dbg`'s own on/off switch (repl.rs) -- see `readonly`'s own field
    // doc comment.
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    // Commits the buffer's current content/cursor as a new undo-tree node
    // if it differs from whatever's already there -- a no-op otherwise
    // (see UndoTree::checkpoint's own doc comment for exactly what "no-op"
    // means and why). Called by repl.rs's `render_nav_frame`, once per
    // top-level key dispatch in the unified Normal-mode loop -- that one
    // call site is what actually defines an undo "group": an entire
    // Insert-mode session (many individual `insert_text` calls) only
    // reaches `render_nav_frame` once, after the whole session ends, so it
    // collapses into a single checkpoint/undo step, with no changes needed
    // to any of the individual mutation call sites themselves.
    pub fn checkpoint_undo(&mut self) {
        self.undo.checkpoint(&self.lines, self.cursor);
    }

    // `u`: moves to the parent node and restores its content/cursor.
    // `false` (buffer untouched) at the tree's root -- there's nothing
    // further back than the content this buffer started with.
    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.undo() else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // `Ctrl-R`: moves to the most recently created child and restores its
    // content/cursor -- see UndoTree::redo's own doc comment for exactly
    // which branch that is. `false` (buffer untouched) if the current node
    // has no children (nothing to redo from here).
    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.undo.redo() else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // `g-`/`g+`: walks the undo tree's own flat creation history rather
    // than parent/child edges -- see UndoTree::time_travel_back/forward's
    // own doc comment for why that reaches branches undo/redo alone
    // can't. `forward: false` is `g-`, `true` is `g+`. `false` (buffer
    // untouched) at either end of that history.
    pub fn time_travel(&mut self, forward: bool) -> bool {
        let result = if forward { self.undo.time_travel_forward() } else { self.undo.time_travel_back() };
        let Some(snap) = result else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // Shared tail of undo/redo/time_travel: splices a snapshot's own
    // content/cursor back into the buffer and updates everything that
    // depends on "what does the buffer actually contain right now."
    fn restore_snapshot(&mut self, content: Vec<Vec<char>>, cursor: (usize, usize)) {
        self.lines = content;
        self.cursor = cursor;
        // Positions may no longer be valid -- same reasoning insert_text/
        // delete_range/join_lines already apply for any real edit.
        self.content_changed();
        // Save-aware: landing back exactly on the node that was on disk
        // as of the last `:w` clears `dirty`, matching real vim's own
        // undo-tree-aware `modified` flag -- see `saved_node`'s own doc
        // comment.
        self.dirty = self.undo.current_id() != self.saved_node;
    }

    // Writes every line joined by "\n", plus one trailing "\n" (matching
    // a real text file, and `open`'s own inverse -- see its doc comment).
    // `path` overrides `self.path` for this write and, if this buffer had
    // none yet, becomes the buffer's own path afterward -- vim's own
    // ":w newname" behavior on an unnamed buffer.
    // The buffer's own full text, `\n`-joined with a trailing newline --
    // `save`'s own inverse (what actually gets written to disk). Also
    // used by anything that needs an ordinary, re-parseable source
    // string rather than this buffer's line-based representation --
    // e.g. `K`-hover's own doc-comment lookup (repl.rs/docs.rs), which
    // parses the *live* in-memory buffer so a not-yet-saved edit is
    // reflected immediately, rather than requiring a `:w` first.
    /// The bytes this buffer writes to disk: its own text with *this
    /// file's* line ending, trailing whitespace trimmed if asked, and a
    /// final newline only if it should have one.
    ///
    /// Separate from `text()` on purpose. `text()` is what every
    /// highlighter, linter and span offset in this codebase reads, and
    /// it is always `\n`-separated with one trailing newline -- making
    /// *that* depend on a file's line ending would put a `\r` into every
    /// offset in the editor.
    pub fn on_disk_text(&self) -> String {
        let mut lines: Vec<String> = self.lines.iter().map(|l| l.iter().collect::<String>()).collect();
        if self.trim_trailing_whitespace {
            for line in lines.iter_mut() {
                let trimmed = line.trim_end();
                if trimmed.len() != line.len() {
                    *line = trimmed.to_string();
                }
            }
        }
        let mut out = lines.join(self.eol.text());
        if self.final_newline {
            out.push_str(self.eol.text());
        }
        out
    }

    pub fn text(&self) -> String {
        let mut text: String = self.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n");
        text.push('\n');
        text
    }

    pub fn save(&mut self, path: Option<&Path>) -> io::Result<()> {
        let target = path.or(self.path.as_deref()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No file name"))?;
        // Vim's own rule, and the reason this guard is here rather than
        // at the `:w` command: read-only is about *this file*, not about
        // the text, so `:w SOMEWHERE-ELSE` still works -- which is how
        // you get a zip member or a gzip'd file back out as an ordinary
        // file. Enforced at the one place that writes, so no caller can
        // forget it; with compressed buffers that matters, since writing
        // one back to its own path would replace an archive with its own
        // decompressed contents.
        if self.readonly && Some(target) == self.path.as_deref() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "buffer is read-only"));
        }
        std::fs::write(target, self.on_disk_text())?;
        if self.path.is_none() {
            self.path = Some(target.to_path_buf());
        }
        self.dirty = false;
        // Remembers exactly what's now on disk -- see `saved_node`'s own
        // doc comment. Safe to read here (rather than needing its own
        // fresh checkpoint first): `self.undo.current()` and `self.lines`
        // are always already in sync by the time any new top-level key
        // (including the `:w` that reached this call) starts being
        // handled -- repl.rs's own `render_nav_frame` checkpoints after
        // *every* one, before the next key is ever read.
        self.saved_node = self.undo.current_id();
        Ok(())
    }

    // Splits `text` on '\n' and splices it into the buffer at `at`,
    // joining/splitting lines as needed -- the one real new primitive
    // this whole feature depends on: nothing before this ever inserted
    // text that could itself contain newlines into a multi-line buffer.
    // Returns the cursor position right after the inserted text (vim's
    // own convention -- see `apply_put`'s own doc comment in vimkeys.rs
    // for the single-line precedent this generalizes).
    pub fn insert_text(&mut self, at: (usize, usize), text: &str) -> (usize, usize) {
        if text.is_empty() {
            return at;
        }
        let row = at.0.min(self.lines.len().saturating_sub(1));
        let parts: Vec<&str> = text.split('\n').collect();
        let line = std::mem::take(&mut self.lines[row]);
        let col = at.1.min(line.len());
        let before = line[..col].to_vec();
        let after = line[col..].to_vec();

        let new_pos = if parts.len() == 1 {
            let mut new_line = before;
            new_line.extend(parts[0].chars());
            let new_col = new_line.len();
            new_line.extend(after);
            self.lines[row] = new_line;
            (row, new_col)
        } else {
            let mut new_lines: Vec<Vec<char>> = Vec::with_capacity(parts.len());
            let mut first = before;
            first.extend(parts[0].chars());
            new_lines.push(first);
            for part in &parts[1..parts.len() - 1] {
                new_lines.push(part.chars().collect());
            }
            let mut last: Vec<char> = parts[parts.len() - 1].chars().collect();
            let new_col = last.len();
            last.extend(after);
            new_lines.push(last);
            let new_row = row + parts.len() - 1;
            self.lines.splice(row..=row, new_lines);
            (new_row, new_col)
        };
        self.dirty = true;
        self.content_changed();
        self.cursor = new_pos;
        // `.` -- vim's own "position of the last change" mark (`` `. ``),
        // set automatically by every mutation here rather than at each of
        // fileeditor.rs's own many call sites -- these three methods are
        // the only places a real edit actually happens (and, per
        // `diagnostics`'s own doc comment, the only places a stale
        // diagnostic's position could actually go wrong).
        self.marks.insert('.', new_pos);
        new_pos
    }

    // Removes a `MotionRange` that may span several real lines (joining
    // the two cut ends into one line), returning the removed text.
    // `motion::extract_text`/`motion::motion_range` already resolve a
    // range correctly across multiple lines (built for `ScreenBuffer`'s
    // own y{motion} originally) -- this is the missing mutating
    // counterpart. Row indices are clamped to the buffer's own *current*
    // bounds up front, so a stale/overlapping range (multiple Visual
    // selections deleted back-to-back -- see `delete_selections` below)
    // degrades gracefully instead of panicking.
    pub fn delete_range(&mut self, range: &motion::MotionRange) -> String {
        let last_row = self.lines.len().saturating_sub(1);
        let range = motion::MotionRange { shape: range.shape, from: (range.from.0.min(last_row), range.from.1), to: (range.to.0.min(last_row), range.to.1) };
        let text = motion::extract_text(&*self, &range);
        match range.shape {
            motion::MotionShape::Linewise => {
                self.lines.drain(range.from.0..=range.to.0);
                if self.lines.is_empty() {
                    self.lines.push(Vec::new());
                }
                let row = range.from.0.min(self.lines.len() - 1);
                self.cursor = (row, 0);
            }
            _ => {
                let end_col = if range.shape == motion::MotionShape::Inclusive { range.to.1 + 1 } else { range.to.1 };
                let last_line = &self.lines[range.to.0];
                let after: Vec<char> = last_line.get(end_col.min(last_line.len())..).map(|s| s.to_vec()).unwrap_or_default();
                let first_line = &self.lines[range.from.0];
                let mut joined: Vec<char> = first_line[..range.from.1.min(first_line.len())].to_vec();
                joined.extend(after);
                self.lines.splice(range.from.0..=range.to.0, std::iter::once(joined));
                let row = range.from.0;
                self.cursor = (row, range.from.1.min(self.lines[row].len()));
            }
        }
        self.dirty = true;
        self.content_changed();
        self.marks.insert('.', self.cursor);
        text
    }

    // `J`/`gJ`: joins `count` lines (minimum 2, matching vim -- a bare `J`
    // or `1J` both just join the current line with the next one) starting
    // at the cursor's own line. `with_space` selects vim's default
    // whitespace-aware join (strips each joined-in line's own leading
    // whitespace and inserts a single space, unless the current line
    // already ends in whitespace, the joined-in line is empty, or it
    // starts with ')') vs. `gJ`'s raw concatenation. Returns whether
    // anything was actually joined (false at the last line, matching
    // every other buffer command's own "nothing happened" signal).
    // Cursor lands at the last join's own boundary, matching vim.
    pub fn join_lines(&mut self, count: usize, with_space: bool) -> bool {
        let (row, _) = self.cursor;
        let available = self.lines.len().saturating_sub(1).saturating_sub(row);
        let joins = count.max(2).saturating_sub(1).min(available);
        if joins == 0 {
            return false;
        }
        let mut join_col = self.lines[row].len();
        for _ in 0..joins {
            let mut next = self.lines.remove(row + 1);
            if with_space {
                let leading_ws = next.iter().take_while(|c| c.is_whitespace()).count();
                next.drain(0..leading_ws);
                let cur_ends_blank = self.lines[row].last().is_none_or(|c| c.is_whitespace());
                let next_starts_close_paren = next.first() == Some(&')');
                join_col = self.lines[row].len();
                if !cur_ends_blank && !next.is_empty() && !next_starts_close_paren {
                    self.lines[row].push(' ');
                }
            } else {
                join_col = self.lines[row].len();
            }
            self.lines[row].extend(next);
        }
        self.cursor = (row, join_col.min(self.lines[row].len().saturating_sub(1)));
        self.dirty = true;
        self.content_changed();
        self.marks.insert('.', self.cursor);
        true
    }

    // Visual mode's own `y`: every selection, concatenated with no
    // separator -- same rule `editor.rs`'s own `yank_selections_line`/
    // repl.rs's `yank_selections` already establish (a `Linewise` part
    // already ends in "\n", so it naturally lands on its own line).
    pub fn yank_selections(&self, registers: &mut Registers, register: Option<char>) {
        if self.selections.is_empty() {
            return;
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            if range.shape == motion::MotionShape::Linewise {
                shape = RegisterShape::Line;
            }
        }
        registers.record_yank(register, RegisterValue { text, shape });
    }

    // Visual mode's own `d`: removes every selection, writing the
    // concatenated deleted text to a register first (vim's own "delete
    // always yanks" rule). Selections are removed highest-position
    // first (`(line, col)` ordered, `(usize, usize)`'s own `Ord` is
    // already exactly that lexicographic order) so removing a later one
    // never shifts a still-pending earlier one's own coordinates --
    // same reasoning `editor.rs`'s own `delete_selections` already
    // established for a single line, just ordered by position instead
    // of a bare column. `delete_range`'s own defensive row-clamping
    // (see its doc comment) covers the rest against any pathological
    // overlap between selections.
    //
    // Returns every selection's own resulting gap position (ascending,
    // empty iff there was nothing to delete) rather than just a bool --
    // `d`'s own caller only cares whether that's empty, but `c`'s
    // (repl.rs's Key::Char('c') arm) needs every one of them: the exact
    // same "removed highest-first" rule that keeps the *lowest*
    // position's own coordinates valid afterward (the only one the old
    // bool-returning version exposed, as this buffer's own new cursor)
    // applies identically to *every* lower position relative to
    // whichever one is currently being removed, not just the very
    // lowest overall -- so each of these is just as valid a post-
    // deletion insertion point, letting a multi-selection `c` type the
    // same replacement into every one of them instead of only the first.
    pub fn delete_selections(&mut self, registers: &mut Registers, register: Option<char>) -> Vec<(usize, usize)> {
        if self.selections.is_empty() {
            return Vec::new();
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            if range.shape == motion::MotionShape::Linewise {
                shape = RegisterShape::Line;
            }
        }
        registers.record_delete(register, RegisterValue { text, shape });

        let mut froms: Vec<(usize, usize)> = self.selections.iter().map(|r| r.from).collect();
        froms.sort();
        let mut ranges = self.selections.clone();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.from));
        for range in &ranges {
            self.delete_range(range);
        }
        for pos in &mut froms {
            pos.0 = pos.0.min(self.lines.len() - 1);
            pos.1 = pos.1.min(self.lines[pos.0].len());
        }
        self.cursor = froms[0];
        froms
    }

    // Visual mode's own `p`/`P`: replaces every selection with the
    // register's content, broadcasting the same replacement to each
    // (see `editor.rs`'s own `put_over_selections` for why: no vim
    // precedent for multi-selection paste, and "replace every one of
    // these with that" is the more useful behavior). Unlike that
    // single-line version, this uses the register's *raw* text, embedded
    // newlines and all -- `insert_text` already understands them, so a
    // linewise yank pasted over a charwise selection correctly splits
    // the surrounding line around the inserted lines, matching real
    // vim's own visual-`p` shape for that case, with no special-casing
    // needed here.
    pub fn put_over_selections(&mut self, registers: &mut Registers, register: Option<char>) -> bool {
        if self.selections.is_empty() {
            return false;
        }
        let text = registers.read(register).text;
        if text.is_empty() {
            return false;
        }

        let leftmost = self.selections.iter().map(|r| r.from).min().unwrap();
        let mut ranges = self.selections.clone();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.from));
        let mut cursor_at = self.cursor;
        for range in &ranges {
            self.delete_range(range);
            let last_row = self.lines.len().saturating_sub(1);
            let at = (range.from.0.min(last_row), range.from.1.min(self.lines[range.from.0.min(last_row)].len()));
            let new_cursor = self.insert_text(at, &text);
            if range.from == leftmost {
                cursor_at = new_cursor;
            }
        }
        self.cursor = cursor_at;
        true
    }
}

impl Buffer for TextBuffer {
    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_len(&self, line: usize) -> usize {
        self.lines.get(line).map_or(0, |l| l.len())
    }

    fn char_at(&self, line: usize, col: usize) -> Option<char> {
        self.lines.get(line).and_then(|l| l.get(col)).copied()
    }

    fn cursor(&self) -> (usize, usize) {
        self.cursor
    }

    fn set_cursor(&mut self, line: usize, col: usize) {
        let row = line.min(self.lines.len().saturating_sub(1));
        let col = col.min(self.lines[row].len());
        self.cursor = (row, col);
    }

    fn viewport_top(&self) -> usize {
        self.vtop
    }

    fn set_viewport_top(&mut self, line: usize) {
        // A sub-row offset belongs to whichever line was on top; moving
        // the top line means it no longer names anything.
        if line != self.vtop {
            self.vtop_sub = 0;
        }
        self.vtop = line;
    }

    fn viewport_height(&self) -> usize {
        self.vheight
    }

    fn set_viewport_height(&mut self, rows: usize) {
        self.vheight = rows.max(1);
    }

    fn viewport_left(&self) -> usize {
        self.hleft
    }

    fn set_viewport_left(&mut self, col: usize) {
        self.hleft = col;
    }

    fn set_mark(&mut self, name: char, pos: (usize, usize)) {
        self.marks.insert(name, pos);
    }

    fn get_mark(&self, name: char) -> Option<(usize, usize)> {
        self.marks.get(&name).copied()
    }
}

// The buffer half of a live snippet -- see bishedit::snippet's own
// `SnippetHost` doc comment for why one line is always enough. Mirrors
// editor.rs's own impl for the single-line prompt buffer.
impl snippet::SnippetHost for TextBuffer {
    fn replace_span(&mut self, from: (usize, usize), to: (usize, usize), text: &str) {
        if to > from {
            self.delete_range(&motion::MotionRange { shape: motion::MotionShape::Exclusive, from, to });
        }
        self.insert_text(from, text);
    }

    fn place_cursor(&mut self, line: usize, col: usize) {
        self.set_cursor(line, col);
    }
}

// Which line ending a file uses. `\r\n` anywhere wins, since a file
// with even one is a CRLF file to every tool that reads it; a lone
// `\r` with no `\n` at all is the old Mac ending. Everything else,
// including an empty file, is Unix -- which is also the right default
// for a file with no line breaks to judge by.
fn detect_eol(text: &str) -> crate::editorconfig::Eol {
    if text.contains("\r\n") {
        return crate::editorconfig::Eol::Crlf;
    }
    if text.contains('\r') && !text.contains('\n') {
        return crate::editorconfig::Eol::Cr;
    }
    crate::editorconfig::Eol::Lf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn make(text: &str) -> TextBuffer {
        let lines: Vec<Vec<char>> = text.split('\n').map(|l| l.chars().collect()).collect();
        TextBuffer {
            undo: UndoTree::new(lines.clone(), (0, 0)),
            saved_node: 0,
            lines,
            cursor: (0, 0),
            vtop: 0,
            vheight: 10,
            hleft: 0,
            marks: HashMap::new(),
            selections: Vec::new(),
            wrap: crate::bishedit::wrap::Options::default(),
            tabular: None,
            hyperlinks: true,
            relativenumber: false,
            colors: None,
            cursorshape: true,
            mouse: true,
            expandtab: true,
            shiftwidth: crate::bishedit::vimkeys::INDENT_WIDTH,
            tabstop: crate::bishedit::vimkeys::INDENT_WIDTH,
            trim_trailing_whitespace: false,
            final_newline: true,
            eol: crate::editorconfig::Eol::Lf,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            breakpoints: std::collections::BTreeSet::new(),
            readonly: false,
            dirty: false,
            version: 0,
            path: None,
        }
    }

    fn text_of(buf: &TextBuffer) -> String {
        buf.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn make_registers() -> Registers {
        Registers::new_for_test()
    }

    #[test]
    fn insert_text_single_line_no_newline() {
        let mut buf = make("foo bar");
        let new_cursor = buf.insert_text((0, 3), "XYZ");
        assert_eq!(text_of(&buf), "fooXYZ bar");
        assert_eq!(new_cursor, (0, 6));
        assert!(buf.is_dirty());
    }

    #[test]
    fn insert_text_splits_the_line_on_embedded_newlines() {
        let mut buf = make("foobar");
        let new_cursor = buf.insert_text((0, 3), "1\n2\n3");
        assert_eq!(text_of(&buf), "foo1\n2\n3bar");
        assert_eq!(new_cursor, (2, 1));
    }

    #[test]
    fn insert_text_at_end_of_buffer_appends_a_new_line() {
        let mut buf = make("foo");
        let new_cursor = buf.insert_text((0, 3), "\nbar");
        assert_eq!(text_of(&buf), "foo\nbar");
        assert_eq!(new_cursor, (1, 3));
    }

    #[test]
    fn delete_range_within_one_line() {
        let mut buf = make("foo bar baz");
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 4), to: (0, 6) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "bar");
        assert_eq!(text_of(&buf), "foo  baz");
        assert_eq!(buf.cursor(), (0, 4));
    }

    #[test]
    fn delete_range_spanning_two_lines_joins_them() {
        let mut buf = make("foo bar\nbaz qux");
        // From the space before "bar" through the space before "qux" --
        // removes "bar\nbaz " and joins "foo " with "qux".
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 4), to: (1, 4) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "bar\nbaz ");
        assert_eq!(text_of(&buf), "foo qux");
        assert_eq!(buf.cursor(), (0, 4));
    }

    #[test]
    fn delete_range_spanning_three_lines_removes_the_middle_ones_entirely() {
        let mut buf = make("one\ntwo\nthree\nfour");
        let range = motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 1), to: (2, 1) };
        buf.delete_range(&range);
        assert_eq!(text_of(&buf), "oree\nfour");
    }

    #[test]
    fn delete_range_linewise_removes_whole_lines_and_never_leaves_zero_lines() {
        let mut buf = make("one\ntwo\nthree");
        let range = motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (2, 0) };
        let deleted = buf.delete_range(&range);
        assert_eq!(deleted, "one\ntwo\nthree\n");
        assert_eq!(text_of(&buf), "");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn mutations_set_the_dot_mark_at_the_cursors_own_landing_spot() {
        let mut buf = make("hello");
        assert_eq!(buf.get_mark('.'), None);
        let pos = buf.insert_text((0, 5), "!");
        assert_eq!(buf.get_mark('.'), Some(pos));

        let mut buf = make("one two");
        let range = motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 0), to: (0, 4) };
        buf.delete_range(&range);
        assert_eq!(buf.get_mark('.'), Some(buf.cursor()));

        let mut buf = make("one\ntwo");
        buf.set_cursor(0, 0);
        buf.join_lines(2, true);
        assert_eq!(buf.get_mark('.'), Some(buf.cursor()));
    }

    #[test]
    fn join_lines_default_inserts_a_space_and_strips_leading_whitespace() {
        let mut buf = make("one\n   two\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one two\nthree");
        assert_eq!(buf.cursor(), (0, 3)); // lands on the inserted space
        assert!(buf.is_dirty());
    }

    #[test]
    fn join_lines_no_space_when_current_line_already_ends_blank() {
        let mut buf = make("one   \ntwo");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one   two");
    }

    #[test]
    fn join_lines_no_space_before_a_leading_close_paren() {
        let mut buf = make("foo(a\n)bar");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "foo(a)bar");
    }

    #[test]
    fn join_lines_empty_joined_line_adds_nothing() {
        let mut buf = make("one\n\ntwo");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "one\ntwo");
    }

    #[test]
    fn gjoin_is_raw_concatenation_no_space_no_stripping() {
        let mut buf = make("one\n   two");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(2, false));
        assert_eq!(text_of(&buf), "one   two");
    }

    #[test]
    fn join_lines_count_joins_several_lines_at_once() {
        let mut buf = make("one\ntwo\nthree\nfour");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(3, true));
        assert_eq!(text_of(&buf), "one two three\nfour");
    }

    #[test]
    fn join_lines_count_of_one_behaves_like_two() {
        let mut buf = make("one\ntwo\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(1, true));
        assert_eq!(text_of(&buf), "one two\nthree");
    }

    #[test]
    fn join_lines_at_the_last_line_is_a_no_op() {
        let mut buf = make("only");
        buf.set_cursor(0, 0);
        assert!(!buf.join_lines(2, true));
        assert_eq!(text_of(&buf), "only");
        assert!(!buf.is_dirty());
    }

    #[test]
    fn join_lines_count_past_the_end_clamps_to_the_last_line() {
        let mut buf = make("one\ntwo\nthree");
        buf.set_cursor(0, 0);
        assert!(buf.join_lines(100, true));
        assert_eq!(text_of(&buf), "one two three");
    }

    // The whole point of `version` existing alongside `dirty`: `dirty`
    // answers "does this differ from disk," which undo and save both
    // move in *both* directions, while `version` only ever counts
    // forward. Anything holding an answer computed from an older
    // revision needs the second question, not the first.
    #[test]
    fn version_counts_forward_through_edits_undo_and_save_alike() {
        let mut buf = TextBuffer::new_unnamed(10);
        assert_eq!(buf.version(), 0);

        buf.insert_text((0, 0), "alpha");
        let after_insert = buf.version();
        assert!(after_insert > 0);

        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Exclusive, from: (0, 0), to: (0, 1) });
        let after_delete = buf.version();
        assert!(after_delete > after_insert);

        // Undo puts the *content* back, so `dirty` can go backwards --
        // but the revision must not, or a reply about `after_delete`
        // would be indistinguishable from one about whatever comes next.
        buf.checkpoint_undo();
        assert!(buf.undo(), "nothing to undo -- the checkpoint above didn't take");
        assert!(buf.version() > after_delete);
    }

    #[test]
    fn open_and_save_round_trip_a_real_file() {
        let dir = std::env::temp_dir().join(format!("bish-textbuffer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "alpha\nbeta\ngamma");
        assert!(!buf.is_dirty());

        buf.insert_text((0, 5), "!");
        assert!(buf.is_dirty());
        buf.save(None).unwrap();
        assert!(!buf.is_dirty());

        let saved = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved, "alpha!\nbeta\ngamma\n");
    }

    #[test]
    fn open_a_nonexistent_path_yields_a_fresh_buffer_with_that_path_remembered() {
        let path = std::env::temp_dir().join(format!("bish-textbuffer-nonexistent-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "");
        assert_eq!(buf.path(), Some(path.as_path()));
        buf.insert_text((0, 0), "hi");
        buf.save(None).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(saved, "hi\n");
    }

    fn range(from: (usize, usize), to: (usize, usize)) -> motion::MotionRange {
        motion::MotionRange { shape: motion::MotionShape::Inclusive, from, to }
    }

    #[test]
    fn yank_selections_concatenates_across_lines_with_no_separator() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        buf.yank_selections(&mut registers, None);
        assert_eq!(registers.read(None).text, "foobaz");
    }

    #[test]
    fn delete_selections_removes_every_range_leftmost_cursor_concatenated_register() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        let gaps = buf.delete_selections(&mut registers, None);
        assert_eq!(gaps, vec![(0, 0), (1, 0)]);
        assert_eq!(text_of(&buf), " bar\n qux");
        assert_eq!(registers.read(None).text, "foobaz");
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn delete_selections_on_an_empty_selection_list_is_a_no_op() {
        let mut buf = make("foo bar");
        let mut registers = make_registers();
        assert_eq!(buf.delete_selections(&mut registers, None), Vec::<(usize, usize)>::new());
        assert_eq!(text_of(&buf), "foo bar");
    }

    #[test]
    fn put_over_selections_broadcasts_multiline_register_content() {
        let mut buf = make("foo bar\nbaz qux");
        buf.selections = vec![range((0, 0), (0, 2)), range((1, 0), (1, 2))]; // "foo", "baz"
        let mut registers = make_registers();
        registers.write(None, RegisterValue { text: "X\nY".to_string(), shape: RegisterShape::Char });
        assert!(buf.put_over_selections(&mut registers, None));
        assert_eq!(text_of(&buf), "X\nY bar\nX\nY qux");
        // Register itself untouched -- same broadcast reasoning as
        // editor.rs's own put_over_selections.
        assert_eq!(registers.read(None).text, "X\nY");
    }

    #[test]
    fn put_over_selections_is_a_no_op_with_an_empty_register() {
        let mut buf = make("foo bar");
        buf.selections = vec![range((0, 0), (0, 2))];
        let mut registers = make_registers();
        assert!(!buf.put_over_selections(&mut registers, None));
        assert_eq!(text_of(&buf), "foo bar");
    }

    #[test]
    fn checkpoint_undo_is_a_noop_with_no_edits() {
        let mut buf = make("foo");
        buf.checkpoint_undo();
        assert!(!buf.undo());
    }

    #[test]
    fn checkpoint_undo_groups_edits_between_calls() {
        let mut buf = make("");
        buf.insert_text((0, 0), "foo");
        buf.checkpoint_undo();
        buf.insert_text((0, 3), "bar");
        buf.checkpoint_undo();
        assert_eq!(text_of(&buf), "foobar");

        // One undo restores the state after the *first* checkpoint, not
        // all the way back to empty -- proves grouping is per checkpoint
        // call, not per mutation.
        assert!(buf.undo());
        assert_eq!(text_of(&buf), "foo");
        assert!(buf.undo());
        assert_eq!(text_of(&buf), "");
        assert!(!buf.undo());
    }

    #[test]
    fn undo_redo_round_trip_content_and_cursor() {
        let mut buf = make("");
        buf.insert_text((0, 0), "foo");
        buf.checkpoint_undo();
        let cursor_after_insert = buf.cursor();

        assert!(buf.undo());
        assert_eq!(text_of(&buf), "");

        assert!(buf.redo());
        assert_eq!(text_of(&buf), "foo");
        assert_eq!(buf.cursor(), cursor_after_insert);
    }

    #[test]
    fn undo_clears_diagnostics() {
        let mut buf = make("foo");
        buf.insert_text((0, 0), "x");
        buf.checkpoint_undo();
        // Diagnostics computed for the *current* (post-edit) content --
        // set directly, since insert_text/checkpoint_undo themselves don't
        // touch this field.
        buf.diagnostics = vec![lint::Diagnostic { start: 0, end: 1, severity: lint::Severity::Warning, code: Cow::Borrowed("x"), source: None, message: String::new(), fix: None }];
        assert!(buf.undo());
        assert!(buf.diagnostics.is_empty());
    }

    #[test]
    fn time_travel_reaches_a_branch_undo_redo_alone_cannot() {
        let mut buf = make("");
        buf.insert_text((0, 0), "a");
        buf.checkpoint_undo(); // root("") -> A("a")
        buf.undo();
        buf.insert_text((0, 0), "b");
        buf.checkpoint_undo(); // root("") -> B("b"), a sibling of A
        assert_eq!(text_of(&buf), "b");
        assert!(!buf.redo()); // B is a leaf -- nothing to redo

        assert!(buf.time_travel(false)); // g- : B -> A, by creation order
        assert_eq!(text_of(&buf), "a");
        assert!(buf.time_travel(true)); // g+ : A -> B
        assert_eq!(text_of(&buf), "b");
    }

    #[test]
    fn time_travel_past_either_end_returns_false() {
        let mut buf = make("foo");
        assert!(!buf.time_travel(false));
        assert!(!buf.time_travel(true));
    }

    #[test]
    fn undo_redo_are_save_aware() {
        let dir = std::env::temp_dir().join(format!("bish-textbuffer-undo-save-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "foo\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.is_dirty());

        buf.insert_text((0, 3), "!");
        buf.checkpoint_undo();
        assert!(buf.is_dirty());

        buf.save(None).unwrap();
        assert!(!buf.is_dirty());

        // Undoing away from the just-saved content is dirty again...
        assert!(buf.undo());
        assert!(buf.is_dirty());
        // ...and redoing back onto it clears dirty again, without a
        // second save -- this is exactly what a plain `dirty = true` on
        // every undo/redo (the pre-existing simplification) couldn't do.
        assert!(buf.redo());
        assert!(!buf.is_dirty());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Read-only is enforced where the write happens, not at the command
    // that asked for it, so nothing can route around it -- and `:w
    // ELSEWHERE` deliberately still works, which is how content that
    // can't be written back (a zip member, a gzip'd file) gets out as an
    // ordinary file.
    #[test]
    fn a_readonly_buffer_refuses_to_overwrite_its_own_file_but_writes_elsewhere() {
        let dir = std::env::temp_dir().join(format!("bish-textbuffer-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("source.txt");
        std::fs::write(&original, "untouched\n").unwrap();

        let mut buf = TextBuffer::from_text(&original, "replacement\n", 10);
        buf.set_readonly(true);

        let err = buf.save(None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "untouched\n", "the file must be exactly as it was");

        let elsewhere = dir.join("copy.txt");
        buf.save(Some(&elsewhere)).unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "replacement\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn from_text_names_the_buffer_without_reading_that_path() {
        let buf = TextBuffer::from_text(std::path::Path::new("/nonexistent/thing.txt"), "one\ntwo\n", 10);
        assert_eq!(buf.path(), Some(std::path::Path::new("/nonexistent/thing.txt")));
        // `text()` re-adds the trailing newline `from_text` stripped --
        // the buffer holds two lines either way.
        assert_eq!(buf.text(), "one\ntwo\n");
        assert_eq!(buf.line_count(), 2);
        assert!(!buf.is_dirty());
    }

    #[test]
    fn a_crlf_file_stays_a_crlf_file() {
        let buf = TextBuffer::from_text(Path::new("/tmp/x"), "one\r\ntwo\r\n", 10);
        assert_eq!(buf.eol, crate::editorconfig::Eol::Crlf);
        assert_eq!(buf.line_count(), 2, "the \\r is not a character in the buffer");
        assert_eq!(buf.text(), "one\ntwo\n", "everything above this reads LF");
        assert_eq!(buf.on_disk_text(), "one\r\ntwo\r\n");
    }

    #[test]
    fn a_file_without_a_final_newline_keeps_not_having_one() {
        let mut buf = TextBuffer::from_text(Path::new("/tmp/x"), "one\ntwo", 10);
        assert!(!buf.final_newline);
        assert_eq!(buf.on_disk_text(), "one\ntwo");
        buf.final_newline = true;
        assert_eq!(buf.on_disk_text(), "one\ntwo\n");
    }

    #[test]
    fn trailing_whitespace_is_trimmed_only_when_asked() {
        let mut buf = TextBuffer::from_text(Path::new("/tmp/x"), "one   \ntwo\t\n", 10);
        assert_eq!(buf.on_disk_text(), "one   \ntwo\t\n");
        buf.trim_trailing_whitespace = true;
        assert_eq!(buf.on_disk_text(), "one\ntwo\n");
    }

    // The old Mac ending, and the one case a lone `\r` means it.
    #[test]
    fn a_cr_only_file_round_trips() {
        let buf = TextBuffer::from_text(Path::new("/tmp/x"), "one\rtwo\r", 10);
        assert_eq!(buf.eol, crate::editorconfig::Eol::Cr);
        assert_eq!(buf.on_disk_text(), "one\rtwo\r");
    }
}
