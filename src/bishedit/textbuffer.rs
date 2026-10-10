// The mutable multi-line buffer behind the file editor. Navigation goes
// through the shared `Buffer` trait; editing is this type's own, as it is
// for `editor.rs`'s `LineBuffer`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::Buffer;
use super::lint;
use super::motion;
use super::registers::{RegisterShape, RegisterValue, Registers};
use super::snippet;
use super::undo::UndoTree;

// One placeholder of a live snippet, in this buffer's (line, column)
// space. `active` is the one being typed into, drawn differently (see
// `fileeditor::build_editor_frame`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnippetHole {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    pub active: bool,
}

pub struct TextBuffer {
    // Never empty: an empty file is one empty line, as in vim (`empty` tells
    // them apart).
    lines: Vec<Vec<char>>,
    cursor: (usize, usize),
    vtop: usize,
    vheight: usize,
    // How many columns of the current line are scrolled off to the left (see
    // `Buffer::viewport_left`). Kept in sync by
    // `fileeditor::scroll_to_show_cursor`, like `vtop`.
    hleft: usize,
    marks: HashMap<char, (usize, usize)>,
    // Visual mode's committed selections, as in repl.rs's
    // `ScreenBuffer::selections`.
    pub selections: Vec<motion::MotionRange>,
    // A live snippet's placeholders, for whoever draws this buffer. View
    // state carried on the buffer like `selections`, so every frame showing
    // it agrees, a detached pane's frozen one included. Set by
    // `fileeditor::run_insert_mode` while a snippet is live.
    pub snippet_holes: Vec<SnippetHole>,
    /// The `ignorecase`/`smartcase` bishopts, copied in by
    /// `apply_view_options` (see `bishedit::ignore_case_for` and
    /// `Buffer::search_ignore_case`).
    pub ignorecase: bool,
    pub smartcase: bool,
    /// The `iskeyword` bishopt, copied the same way (see
    /// `Buffer::word_chars`).
    pub iskeyword: String,
    /// The project root this buffer's language server was reached through,
    /// when inherited rather than computed. A file opened fresh has none,
    /// and `repl::server_target` works one out. A file reached by a jump
    /// keeps its origin's, so `gd` into the Rust standard library uses the
    /// running rust-analyzer instead of starting one for the toolchain
    /// directory.
    pub lsp_root: Option<std::path::PathBuf>,
    /// What this buffer's language server says it's busy with, for the status
    /// line. `None` when idle or absent. Refreshed on the idle tick, like
    /// `diagnostics`.
    pub lsp_progress: Option<String>,
    /// A message to show once: a server's `window/showMessage` (usually that
    /// it couldn't load the project), or the file changing on disk. Takes the
    /// status line's progress slot and is cleared by the next key, vim's
    /// convention for an echoed message.
    pub echoed_message: Option<String>,
    // A language server's semantic tokens, resolved to colours and this
    // buffer's char offsets (see `repl::sync_semantic_tokens`), painted
    // over the local highlighter's layer. Resolved on arrival because
    // that's where the `Shell` is: a token type's colour comes from
    // `::bish hl`, whose open namespace the renderer's `ColorOverrides`
    // can't cover.
    //
    // Not cleared by an edit, unlike `diagnostics`: a misplaced colour for
    // the moment until the next answer beats the whole file strobing on
    // every keystroke.
    pub semantic_spans: Vec<super::highlight::StyledSpan>,
    /// Where else the symbol under the cursor appears, per the language
    /// server (`textDocument/documentHighlight`), in whole-buffer char
    /// offsets. Only the attributes are used, over the existing colour (see
    /// `highlight::compose_attrs`). Refreshed on the idle tick, emptied
    /// when the server has nothing to say.
    pub document_highlights: Vec<super::highlight::StyledSpan>,
    /// The language server's inlay hints as `(line, column, text)` in char
    /// positions, spliced between characters when drawn (see
    /// `fileeditor::display_row`). Sorted, for the one-pass splice. Requested
    /// for the visible range only.
    pub inlay_hints: Vec<(usize, usize, String)>,
    /// Whether to draw them: the `inlayhints` bishopt, on by default.
    pub inlayhints: bool,
    /// The `matchbrackets` bishopt: whether the bracket under the cursor and
    /// its partner are marked.
    pub matchbrackets: bool,
    // A fingerprint of the file's bytes as of the last read or write, to
    // notice it being rewritten. `None` for a buffer that isn't a view of a
    // file.
    disk_hash: Option<u64>,
    // `:diag`'s last result (see `fileeditor::diagnose_buffer`). Survives a
    // Ctrl+G detach like `selections`. Cleared by `:diag clear` or by any
    // edit, since its offsets point into the text as it was.
    pub diagnostics: Vec<lint::Diagnostic>,
    // How this view lays out a line wider than the pane: vim's `wrap` family,
    // set from bishopt by the driver (see repl.rs's `wrap_options`). A view
    // setting, like `vtop` and `hleft`.
    pub wrap: crate::bishedit::wrap::Options,
    // The delimiter this buffer's columns line up on, when its language has
    // a tabular form the `tabular` bishopt matches. Display only, like
    // `wrap`.
    pub tabular: Option<crate::bishedit::tabular::Style>,
    // The `hyperlinks` bishopt: whether links are real OSC 8 hyperlinks.
    // The link spans are computed either way.
    pub hyperlinks: bool,
    // The `relativenumber` bishopt: gutter numbers relative to the cursor's
    // line.
    pub relativenumber: bool,
    /// This session's `ui_col_*` colours (see theme.rs), carried here like
    /// `wrap` and `tabular` so every frame agrees, a detached pane's
    /// included, and the gutter renderers can stay plain functions of a
    /// `TextBuffer`.
    pub colors: Option<crate::theme::UiColors>,
    /// The `cursorshape` bishopt: whether the terminal cursor's shape shows
    /// the mode.
    pub cursorshape: bool,
    /// The `mouse` bishopt, so the frame and the key loop agree.
    pub mouse: bool,
    /// How this buffer indents: `expandtab` off inserts a literal tab,
    /// `shiftwidth` is what `>>` shifts by and Tab advances to, `tabstop` how
    /// wide a tab draws.
    pub expandtab: bool,
    pub shiftwidth: usize,
    pub tabstop: usize,
    /// Whether `:w` strips whitespace from the end of every line.
    pub trim_trailing_whitespace: bool,
    /// Whether the file ends in a newline. Detected on load, so a file
    /// without one stays without, unless `fixendofline` or
    /// `insert_final_newline` says otherwise.
    pub final_newline: bool,
    /// Whether this buffer holds no lines, as opposed to one empty line. A
    /// buffer always keeps a line for the cursor, so `lines` can't tell them
    /// apart, but they write differently: one byte versus zero (vim's
    /// `ML_EMPTY`). Set by loading an empty file or deleting the last line,
    /// cleared by inserting anything: `dd` on a one-line file writes an empty
    /// file.
    pub empty: bool,
    /// The line ending written back, detected on load, so a CRLF file stays
    /// CRLF.
    pub eol: crate::editorconfig::Eol,
    /// The encoding written back, detected on load: a latin-1 file saved as
    /// UTF-8 would change every accented character in it.
    pub encoding: crate::encoding::Encoding,
    /// Whether the file began with a byte-order mark. Separate from
    /// `encoding`: with and without one are the same encoding but different
    /// files.
    pub bom: bool,
    // Which visual row of `vtop`'s line the viewport starts at. Non-zero only
    // with wrapping, for a line taller than the pane, so a minified file can
    // still reach the cursor inside its one line.
    vtop_sub: usize,
    // `:git blame`'s toggle state (see `fileeditor::toggle_git_blame` and
    // the gutter's blame column). `None` is off; `Some` has one entry per
    // line, the inner `None` being a line git has nothing on (typed since,
    // or not in the blamed revision). Cleared on any edit, since a shifted
    // line invalidates it; `:git blame` again recomputes it.
    pub blame: Option<Vec<Option<crate::git::BlameLine>>>,
    // `:git diff`'s toggle state, like `blame`: `None` is off. Sparse: only
    // lines `crate::git::diff` marked have entries.
    pub diff: Option<std::collections::HashMap<usize, crate::git::DiffMark>>,
    // `zf` and friends. Not discarded on an edit like `diff` and `blame`: a
    // fold moves with its lines (see `content_changed`'s callers).
    pub folds: crate::bishedit::fold::Folds,
    // `:dbg`'s breakpoints, 1-based as the debugger reports lines, read by
    // the gutter's breakpoint column (see `GUTTER_COLUMNS` in fileeditor.rs).
    // Not cleared on edit: a buffer with breakpoints is `readonly` while it
    // has them.
    pub breakpoints: std::collections::BTreeSet<usize>,
    /// Whether a debug session (`:dbg`'s own or an adapter's) is attached.
    /// Only the gutter reads it, to keep the breakpoint column for the whole
    /// session: otherwise the first breakpoint shifts every line right, and
    /// there's no column to click the first one into.
    pub debug_attached: bool,
    // Set while a `:dbg` session is attached (repl.rs). Each mutating
    // `KeyOutcome` arm in `run_normal_mode_navigation` checks it alongside
    // `NavBuffer::Editable`. Doesn't affect `dirty` or `save`: `:dbg` won't
    // attach to a dirty buffer, and nothing can edit it meanwhile.
    readonly: bool,
    // `u`/`Ctrl-R`: a branching tree (see `bishedit::undo`), seeded with the
    // starting content in `new_unnamed`/`open`, and grouped by
    // `checkpoint_undo`. Survives a Ctrl+G detach.
    undo: UndoTree<Vec<Vec<char>>>,
    // The undo node that was on disk as of the last save (or the starting
    // content). Undo, redo and time travel compare against it so landing back
    // on it clears `dirty`, like vim's undo-aware `modified`. Ordinary edits
    // just set `dirty`.
    saved_node: usize,
    dirty: bool,
    // Bumped on every edit, never reset: which revision this is, where
    // `dirty` (which undo can clear) is whether it differs from disk. For
    // answers computed from the text that arrive later: a language server's
    // diagnostics carry the version they describe, so a reply about a past
    // revision can be dropped instead of drawn at stale offsets.
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
            empty: false,
            eol: crate::editorconfig::Eol::Lf,
            encoding: crate::encoding::Encoding::Utf8,
            bom: false,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            ignorecase: false,
            smartcase: false,
            iskeyword: "_".to_string(),
            lsp_root: None,
            lsp_progress: None,
            echoed_message: None,
            semantic_spans: Vec::new(),
            document_highlights: Vec::new(),
            inlay_hints: Vec::new(),
            inlayhints: true,
            matchbrackets: true,
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            folds: crate::bishedit::fold::Folds::default(),
            breakpoints: std::collections::BTreeSet::new(),
            debug_attached: false,
            readonly: false,
            dirty: false,
            disk_hash: None,
            version: 0,
            path: None,
        }
    }

    // A nonexistent path opens as a new buffer with that path, as vim's
    // `:e newfile` does; the file is created on the first `:w`.
    pub fn open(path: &Path, vheight: usize) -> io::Result<TextBuffer> {
        // Bytes, not a `String`: the encoding is worked out from them, so a
        // latin-1 file opens as text (see encoding.rs).
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let decoded = crate::encoding::decode(&bytes);
        let mut buf = TextBuffer::from_text(path, &decoded.text, vheight);
        buf.encoding = decoded.encoding;
        buf.bom = decoded.bom;
        buf.disk_hash = disk_hash(path);
        Ok(buf)
    }

    /// Whether the file changed on disk since this buffer last read or
    /// wrote it. Compares content, not mtime: a `touch`, a no-op formatter
    /// or a checkout of the same bytes changes nothing. `false` for a
    /// buffer with no path, a deleted file (writing it back is what you'd
    /// want), or content that never came from the path (a zip member, a
    /// decompressed stream).
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = self.path.as_deref() else { return false };
        let Some(known) = self.disk_hash else { return false };
        disk_hash(path).is_some_and(|now| now != known)
    }

    /// Re-reads this buffer from its file, discarding its content: `:e!`. The
    /// cursor stays where it was, clamped.
    pub fn reload(&mut self) -> io::Result<()> {
        let path = self.path.clone().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No file name"))?;
        let decoded = crate::encoding::decode(&std::fs::read(&path)?);
        let (row, col) = self.cursor;
        let fresh = TextBuffer::from_text(&path, &decoded.text, self.vheight);
        self.folds.text_replaced(&self.lines, &fresh.lines);
        self.lines = fresh.lines;
        self.eol = fresh.eol;
        self.encoding = decoded.encoding;
        self.bom = decoded.bom;
        self.disk_hash = disk_hash(&path);
        self.dirty = false;
        self.saved_node = self.undo.current_id();
        self.diagnostics.clear();
        self.selections.clear();
        self.diff = None;
        let row = row.min(self.lines.len() - 1);
        self.cursor = (row, col.min(self.lines[row].len()));
        self.content_changed();
        Ok(())
    }

    // A buffer holding `text` under `path`, for content not read from `path`
    // directly: compressed content (a gzipped file, a zip member), which
    // callers also mark readonly. See `fileeditor::compressed_text`.
    pub fn from_text(path: &Path, text: &str, vheight: usize) -> TextBuffer {
        // The file's line ending, before normalizing, so `save` writes it
        // back.
        let eol = detect_eol(text);
        // Normalized to `\n`: everything here assumes one character per
        // line break, and a `\r` would be a character the file's text
        // doesn't have.
        let normalized = match eol {
            crate::editorconfig::Eol::Lf => std::borrow::Cow::Borrowed(text),
            crate::editorconfig::Eol::Crlf => std::borrow::Cow::Owned(text.replace("\r\n", "\n")),
            crate::editorconfig::Eol::Cr => std::borrow::Cow::Owned(text.replace('\r', "\n")),
        };
        // A trailing newline is stripped so it isn't a phantom empty last
        // line, and remembered so a file without one stays without. An
        // empty file counts as having one: what an untouched empty file
        // writes is up to `empty`, and typing into one should end with a
        // newline, as a new file does.
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
            encoding: crate::encoding::Encoding::Utf8,
            bom: false,
            final_newline,
            empty: normalized.is_empty(),
            expandtab: true,
            shiftwidth: crate::bishedit::vimkeys::INDENT_WIDTH,
            tabstop: crate::bishedit::vimkeys::INDENT_WIDTH,
            trim_trailing_whitespace: false,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            ignorecase: false,
            smartcase: false,
            iskeyword: "_".to_string(),
            lsp_root: None,
            lsp_progress: None,
            echoed_message: None,
            semantic_spans: Vec::new(),
            document_highlights: Vec::new(),
            inlay_hints: Vec::new(),
            inlayhints: true,
            matchbrackets: true,
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            folds: crate::bishedit::fold::Folds::default(),
            breakpoints: std::collections::BTreeSet::new(),
            debug_attached: false,
            readonly: false,
            dirty: false,
            disk_hash: None,
            version: 0,
            path: Some(path.to_path_buf()),
        }
    }

    // See `vtop_sub`. Cleared when the top line moves.
    pub fn viewport_sub(&self) -> usize {
        self.vtop_sub
    }

    pub fn set_viewport_sub(&mut self, sub: usize) {
        self.vtop_sub = sub;
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Points the buffer at its file's new name. The text and saved state
    /// stay: the file moved with its contents.
    pub fn set_path(&mut self, path: &Path) {
        self.path = Some(path.to_path_buf());
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// This buffer's revision; see `version`.
    pub fn version(&self) -> u64 {
        self.version
    }

    // What every edit does: bump the revision and drop everything derived
    // from the old text. Called by `insert_text`, `delete_range`,
    // `join_lines` and `restore_snapshot`, the only places content changes.
    // Leaves `dirty` alone: the edits set it, while `restore_snapshot`
    // compares against the saved node.
    fn content_changed(&mut self) {
        self.version += 1;
        // Their positions are char offsets into the text that just changed.
        self.diagnostics.clear();
        self.blame = None;
        self.diff = None;
    }

    pub fn is_readonly(&self) -> bool {
        self.readonly
    }

    // `:dbg`'s switch (repl.rs); see `readonly`.
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    // Commits the content and cursor as a new undo node if they changed
    // (see `UndoTree::checkpoint`). Called by repl.rs's `render_nav_frame`
    // once per key dispatched in Normal mode, which is what makes a whole
    // Insert session one undo step.
    pub fn checkpoint_undo(&mut self) {
        self.undo.checkpoint(&self.lines, self.cursor);
    }

    // `u`: restores the parent node. `false` at the root.
    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.undo() else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // `Ctrl-R`: restores the most recently created child (see
    // `UndoTree::redo`). `false` without children.
    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.undo.redo() else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // `g-`/`g+`: walks the tree's creation history rather than its edges,
    // reaching branches undo and redo can't (see
    // `UndoTree::time_travel_back`). `forward: true` is `g+`. `false` at
    // either end.
    pub fn time_travel(&mut self, forward: bool) -> bool {
        let result = if forward { self.undo.time_travel_forward() } else { self.undo.time_travel_back() };
        let Some(snap) = result else { return false };
        let (content, cursor) = (snap.content.clone(), snap.cursor);
        self.restore_snapshot(content, cursor);
        true
    }

    // Restores a snapshot for undo, redo and time travel.
    fn restore_snapshot(&mut self, content: Vec<Vec<char>>, cursor: (usize, usize)) {
        // A snapshot doesn't say which lines moved; `text_replaced` works
        // it out for the folds.
        self.folds.text_replaced(&self.lines, &content);
        self.lines = content;
        self.cursor = cursor;
        self.content_changed();
        // Landing on the saved node clears `dirty`; see `saved_node`.
        self.dirty = self.undo.current_id() != self.saved_node;
    }

    /// The bytes this buffer writes: its text with this file's line ending,
    /// trailing whitespace trimmed if asked, and a final newline only if it
    /// should have one. Separate from `text()`, which every highlighter,
    /// linter and offset reads: a `\r` there would shift every offset.
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
        // No lines means no bytes; see `empty`.
        if self.empty {
            return String::new();
        }
        let mut out = lines.join(self.eol.text());
        if self.final_newline {
            out.push_str(self.eol.text());
        }
        out
    }

    /// The full text, `\n`-joined with a trailing newline. For anything that
    /// wants a re-parseable string of the live buffer, such as `K` hover's
    /// doc-comment lookup, which sees unsaved edits.
    pub fn text(&self) -> String {
        let mut text: String = self.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n");
        text.push('\n');
        text
    }

    /// Writes `on_disk_text`. `path` overrides the buffer's path for this
    /// write, and becomes it if there was none, as vim's `:w newname` does
    /// on an unnamed buffer.
    pub fn save(&mut self, path: Option<&Path>) -> io::Result<()> {
        let target = path.or(self.path.as_deref()).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "No file name"))?;
        // Read-only concerns this file, not the text, so `:w ELSEWHERE`
        // still works (vim's rule). Enforced at the one place that writes:
        // writing a compressed buffer back to its own path would replace an
        // archive with its decompressed contents.
        if self.readonly && Some(target) == self.path.as_deref() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "buffer is read-only"));
        }
        // Encoded before anything is written, so a character the encoding
        // can't hold leaves the file untouched.
        let bytes = crate::encoding::encode(&self.on_disk_text(), self.encoding, self.bom)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(target, bytes)?;
        if self.path.is_none() {
            self.path = Some(target.to_path_buf());
        }
        self.dirty = false;
        // Records what's now on disk (see `saved_node`). The undo tree and
        // `lines` are already in sync: `render_nav_frame` checkpoints after
        // every key, before the next is read.
        self.saved_node = self.undo.current_id();
        self.disk_hash = self.path.as_deref().and_then(disk_hash);
        Ok(())
    }

    // Splits `text` on `\n` and splices it in at `at`, joining and splitting
    // lines as needed. Returns the position just after the inserted text, as
    // vim leaves the cursor (see vimkeys.rs's `apply_put`).
    pub fn insert_text(&mut self, at: (usize, usize), text: &str) -> (usize, usize) {
        if text.is_empty() {
            return at;
        }
        // There's content now, so the placeholder line is a real one.
        self.empty = false;
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
            // Inserted at the very start of a line, the old line moved down
            // below the new ones; anywhere else, they follow it. A fold
            // only grows by lines landing between two of its own.
            let at = if col == 0 { row } else { row + 1 };
            self.folds.lines_inserted(at, parts.len() - 1);
            (new_row, new_col)
        };
        self.dirty = true;
        self.content_changed();
        self.cursor = new_pos;
        // `.`: vim's last-change mark (`` `. ``), set here because these
        // three methods are the only places an edit happens.
        self.marks.insert('.', new_pos);
        new_pos
    }

    /// Replaces whole lines in `range` with `with`: putting back one
    /// refused change of a review. Not a delete then an insert: deleting
    /// every line leaves a placeholder the insert would keep as an empty
    /// last line, and a range at the end has no line after it to insert
    /// before.
    pub fn replace_lines(&mut self, range: std::ops::Range<usize>, with: Vec<String>) {
        let start = range.start.min(self.lines.len());
        let end = range.end.min(self.lines.len());
        let added = with.len();
        if self.empty {
            self.lines.clear();
        }
        let (start, end) = (start.min(self.lines.len()), end.min(self.lines.len()));
        self.lines.splice(start..end, with.into_iter().map(|line| line.chars().collect::<Vec<char>>()));
        self.folds.lines_deleted(start, end - start);
        self.folds.lines_inserted(start, added);
        self.empty = self.lines.is_empty();
        if self.empty {
            self.lines.push(Vec::new());
        }
        let row = start.min(self.lines.len() - 1);
        self.cursor = (row, 0);
        self.dirty = true;
        self.content_changed();
        self.marks.insert('.', self.cursor);
    }

    // Removes a `MotionRange` that may span lines, joining the cut ends, and
    // returns the removed text. Rows are clamped to the current bounds first,
    // so a stale range (Visual selections deleted back to back) can't panic.
    pub fn delete_range(&mut self, range: &motion::MotionRange) -> String {
        let last_row = self.lines.len().saturating_sub(1);
        let range =
            motion::MotionRange { shape: range.shape, from: (range.from.0.min(last_row), range.from.1), to: (range.to.0.min(last_row), range.to.1) };
        let text = motion::extract_text(&*self, &range);
        match range.shape {
            motion::MotionShape::Linewise => {
                self.lines.drain(range.from.0..=range.to.0);
                self.folds.lines_deleted(range.from.0, range.to.0 - range.from.0 + 1);
                if self.lines.is_empty() {
                    // No lines left. The placeholder is somewhere for the
                    // cursor, not content (see `empty`). Only a linewise
                    // delete gets here: `x` on a line's last character
                    // leaves an empty line, which still writes its newline.
                    self.empty = true;
                    self.lines.push(Vec::new());
                }
                let row = range.from.0.min(self.lines.len() - 1);
                self.cursor = (row, 0);
            }
            // A block cuts the same columns from each row and leaves the
            // rows. A row shorter than the block gives what falls inside,
            // possibly nothing.
            motion::MotionShape::Blockwise => {
                let (left, right) = motion::block_columns(&range).expect("blockwise");
                for line in range.from.0..=range.to.0 {
                    let row = &mut self.lines[line];
                    let start = left.min(row.len());
                    let end = (right + 1).min(row.len());
                    if end > start {
                        row.drain(start..end);
                    }
                }
                self.cursor = (range.from.0, left.min(self.lines[range.from.0].len()));
            }
            _ => {
                let end_col = if range.shape == motion::MotionShape::Inclusive { range.to.1 + 1 } else { range.to.1 };
                let last_line = &self.lines[range.to.0];
                let after: Vec<char> = last_line.get(end_col.min(last_line.len())..).map(|s| s.to_vec()).unwrap_or_default();
                let first_line = &self.lines[range.from.0];
                let mut joined: Vec<char> = first_line[..range.from.1.min(first_line.len())].to_vec();
                joined.extend(after);
                self.lines.splice(range.from.0..=range.to.0, std::iter::once(joined));
                // The lines after the first were joined onto it, so those
                // are gone.
                self.folds.lines_deleted(range.from.0 + 1, range.to.0 - range.from.0);
                let row = range.from.0;
                self.cursor = (row, range.from.1.min(self.lines[row].len()));
            }
        }
        self.dirty = true;
        self.content_changed();
        self.marks.insert('.', self.cursor);
        text
    }

    // `J`/`gJ`: joins `count` lines (at least 2, so `J` and `1J` both join
    // with the next) from the cursor's. `with_space` is vim's `J`: strip the
    // joined line's leading whitespace and put one space between, unless the
    // current line ends in whitespace, the joined line is empty, or it starts
    // with `)`. `gJ` concatenates. Returns whether anything was joined. The
    // cursor lands at the last join's boundary, as in vim.
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
        self.folds.lines_deleted(row + 1, joins);
        self.cursor = (row, join_col.min(self.lines[row].len().saturating_sub(1)));
        self.dirty = true;
        self.content_changed();
        self.marks.insert('.', self.cursor);
        true
    }

    // Visual `y`: every selection, concatenated, as in editor.rs and repl.rs.
    // A `Linewise` part already ends in `\n`.
    pub fn yank_selections(&self, registers: &mut Registers, register: Option<char>) {
        if self.selections.is_empty() {
            return;
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            // A block stays a block through the register; `Line` wins over
            // both.
            match range.shape {
                motion::MotionShape::Linewise => shape = RegisterShape::Line,
                motion::MotionShape::Blockwise if shape == RegisterShape::Char => shape = RegisterShape::Block,
                _ => {}
            }
        }
        registers.record_yank(register, RegisterValue { text, shape });
    }

    // Visual `d`: writes the deleted text to a register first (in vim,
    // delete always yanks), then removes selections from the highest
    // position down, so earlier ones keep their coordinates. `delete_range`
    // clamps rows against any overlap.
    //
    // Returns each selection's resulting position, ascending. Removing
    // highest first keeps every lower one valid, so `c` can type into all
    // of them.
    pub fn delete_selections(&mut self, registers: &mut Registers, register: Option<char>) -> Vec<(usize, usize)> {
        if self.selections.is_empty() {
            return Vec::new();
        }
        let mut text = String::new();
        let mut shape = RegisterShape::Char;
        for range in &self.selections {
            text.push_str(&motion::extract_text(self, range));
            match range.shape {
                motion::MotionShape::Linewise => shape = RegisterShape::Line,
                motion::MotionShape::Blockwise if shape == RegisterShape::Char => shape = RegisterShape::Block,
                _ => {}
            }
        }
        registers.record_delete(register, RegisterValue { text, shape });

        // One position per range, but a block leaves one per row, so `c` in
        // block mode types into every row.
        let mut froms: Vec<(usize, usize)> = self
            .selections
            .iter()
            .flat_map(|r| match motion::block_columns(r) {
                Some((left, _)) => (r.from.0..=r.to.0).map(|line| (line, left)).collect::<Vec<_>>(),
                None => vec![r.from],
            })
            .collect();
        crate::sort::sort(&mut froms);
        let mut ranges = self.selections.clone();
        crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from));
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

    // Visual `p`/`P`: replaces every selection with the register's content
    // (see editor.rs's `put_over_selections`). Uses the raw text, newlines
    // and all: `insert_text` splits the line around a linewise yank put
    // over a charwise selection, as vim's visual `p` does.
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
        crate::sort::sort_by_key(&mut ranges, |r| std::cmp::Reverse(r.from));
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
        // A sub-row offset belongs to the old top line.
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

    fn search_ignore_case(&self, pattern: &str) -> bool {
        super::ignore_case_for(pattern, self.ignorecase, self.smartcase)
    }

    fn word_chars(&self) -> &str {
        &self.iskeyword
    }

    fn folds(&self) -> Option<&crate::bishedit::fold::Folds> {
        Some(&self.folds)
    }

    fn folds_mut(&mut self) -> Option<&mut crate::bishedit::fold::Folds> {
        Some(&mut self.folds)
    }
}

// The buffer half of a live snippet (see `snippet::SnippetHost`).
// Mirrors editor.rs's impl for the prompt buffer.
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

// A file's line ending. Any `\r\n` makes it CRLF, as every tool reading
// it will see; a lone `\r` with no `\n` is the old Mac ending. Everything
// else, an empty file included, is Unix.
fn detect_eol(text: &str) -> crate::editorconfig::Eol {
    if text.contains("\r\n") {
        return crate::editorconfig::Eol::Crlf;
    }
    if text.contains('\r') && !text.contains('\n') {
        return crate::editorconfig::Eol::Cr;
    }
    crate::editorconfig::Eol::Lf
}

// FNV-1a over a file's bytes, to tell the same file from a different one
// without keeping a second copy of every open file. Not cryptographic: the
// other side is a formatter or a branch switch, not an adversary.
fn disk_hash(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    Some(h)
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
            empty: false,
            eol: crate::editorconfig::Eol::Lf,
            encoding: crate::encoding::Encoding::Utf8,
            bom: false,
            vtop_sub: 0,
            snippet_holes: Vec::new(),
            ignorecase: false,
            smartcase: false,
            iskeyword: "_".to_string(),
            lsp_root: None,
            lsp_progress: None,
            echoed_message: None,
            semantic_spans: Vec::new(),
            document_highlights: Vec::new(),
            inlay_hints: Vec::new(),
            inlayhints: true,
            matchbrackets: true,
            diagnostics: Vec::new(),
            blame: None,
            diff: None,
            folds: crate::bishedit::fold::Folds::default(),
            breakpoints: std::collections::BTreeSet::new(),
            debug_attached: false,
            readonly: false,
            dirty: false,
            disk_hash: None,
            version: 0,
            path: None,
        }
    }

    fn text_of(buf: &TextBuffer) -> String {
        buf.lines.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn folded(text: &str, start: usize, end: usize) -> TextBuffer {
        let mut buf = TextBuffer::from_text(std::path::Path::new("/tmp/bish-fold-test.txt"), text, 10);
        buf.folds.create(start, end);
        buf
    }

    #[test]
    fn j_and_k_step_over_a_closed_fold_as_one_line() {
        let mut buf = folded("0\n1\n2\n3\n4\n", 1, 3);
        motion::apply_motion(&mut buf, motion::Motion::Down, None);
        assert_eq!(buf.cursor().0, 1, "onto the fold's row");
        motion::apply_motion(&mut buf, motion::Motion::Down, None);
        assert_eq!(buf.cursor().0, 4, "and past every line of it");
        motion::apply_motion(&mut buf, motion::Motion::Up, None);
        assert_eq!(buf.cursor().0, 1, "back onto its row, not its last line");
        motion::apply_motion(&mut buf, motion::Motion::Down, Some(2));
        assert_eq!(buf.cursor().0, 4, "a count clamps at the last line");
    }

    #[test]
    fn landing_in_a_closed_fold_opens_it_only_for_a_motion_that_was_looking() {
        let mut buf = folded("0\n1\n2\n3\n4\n", 1, 3);
        motion::apply_motion(&mut buf, motion::Motion::GotoFirstLine, Some(3));
        assert_eq!(buf.cursor().0, 1, "`3gg` stops on the fold's row");
        assert_eq!(buf.folds.closed_at(2), Some((1, 3)), "and leaves it closed");
        buf.set_cursor(2, 0);
        motion::apply_motion(&mut buf, motion::Motion::Right, None);
        assert_eq!(buf.folds.closed_at(2), None, "`l` opens it");
        assert_eq!(buf.cursor().0, 2);
    }

    #[test]
    fn an_operator_takes_a_closed_fold_whole() {
        let mut buf = folded("0\n1\n2\n3\n4\n", 0, 1);
        let range = motion::motion_range(&mut buf, motion::Motion::Down, None).unwrap();
        assert_eq!((range.from.0, range.to.0), (0, 2), "`dj` is the fold and the line after it");
        buf.set_cursor(0, 0);
        assert_eq!(motion::line_span(&buf, 1), (0, 1), "`dd` is the fold");
        assert_eq!(motion::line_span(&buf, 2), (0, 2), "`2dd` is the fold and one more line");
        assert_eq!(motion::whole_lines(&buf, 1), "0\n1\n", "`yy` yanks what `dd` would delete");
    }

    #[test]
    fn a_charwise_operator_on_a_closed_fold_stops_at_the_fold() {
        let mut buf = folded("ab\ncd\nef\ngh\n", 0, 1);
        let range = motion::motion_range(&mut buf, motion::Motion::Right, None).unwrap();
        assert_eq!(motion::extract_text(&buf, &range), "ab\ncd\n", "`yl` takes the fold and not the rest of the file");
        buf.delete_range(&range);
        assert_eq!(text_of(&buf), "ef\ngh", "`dl` takes every line of it");
    }

    #[test]
    fn a_fold_moves_with_the_lines_an_edit_adds_and_removes() {
        let mut buf = folded("0\n1\n2\n3\n", 2, 3);
        buf.insert_text((0, 0), "new\n");
        assert_eq!(buf.folds.closed_at(3), Some((3, 4)), "a line above pushes it down");
        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (1, 0) });
        assert_eq!(buf.folds.closed_at(1), Some((1, 2)), "two lines above gone pull it up");
        buf.set_cursor(1, 0);
        buf.join_lines(2, true);
        assert_eq!(text_of(&buf), "1\n2 3");
        assert_eq!(buf.folds.closed_at(1), Some((1, 1)), "a joined-away line leaves it");
    }

    #[test]
    fn a_line_opened_or_put_on_a_closed_fold_goes_below_the_whole_of_it() {
        let mut buf = folded("0\n1\n2\n", 0, 1);
        crate::fileeditor::open_line(&mut buf, false);
        assert_eq!(text_of(&buf), "0\n1\n\n2");
        assert_eq!(buf.cursor().0, 2);
        assert_eq!(buf.folds.closed_at(1), Some((0, 1)), "and the fold did not grow to take it in");
    }

    #[test]
    fn replace_lines_swaps_whole_lines_in_the_middle_at_the_end_and_everywhere() {
        let mut buf = TextBuffer::from_text(std::path::Path::new("/tmp/bish-replace-test.txt"), "a\nB\nB2\nc\n", 10);
        buf.replace_lines(1..3, vec!["b".to_string()]);
        assert_eq!(text_of(&buf), "a\nb\nc");
        buf.replace_lines(3..3, vec!["d".to_string()]);
        assert_eq!(text_of(&buf), "a\nb\nc\nd", "past the last line is an append");
        buf.replace_lines(0..4, Vec::new());
        assert_eq!(buf.on_disk_text(), "", "every line gone is an empty file, not one empty line");
        buf.replace_lines(0..0, vec!["x".to_string()]);
        assert_eq!(buf.on_disk_text(), "x\n");
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
    fn changed_on_disk_compares_content_not_timestamps() {
        let dir = std::env::temp_dir().join(format!("bish-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert!(!buf.changed_on_disk());

        // Rewritten with the same bytes (a `touch`, a no-op formatter): not a
        // change.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "one\n").unwrap();
        assert!(!buf.changed_on_disk(), "same content is not a change, whatever the timestamp says");

        std::fs::write(&path, "someone else\n").unwrap();
        assert!(buf.changed_on_disk());

        // Saving takes ownership of what's there again.
        buf.save(None).unwrap();
        assert!(!buf.changed_on_disk());

        // ...and so does reloading, which also takes the new content.
        std::fs::write(&path, "third\n").unwrap();
        assert!(buf.changed_on_disk());
        buf.reload().unwrap();
        assert_eq!(buf.text(), "third\n");
        assert!(!buf.changed_on_disk());
        assert!(!buf.is_dirty(), "a reload is not an unsaved change");

        let unnamed = TextBuffer::new_unnamed(10);
        assert!(!unnamed.changed_on_disk());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_range_takes_a_block_out_of_every_row_and_keeps_the_rows() {
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "aaa111\nbbb222\nccc333\nddd444");
        let text = buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Blockwise, from: (0, 0), to: (2, 2) });
        assert_eq!(text, "aaa\nbbb\nccc");
        assert_eq!(buf.text(), "111\n222\n333\nddd444\n", "the rows stay; only the rectangle goes");
        assert_eq!(buf.cursor(), (0, 0));

        // A row too short to reach the block loses nothing and isn't padded.
        let mut buf = TextBuffer::new_unnamed(10);
        buf.insert_text((0, 0), "aaa111\nb\nccc333");
        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Blockwise, from: (0, 3), to: (2, 5) });
        assert_eq!(buf.text(), "aaa\nb\nccc\n");
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
        // From the space before `bar` to the space before `qux`: removes
        // `bar\nbaz ` and joins `foo ` with `qux`.
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

    // `dirty` moves both ways under undo and save; `version` only counts
    // forward, which is what an answer about an older revision needs.
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

        // Undo restores the content, so `dirty` can go back, but the revision
        // mustn't.
        buf.checkpoint_undo();
        assert!(buf.undo(), "nothing to undo -- the checkpoint above didn't take");
        assert!(buf.version() > after_delete);
    }

    // `caf\xe9` is latin-1, not valid UTF-8, and must open and save
    // unchanged.
    #[test]
    fn a_latin1_file_opens_as_text_and_saves_as_latin1() {
        let dir = std::env::temp_dir().join(format!("bish-encoding-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latin1.txt");
        std::fs::write(&path, b"caf\xe9\nna\xefve\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "café\nnaïve");
        assert_eq!(buf.encoding, crate::encoding::Encoding::Latin1);

        buf.insert_text((0, 4), "s");
        buf.save(None).unwrap();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved, b"caf\xe9s\nna\xefve\n", "still latin-1, and only the edit changed");
    }

    // A character the encoding can't hold fails the save and leaves the file
    // as it was, rather than writing a `?`.
    #[test]
    fn a_character_the_files_encoding_cannot_hold_fails_the_save() {
        let dir = std::env::temp_dir().join(format!("bish-encoding-refuse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("latin1.txt");
        std::fs::write(&path, b"caf\xe9\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        buf.insert_text((0, 4), "\u{2014}");
        let err = buf.save(None).unwrap_err();
        let on_disk = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert!(err.to_string().contains("U+2014"), "the message names the character: {err}");
        assert_eq!(on_disk, b"caf\xe9\n", "the file is exactly as it was");
    }

    #[test]
    fn a_utf16_file_keeps_its_encoding_and_its_mark() {
        let dir = std::env::temp_dir().join(format!("bish-encoding-utf16-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.txt");
        std::fs::write(&path, b"\xff\xfeh\x00i\x00\n\x00").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "hi");
        assert_eq!(buf.encoding, crate::encoding::Encoding::Utf16Le);
        buf.insert_text((0, 2), "!");
        buf.save(None).unwrap();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved, b"\xff\xfeh\x00i\x00!\x00\n\x00");
    }

    // A BOM isn't a character in the buffer, and it isn't dropped on save:
    // whatever reads the file next may look for it.
    #[test]
    fn a_utf8_bom_survives_a_save_without_becoming_text() {
        let dir = std::env::temp_dir().join(format!("bish-encoding-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bom.txt");
        std::fs::write(&path, b"\xef\xbb\xbfone\n").unwrap();

        let mut buf = TextBuffer::open(&path, 10).unwrap();
        assert_eq!(text_of(&buf), "one");
        assert!(buf.bom);
        buf.insert_text((0, 3), "!");
        buf.save(None).unwrap();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(saved, b"\xef\xbb\xbfone!\n");
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
        // The register itself is untouched.
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

        // One undo goes back to the first checkpoint, not to empty:
        // grouping is per checkpoint, not per edit.
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
        // Diagnostics for the current content, set directly.
        buf.diagnostics = vec![lint::Diagnostic {
            start: 0,
            end: 1,
            severity: lint::Severity::Warning,
            code: Cow::Borrowed("x"),
            source: None,
            message: String::new(),
            fix: None,
        }];
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

        // Undoing away from the saved content is dirty again...
        assert!(buf.undo());
        assert!(buf.is_dirty());
        // ...and redoing onto it clears dirty without a second save.
        assert!(buf.redo());
        assert!(!buf.is_dirty());

        std::fs::remove_dir_all(&dir).ok();
    }

    // Read-only is enforced where the write happens, so nothing can route
    // around it. `:w ELSEWHERE` still works, which is how a zip member or a
    // gzipped file gets out as an ordinary file.
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
        // `text()` adds back the trailing newline `from_text` stripped.
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

    // A file of no lines (zero bytes) and one empty line (one byte) differ;
    // see `empty`.
    #[test]
    fn an_empty_file_stays_empty_and_only_a_linewise_delete_empties_one() {
        let path = Path::new("/tmp/x");
        // Opened and saved untouched: still zero bytes.
        let buf = TextBuffer::from_text(path, "", 10);
        assert!(buf.empty);
        assert_eq!(buf.on_disk_text(), "");

        // Typing into it makes it a real line.
        let mut buf = TextBuffer::from_text(path, "", 10);
        buf.insert_text((0, 0), "hi");
        assert!(!buf.empty);
        assert_eq!(buf.on_disk_text(), "hi\n");

        // Deleting every line leaves no lines, which writes nothing.
        let mut buf = TextBuffer::from_text(path, "a\nb\n", 10);
        assert!(!buf.empty);
        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Linewise, from: (0, 0), to: (1, 0) });
        assert!(buf.empty);
        assert_eq!(buf.on_disk_text(), "");

        // Emptying the only line leaves the line, which still ends in a
        // newline.
        let mut buf = TextBuffer::from_text(path, "a\n", 10);
        buf.delete_range(&motion::MotionRange { shape: motion::MotionShape::Inclusive, from: (0, 0), to: (0, 0) });
        assert!(!buf.empty);
        assert_eq!(buf.on_disk_text(), "\n");
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

    // The old Mac ending, the one case where a lone `\r` means it.
    #[test]
    fn a_cr_only_file_round_trips() {
        let buf = TextBuffer::from_text(Path::new("/tmp/x"), "one\rtwo\r", 10);
        assert_eq!(buf.eol, crate::editorconfig::Eol::Cr);
        assert_eq!(buf.on_disk_text(), "one\rtwo\r");
    }
}
