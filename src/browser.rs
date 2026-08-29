// `e DIR` -- a real file browser, drawn into the focused pane so files
// can be picked out of it and opened. A top-level module, the same tier
// as fileeditor.rs/debugger.rs: a concrete interactive view, not
// reusable headless logic.
//
// Split the same way every other interactive view in this codebase
// already is: *this* module owns the model (what's in the directory,
// where the cursor is, what's selected, what the filter matches), the
// layout arithmetic, and the rendering -- all of it pure enough to unit
// test without a terminal -- while repl.rs owns the small driving loop
// (`run_browse_frame`: a RawGuard, `read_key_idle`, and the same
// `service_background_jobs` idle callback every other blocking loop
// here already threads through). Same shape as
// `render_diagnostics_list_frame`/`run_diagnostics_frame`.
//
// The layout is a real grid, filled *column-major* -- item 0 top-left,
// item 1 directly below it, wrapping to the top of the next column --
// which is exactly what `ls` and Midnight Commander's panel do, and
// what makes an alphabetical listing read naturally down a column.
// Overflow scrolls sideways by whole columns rather than wrapping to a
// second page.
//
// Because the layout is a grid, `hjkl`/arrows are *grid* movement (left/
// right step a whole column, up/down step one row within it), not
// ranger's "h = parent, l = child" -- that vocabulary only makes sense
// in ranger's stacked-column view, where there's no second axis to
// navigate. Directory navigation lives on Enter (descend) and
// Backspace/Alt-Up (parent) instead, matching a graphical browser.
//
// `Outcome::Accepted` carries paths rather than performing anything with
// them: the browser is a *chooser*, and its one caller today (`e DIR`,
// which opens each chosen path as an editor frame -- see repl.rs's
// expand_browse_targets) is not the only thing that shape can serve.
// Cancelling with Esc/`q` chooses nothing, so `e DIR` then opens
// nothing, rather than falling back to some arbitrary file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bishedit::fuzzy::fuzzy_match;
use crate::bishedit::grapheme;
use crate::bishedit::unicode_width::{char_width, str_width};
use crate::editor::Key;
use crate::repl::Rect;

// One listing entry. `path` is always absolute (joined against the
// browser's own cwd, which itself is canonicalized on open) so the
// selection set stays meaningful across directory changes and across
// the fuzzy filter narrowing/widening under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    // A zip archive, which this browser navigates *into* exactly like a
    // directory (see `navigable`) -- the whole point of the feature, and
    // the reason "can I descend into this" is a question of its own
    // rather than just `is_dir`. Kept separate from `is_dir` so an
    // archive can still be told apart on screen and, unlike a real
    // directory, still has a size worth showing.
    pub(crate) is_archive: bool,
    pub(crate) is_symlink: bool,
    pub(crate) is_exec: bool,
    // The synthetic ".." row -- always first, never selectable (there's
    // no sensible thing to hand back for "the parent directory" as part
    // of a multi-selection), and skipped entirely once a filter query is
    // typed, since it isn't a real match for anything.
    pub(crate) is_parent: bool,
    pub(crate) size: u64,
}

impl Entry {
    // Enter descends into this rather than choosing it.
    fn navigable(&self) -> bool {
        self.is_dir || self.is_archive
    }
}

// What one keystroke did, from the driving loop's point of view.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    // Stay in the browser and redraw.
    Continue,
    // Esc (outside search) or `q` -- the pane goes back to whatever it
    // was showing before, nothing chosen.
    Cancelled,
    // Enter on a file, or on a non-empty multi-selection. Paths are
    // absolute, and in a stable order (selection order is the set's own
    // sorted order, not click order -- see `selected`'s type).
    Accepted(Vec<PathBuf>),
    // Ctrl-Y: leave the browser and make *this* directory the shell's
    // own. Only ever produced when the caller said it has a shell to
    // change (see `set_can_change_directory`) -- browsing from `bish
    // tool edit` is a one-shot command line with no session to move.
    ChangeDirectory(PathBuf),
}

// The visible grid's own shape, resolved from a pane rect and whatever
// the current listing's widest label happens to be. `cols` is how many
// columns *fit on screen*, not how many the listing needs -- see
// `total_cols`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) col_width: usize,
}

// Blank columns between one grid column's text and the next.
const GUTTER: usize = 2;
// A column narrower than this is more annoying than useful (every name
// truncated to a couple of characters), so a very narrow pane gets
// fewer, wider columns instead -- possibly just one.
const MIN_COL_WIDTH: usize = 18;
// ...and a column wider than this is where `ls`'s uniform-column model
// stops being what you want: one 45-character filename in a directory
// of 8-character ones would otherwise set the width for *every* column,
// collapsing a 100-column pane to a single column. Found interactively
// -- the unit tests all passed with no cap at all, because none of them
// mixed one pathological name in with ordinary ones. Past this, the
// outliers get truncated (with an ellipsis) instead of the grid getting
// destroyed.
const MAX_COL_WIDTH: usize = 32;
// `mark` (1) + icon (2) + separating space (1). Fixed, so every name in
// a column starts at the same screen column regardless of its icon.
const PREFIX_WIDTH: usize = 4;

pub(crate) struct Browser {
    cwd: PathBuf,
    // Everything in `cwd` (plus the synthetic ".." row), already sorted
    // -- directories first, then files, each case-insensitively by name.
    entries: Vec<Entry>,
    // Indices into `entries`, in display order: the fuzzy filter's own
    // ranking when `query` is non-empty, otherwise just `0..entries.len()`.
    view: Vec<usize>,
    // Per-`view`-entry matched character positions (indices into that
    // entry's `name`), straight from `fuzzy::fuzzy_match` -- empty for
    // every entry while no query is typed.
    matches: Vec<Vec<usize>>,
    // Index into `view`, not `entries`.
    cursor: usize,
    // Leftmost grid column currently on screen.
    scroll_col: usize,
    // Keyed by absolute path rather than by index so it survives both
    // the filter narrowing under it and a directory change -- selecting
    // files from several directories and then hitting Enter is a real
    // file-browser gesture, not an accident to guard against. `BTreeSet`
    // (not `HashSet`) so `Accepted`'s own path list comes out in a
    // stable, reproducible order.
    selected: BTreeSet<PathBuf>,
    query: String,
    // `/` puts the keyboard into the filter input; Esc backs out of it
    // (clearing the query) rather than leaving the browser entirely.
    searching: bool,
    show_hidden: bool,
    // Back/forward stacks for Alt-Left/Alt-Right, mirroring the
    // vocabulary the ordinary shell prompt's own directory navigation
    // already uses (editor::DirNav).
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    // Whatever went wrong on the last attempted directory read, shown
    // in the status row until the next successful action.
    error: Option<String>,
    // Whether Ctrl-Y is offered at all -- see Outcome::ChangeDirectory.
    can_change_directory: bool,
}

impl Browser {
    // Opens `start` (or its nearest readable ancestor is *not* attempted
    // -- an unreadable start directory is a plain error, same as any
    // other failed `:` command).
    pub(crate) fn open(start: &Path) -> Result<Self, String> {
        let cwd = canonical(start)?;
        let mut b = Browser {
            cwd,
            entries: Vec::new(),
            view: Vec::new(),
            matches: Vec::new(),
            cursor: 0,
            scroll_col: 0,
            selected: BTreeSet::new(),
            query: String::new(),
            searching: false,
            show_hidden: false,
            back: Vec::new(),
            forward: Vec::new(),
            error: None,
            can_change_directory: false,
        };
        b.reload()?;
        Ok(b)
    }

    // Turns Ctrl-Y on. Off by default so a caller that has no shell to
    // move can't accidentally offer a key that would do nothing.
    pub(crate) fn set_can_change_directory(&mut self, allow: bool) {
        self.can_change_directory = allow;
    }

    // Re-reads `cwd` from disk and rebuilds the filtered view. Keeps the
    // cursor's *position* only in so far as it still exists -- callers
    // that want it on a specific name (going up to a parent, say) call
    // `focus_name` right after.
    fn reload(&mut self) -> Result<(), String> {
        let mut entries = self.read_here()?;
        // `Path::parent` already understands a virtual path: the parent
        // of `/a/b.zip!/dir` is `/a/b.zip!` and the parent of the
        // archive root `/a/b.zip!` is the real directory `/a`, which is
        // exactly the walk out of an archive this needs -- no separate
        // case, because `!` sits inside a path component rather than
        // acting as one.
        if let Some(parent) = self.cwd.parent() {
            entries.insert(
                0,
                Entry {
                    name: "..".to_string(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    is_archive: false,
                    is_symlink: false,
                    is_exec: false,
                    is_parent: true,
                    size: 0,
                },
            );
        }
        // Directories first (the ".." row is a directory too, and sorts
        // above everything by its own name), then case-insensitive by
        // name with a case-sensitive tiebreak so the order is total and
        // reproducible rather than read_dir's arbitrary one.
        entries.sort_by(|a, b| {
            b.is_parent
                .cmp(&a.is_parent)
                .then(b.navigable().cmp(&a.navigable()))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.entries = entries;
        self.refilter();
        Ok(())
    }

    // Everything in `cwd`, from whichever of the two things `cwd` can be.
    // The archive half is where `e some.zip` ends up: `cwd` is a virtual
    // path (archive::split), and the listing comes from the central
    // directory rather than from `read_dir`. Everything downstream of
    // here -- the grid, the filter, the selection set, Enter -- works on
    // `Entry` and never learns the difference.
    fn read_here(&self) -> Result<Vec<Entry>, String> {
        let here = self.cwd.to_string_lossy().into_owned();
        if let Some((archive, inner)) = crate::archive::split(&here) {
            let members = crate::archive::list(&archive)?;
            return Ok(crate::archive::list_dir(&members, &inner)
                .into_iter()
                .filter(|m| self.show_hidden || !m.name.starts_with('.'))
                .map(|m| Entry {
                    path: PathBuf::from(crate::archive::join(&archive, &join_inner(&inner, &m.name))),
                    name: m.name,
                    is_dir: m.is_dir,
                    // Nesting isn't supported (see archive::split), so an
                    // archive inside an archive is an ordinary file here.
                    is_archive: false,
                    is_symlink: false,
                    is_exec: false,
                    is_parent: false,
                    size: m.size,
                })
                .collect());
        }
        let read = std::fs::read_dir(&self.cwd).map_err(|e| format!("{}: {e}", self.cwd.display()))?;
        let mut entries = Vec::new();
        for dirent in read.flatten() {
            let name = dirent.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            entries.push(describe(dirent.path(), name));
        }
        Ok(entries)
    }

    // Rebuilds `view`/`matches` from `entries` + `query`, then clamps
    // the cursor. An empty query keeps the on-disk sort order (see
    // `fuzzy_match`'s own "empty query matches everything with score 0"
    // contract, plus a stable sort); a non-empty one re-ranks by score,
    // fzf-style, which deliberately breaks the directories-first
    // grouping -- when you're searching, rank is what you want, not
    // taxonomy.
    fn refilter(&mut self) {
        self.view.clear();
        self.matches.clear();
        if self.query.is_empty() {
            self.view.extend(0..self.entries.len());
            self.matches.resize(self.view.len(), Vec::new());
        } else {
            let mut scored: Vec<(i32, usize, Vec<usize>)> = Vec::new();
            for (i, e) in self.entries.iter().enumerate() {
                if e.is_parent {
                    continue;
                }
                if let Some(m) = fuzzy_match(&self.query, &e.name) {
                    scored.push((m.score, i, m.positions));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            for (_, i, positions) in scored {
                self.view.push(i);
                self.matches.push(positions);
            }
        }
        if self.cursor >= self.view.len() {
            self.cursor = self.view.len().saturating_sub(1);
        }
    }

    // Puts the cursor on the entry with this name, if it's still in the
    // filtered view -- used when going up to a parent, so the directory
    // just left is the one highlighted on arrival.
    fn focus_name(&mut self, name: &str) {
        if let Some(pos) = self.view.iter().position(|&i| self.entries[i].name == name) {
            self.cursor = pos;
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.view.get(self.cursor).map(|&i| &self.entries[i])
    }

    // The grid that fits in `rect`: one header row is reserved at the
    // top, everything below it is grid. Column width comes from the
    // widest label actually present (`ls`'s own uniform-column model),
    // clamped so one pathological filename can't collapse the grid to a
    // single column in a wide pane, and so a narrow pane still gets
    // usable columns.
    pub(crate) fn layout(&self, rect: Rect) -> Layout {
        let rows = rect.rows.saturating_sub(1).max(1);
        let widest = self.view.iter().map(|&i| PREFIX_WIDTH + str_width(&self.entries[i].name) + if self.entries[i].is_dir { 1 } else { 0 }).max().unwrap_or(0);
        let cols_avail = rect.cols.max(1);
        let col_width = (widest + GUTTER).clamp(MIN_COL_WIDTH.min(cols_avail), MAX_COL_WIDTH.min(cols_avail));
        let cols = (cols_avail / col_width).max(1);
        Layout { rows, cols, col_width }
    }

    // How many grid columns the whole (filtered) listing needs -- which
    // is usually more than `Layout::cols`, the number that fit.
    fn total_cols(&self, rows: usize) -> usize {
        self.view.len().div_ceil(rows.max(1))
    }

    // Scrolls the minimum amount needed to bring the cursor's own grid
    // column on screen. Called after every cursor move, and again at the
    // top of `render` so a resize that shrank the pane between two
    // keystrokes can't leave the cursor stranded off-screen.
    fn ensure_visible(&mut self, layout: Layout) {
        let col = self.cursor / layout.rows.max(1);
        if col < self.scroll_col {
            self.scroll_col = col;
        } else if col >= self.scroll_col + layout.cols {
            self.scroll_col = col + 1 - layout.cols;
        }
        let total = self.total_cols(layout.rows);
        let max_scroll = total.saturating_sub(layout.cols);
        self.scroll_col = self.scroll_col.min(max_scroll);
    }

    fn enter_dir(&mut self, path: PathBuf, remember: bool) {
        let previous = self.cwd.clone();
        let target = match canonical(&path) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let saved = std::mem::replace(&mut self.cwd, target);
        if let Err(e) = self.reload() {
            // Failed reads leave the browser exactly where it was rather
            // than stranding it in a directory it can't list.
            self.cwd = saved;
            self.error = Some(e);
            let _ = self.reload();
            return;
        }
        if remember {
            self.back.push(previous.clone());
            self.forward.clear();
        }
        self.error = None;
        self.query.clear();
        self.searching = false;
        self.cursor = 0;
        self.scroll_col = 0;
        self.refilter();
        // Coming *up* from a child: land on the child that was just
        // left. Stepping out of an archive root arrives here with
        // `some.zip!`, whose trailing separator is part of the virtual
        // path rather than of the filename it's listed under.
        if previous.parent() == Some(self.cwd.as_path())
            && let Some(name) = previous.file_name().map(|n| n.to_string_lossy().into_owned())
        {
            self.focus_name(name.trim_end_matches(crate::archive::SEPARATOR));
        }
    }

    fn go_parent(&mut self) {
        if let Some(parent) = self.cwd.parent().map(|p| p.to_path_buf()) {
            self.enter_dir(parent, true);
        }
    }

    fn toggle_selection(&mut self) {
        let Some(entry) = self.current() else { return };
        if entry.is_parent {
            return;
        }
        let path = entry.path.clone();
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
    }

    // Enter's own contract. Navigation wins over accepting whenever the
    // cursor is on a directory -- otherwise, once anything at all was
    // selected, Enter could never descend again, which would make
    // building a selection that spans several directories (the whole
    // point of keying `selected` by path) impossible. On a *file*, an
    // existing multi-selection is what gets returned -- that's what the
    // user built it for -- and only an empty selection falls back to
    // "just the one under the cursor".
    fn accept(&mut self) -> Outcome {
        let selection = || -> Outcome { Outcome::Accepted(self.selected.iter().cloned().collect()) };
        let Some(entry) = self.current() else {
            // Nothing under the cursor at all (an empty directory, or a
            // filter matching nothing) -- a selection built elsewhere is
            // still worth handing back.
            return if self.selected.is_empty() { Outcome::Continue } else { selection() };
        };
        if entry.navigable() {
            let path = entry.path.clone();
            self.enter_dir(path, true);
            return Outcome::Continue;
        }
        if !self.selected.is_empty() {
            return selection();
        }
        Outcome::Accepted(vec![entry.path.clone()])
    }

    pub(crate) fn handle_key(&mut self, key: Key, rect: Rect) -> Outcome {
        let layout = self.layout(rect);
        let page = layout.rows * layout.cols;
        let last = self.view.len().saturating_sub(1);
        self.error = None;

        // The filter input owns every printable key while it's active,
        // so only the keys that *aren't* text fall through to the
        // navigation vocabulary below. Arrows/Ctrl-N/Ctrl-P still move
        // the grid cursor while typing, fzf-style -- filtering and
        // picking are one gesture, not two modes.
        if self.searching {
            match key {
                Key::Char(c) => {
                    self.query.push(c);
                    self.cursor = 0;
                    self.scroll_col = 0;
                    self.refilter();
                    return Outcome::Continue;
                }
                Key::Backspace => {
                    if self.query.pop().is_none() {
                        self.searching = false;
                    }
                    self.cursor = 0;
                    self.scroll_col = 0;
                    self.refilter();
                    return Outcome::Continue;
                }
                // Esc backs out of the filter (clearing it) rather than
                // out of the browser -- one Esc per level, matching how
                // command mode's own Esc handling already behaves.
                Key::Escape => {
                    self.searching = false;
                    self.query.clear();
                    self.cursor = 0;
                    self.scroll_col = 0;
                    self.refilter();
                    return Outcome::Continue;
                }
                // Commits the filter and hands the keyboard back to the
                // grid, *keeping* what's typed -- so `/` `r` `s` Enter
                // leaves a narrowed listing to navigate, rather than
                // immediately opening the top hit.
                Key::Enter => {
                    self.searching = false;
                    return Outcome::Continue;
                }
                Key::CtrlN | Key::Down => {
                    self.cursor = (self.cursor + 1).min(last);
                    self.ensure_visible(layout);
                    return Outcome::Continue;
                }
                Key::CtrlP | Key::Up => {
                    self.cursor = self.cursor.saturating_sub(1);
                    self.ensure_visible(layout);
                    return Outcome::Continue;
                }
                Key::Tab => {
                    self.toggle_selection();
                    self.cursor = (self.cursor + 1).min(last);
                    self.ensure_visible(layout);
                    return Outcome::Continue;
                }
                _ => {}
            }
        }

        match key {
            Key::Escape | Key::Char('q') | Key::CtrlC => Outcome::Cancelled,
            Key::Enter => self.accept(),
            // Ctrl-Y: take the shell here. The directory is the one
            // being *browsed*, not whatever the cursor happens to be on
            // -- Enter already descends, so you walk to where you want
            // and then bring the shell along.
            Key::CtrlY if self.can_change_directory => {
                if crate::archive::split(&self.cwd.to_string_lossy()).is_some() {
                    // An archive member is not a directory anything can
                    // have as its working directory.
                    self.error = Some("inside an archive -- nothing to cd into".to_string());
                    return Outcome::Continue;
                }
                Outcome::ChangeDirectory(self.cwd.clone())
            }
            Key::Char('/') => {
                self.searching = true;
                Outcome::Continue
            }
            // Toggle-and-advance, the gesture every file browser with
            // multi-selection uses (ranger's Space, fzf's Tab): holding
            // Tab down walks a run of files into the selection.
            Key::Tab | Key::Char(' ') => {
                self.toggle_selection();
                self.cursor = (self.cursor + 1).min(last);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('v') => {
                for &i in &self.view {
                    let e = &self.entries[i];
                    if e.is_parent {
                        continue;
                    }
                    if !self.selected.remove(&e.path) {
                        self.selected.insert(e.path.clone());
                    }
                }
                Outcome::Continue
            }
            Key::CtrlA => {
                for &i in &self.view {
                    let e = &self.entries[i];
                    if !e.is_parent {
                        self.selected.insert(e.path.clone());
                    }
                }
                Outcome::Continue
            }
            // Clears the selection without leaving -- Esc is already
            // spoken for (leave/exit-search), so this gets its own key
            // rather than overloading that further.
            Key::Char('u') => {
                self.selected.clear();
                Outcome::Continue
            }
            Key::Char('j') | Key::Down | Key::CtrlN => {
                self.cursor = (self.cursor + 1).min(last);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('k') | Key::Up | Key::CtrlP => {
                self.cursor = self.cursor.saturating_sub(1);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('l') | Key::Right => {
                self.cursor = (self.cursor + layout.rows).min(last);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('h') | Key::Left => {
                self.cursor = self.cursor.saturating_sub(layout.rows);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::PageDown | Key::CtrlF => {
                self.cursor = (self.cursor + page).min(last);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::PageUp | Key::CtrlB => {
                self.cursor = self.cursor.saturating_sub(page);
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('g') | Key::Home => {
                self.cursor = 0;
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Char('G') | Key::End => {
                self.cursor = last;
                self.ensure_visible(layout);
                Outcome::Continue
            }
            Key::Backspace | Key::Char('-') | Key::AltUp => {
                self.go_parent();
                self.ensure_visible(self.layout(rect));
                Outcome::Continue
            }
            Key::AltLeft => {
                if let Some(prev) = self.back.pop() {
                    let here = self.cwd.clone();
                    self.enter_dir(prev, false);
                    self.forward.push(here);
                }
                Outcome::Continue
            }
            Key::AltRight => {
                if let Some(next) = self.forward.pop() {
                    let here = self.cwd.clone();
                    self.enter_dir(next, false);
                    self.back.push(here);
                }
                Outcome::Continue
            }
            Key::Char('~') => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.enter_dir(PathBuf::from(home), true);
                }
                Outcome::Continue
            }
            Key::Char('.') => {
                self.show_hidden = !self.show_hidden;
                let name = self.current().map(|e| e.name.clone());
                if let Err(e) = self.reload() {
                    self.error = Some(e);
                }
                if let Some(name) = name {
                    self.focus_name(&name);
                }
                self.ensure_visible(self.layout(rect));
                Outcome::Continue
            }
            Key::Mouse(ev) => {
                if ev.is_scroll_down() {
                    self.scroll_col = (self.scroll_col + 1).min(self.total_cols(layout.rows).saturating_sub(1));
                } else if ev.is_scroll_up() {
                    self.scroll_col = self.scroll_col.saturating_sub(1);
                } else if ev.is_left_click()
                    && let Some(idx) = self.hit_test(ev.row as usize, ev.col as usize, rect, layout)
                {
                    self.cursor = idx;
                    self.ensure_visible(layout);
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    // Which view index a 1-indexed terminal cell belongs to, if any.
    // Rejects the header row and any cell past the end of the listing,
    // so clicking empty space is a no-op rather than snapping the cursor
    // somewhere arbitrary.
    fn hit_test(&self, term_row: usize, term_col: usize, rect: Rect, layout: Layout) -> Option<usize> {
        let row0 = term_row.checked_sub(1)?;
        let col0 = term_col.checked_sub(1)?;
        let grid_row = row0.checked_sub(rect.row + 1)?;
        let grid_col_px = col0.checked_sub(rect.col)?;
        if grid_row >= layout.rows || grid_col_px >= layout.cols * layout.col_width {
            return None;
        }
        let idx = (self.scroll_col + grid_col_px / layout.col_width) * layout.rows + grid_row;
        (idx < self.view.len()).then_some(idx)
    }

    // The whole frame: the pane's own header + grid, plus the terminal's
    // one global status row (`repl::render_global_status_row` -- the
    // same row the file editor's own status line uses, and the row
    // command mode's `:browse` text is still sitting on until this
    // overwrites it). Ends by parking the real cursor: inside the filter
    // input while searching (visible, so typing has somewhere to look),
    // hidden otherwise.
    pub(crate) fn render(&mut self, rect: Rect, term_rows: usize, term_cols: usize) -> String {
        let layout = self.layout(rect);
        self.ensure_visible(layout);

        let mut out = String::new();
        out.push_str("\x1b[?25l");
        out.push_str(&format!("\x1b[{};{}H", rect.row + 1, rect.col + 1));
        out.push_str("\x1b[1m");
        out.push_str(&pad_to(&self.header_text(rect.cols), rect.cols));
        out.push_str("\x1b[0m");

        for grid_row in 0..layout.rows {
            out.push_str(&format!("\x1b[{};{}H", rect.row + grid_row + 2, rect.col + 1));
            let mut painted = 0;
            for grid_col in 0..layout.cols {
                let width = layout.col_width.min(rect.cols.saturating_sub(painted));
                if width == 0 {
                    break;
                }
                let idx = (self.scroll_col + grid_col) * layout.rows + grid_row;
                out.push_str(&self.render_cell(idx, width));
                painted += width;
            }
            if painted < rect.cols {
                out.push_str(&" ".repeat(rect.cols - painted));
            }
        }

        out.push_str(&crate::repl::render_global_status_row(&pad_to(&self.status_text(term_cols), term_cols), term_rows));
        if self.searching {
            // The `/` and the query both live in the status row, so the
            // cursor goes right after the last typed character.
            let col = 2 + str_width(&self.query);
            out.push_str(&format!("\x1b[{};{}H\x1b[?25h", term_rows.saturating_sub(1), col.min(term_cols)));
        }
        out
    }

    fn render_cell(&self, idx: usize, width: usize) -> String {
        let Some(&entry_idx) = self.view.get(idx) else {
            return " ".repeat(width);
        };
        let entry = &self.entries[entry_idx];
        let selected = self.selected.contains(&entry.path);
        let focused = idx == self.cursor;

        // A directory's `/` and an archive's `!` are the same hint:
        // Enter goes *into* this. `!` because that's exactly what the
        // path gains when you do (archive::SEPARATOR).
        let label = match entry {
            e if e.is_parent => e.name.clone(),
            e if e.is_dir => format!("{}/", e.name),
            e if e.is_archive => format!("{}{}", e.name, crate::archive::SEPARATOR),
            e => e.name.clone(),
        };
        let name_width = width.saturating_sub(PREFIX_WIDTH + GUTTER.min(width.saturating_sub(PREFIX_WIDTH)));
        let empty = Vec::new();
        let positions = self.matches.get(idx).unwrap_or(&empty);
        let (pieces, used) = fit_marked(&label, positions, name_width);

        let mut out = String::new();
        if focused {
            out.push_str("\x1b[7m");
        }
        out.push_str(color_for(entry));
        out.push(if selected { '\u{2022}' } else { ' ' });
        out.push(icon_for(entry));
        out.push(' ');
        for (piece, matched) in &pieces {
            if *matched {
                out.push_str("\x1b[4m");
                out.push_str(piece);
                out.push_str("\x1b[24m");
            } else {
                out.push_str(piece);
            }
        }
        let painted = PREFIX_WIDTH + used;
        if painted < width {
            out.push_str(&" ".repeat(width - painted));
        }
        out.push_str("\x1b[0m");
        out
    }

    // `📁 ~/bish/src` on the left, counts on the right. The path is
    // truncated from the *left* (keeping the leaf, which is what
    // identifies where you are) rather than from the right like every
    // other truncation here.
    fn header_text(&self, cols: usize) -> String {
        // While a filter is active the raw count on its own is
        // misleading ("5 items" in a directory of 22) -- say what it's
        // five *of*.
        let shown = self.view.len();
        let mut right = if self.query.is_empty() {
            format!("{shown} item{}", if shown == 1 { "" } else { "s" })
        } else {
            format!("{shown} of {}", self.entries.len())
        };
        if !self.selected.is_empty() {
            right.push_str(&format!("  \u{2022}{} selected", self.selected.len()));
        }
        let path = shorten_home(&self.cwd);
        let icon_and_gap = 3;
        let room = cols.saturating_sub(icon_and_gap + str_width(&right) + GUTTER);
        let path = fit_left(&path, room);
        let left = format!("\u{1F4C1} {path}");
        let used = str_width(&left) + str_width(&right);
        if used + GUTTER > cols {
            fit(&left, cols)
        } else {
            format!("{left}{}{right}", " ".repeat(cols - used))
        }
    }

    fn status_text(&self, cols: usize) -> String {
        if self.searching {
            return fit(&format!("\u{1F50D} {}", self.query), cols);
        }
        if let Some(err) = &self.error {
            return fit(&format!("\u{26A0} {err}"), cols);
        }
        let detail = match self.current() {
            Some(e) if e.is_parent => "parent directory".to_string(),
            Some(e) if e.is_dir => format!("{}/", e.name),
            Some(e) => format!("{}  {}", e.name, human_size(e.size)),
            None => if self.query.is_empty() { "empty directory".to_string() } else { format!("no matches for '{}'", self.query) },
        };
        let hints = if self.can_change_directory {
            "enter open  ^y cd here  tab select  / filter  . hidden  bksp up  esc back"
        } else {
            "enter open  tab select  / filter  . hidden  bksp up  esc back"
        };
        let used = str_width(&detail) + str_width(hints);
        if used + GUTTER > cols {
            fit(&detail, cols)
        } else {
            format!("{detail}{}{hints}", " ".repeat(cols - used))
        }
    }
}

// Everything below is `Browser`-independent helper machinery.

// Where a browse should start: the argument if there is one (relative
// paths resolved against the *session's* own cwd, not the process's --
// each window/pane tracks its own), otherwise that cwd itself. `~`/
// `~/...` expand here too, which is redundant for an `e` argument the
// shell already expanded but harmless, and keeps this usable by any
// caller with a raw path in hand.
pub(crate) fn resolve_start(cwd: &Path, arg: Option<&str>) -> PathBuf {
    let Some(arg) = arg else { return cwd.to_path_buf() };
    let expanded = if arg == "~" || arg.starts_with("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(arg.trim_start_matches('~').trim_start_matches('/')),
            None => PathBuf::from(arg),
        }
    } else {
        PathBuf::from(arg)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

// The absolute, symlink-free spelling of wherever the browser is about
// to sit -- extended to the two archive cases, since neither names
// anything `fs::canonicalize` could resolve on its own.
//
// Naming an archive *file* lands at its root rather than failing: `e
// some.zip` and Enter on `some.zip` both arrive here, and both mean
// "show me what's inside", so the normalization belongs in one place
// rather than at each of them.
fn canonical(path: &Path) -> Result<PathBuf, String> {
    let raw = path.to_string_lossy().into_owned();
    if let Some((archive, inner)) = crate::archive::split(&raw) {
        let archive = std::fs::canonicalize(&archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        return Ok(PathBuf::from(crate::archive::join(&archive, &inner)));
    }
    let real = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if crate::archive::kind_of(&real) == Some(crate::archive::Kind::Zip) {
        return Ok(PathBuf::from(crate::archive::join(&real, "")));
    }
    Ok(real)
}

fn describe(path: PathBuf, name: String) -> Entry {
    use std::os::unix::fs::PermissionsExt;
    // `symlink_metadata` first (does *this* name point at a link?), then
    // the followed `metadata` for what it actually resolves to -- a
    // symlink to a directory should still navigate like a directory, it
    // just gets the link icon.
    let link = std::fs::symlink_metadata(&path).ok();
    let meta = std::fs::metadata(&path).ok();
    let is_symlink = link.as_ref().map(|m| m.file_type().is_symlink()).unwrap_or(false);
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let is_exec = meta.as_ref().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) && !is_dir;
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    // By magic bytes, not by name (crate::archive::kind_of) -- so a zip
    // called `.jar`, `.whl` or nothing at all still opens, and a `.zip`
    // that isn't one doesn't pretend to. That's an open and a 4-byte
    // read per file on top of the two stats this already does, which is
    // fine for the directories a person browses and would be worth
    // revisiting only if listing something like /usr/bin ever felt slow.
    let is_archive = !is_dir && crate::archive::kind_of(&path) == Some(crate::archive::Kind::Zip);
    Entry { name, path, is_dir, is_archive, is_symlink, is_exec, is_parent: false, size }
}

// Joins a directory path inside an archive with one of its children --
// plain string work, since these are archive member names ('/'-separated
// by the format's own definition) rather than host paths.
fn join_inner(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn color_for(e: &Entry) -> &'static str {
    if e.is_symlink {
        "\x1b[36m"
    } else if e.is_archive {
        // Its own colour rather than a directory's blue: it navigates
        // like one, but it's still a single file on disk and everything
        // inside it is read-only.
        "\x1b[35m"
    } else if e.is_dir {
        "\x1b[1;34m"
    } else if e.is_exec {
        "\x1b[32m"
    } else {
        "\x1b[39m"
    }
}

// Every icon here is drawn from U+1F300..=U+1FAFF on purpose -- that's
// the one block `unicode_width::char_width` reports as width 2, which is
// what this module's whole column arithmetic assumes. Picking a
// pictograph from, say, Miscellaneous Symbols (U+2600..) instead would
// measure as 1 column here while most terminals still draw it as 2, and
// every column to its right would land one cell off. Enforced by
// `every_icon_is_two_columns_wide` below rather than left to care.
const BY_EXTENSION: &[(&[&str], char)] = &[
    (&["rs"], '\u{1F980}'),
    (&["md", "markdown", "rst", "adoc", "org"], '\u{1F4DD}'),
    (&["txt", "log", "text"], '\u{1F4C4}'),
    (&["json", "toml", "yaml", "yml", "ini", "conf", "cfg", "env", "properties"], '\u{1F527}'),
    (&["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tiff"], '\u{1F3A8}'),
    (&["mp3", "wav", "flac", "ogg", "m4a", "opus", "aac"], '\u{1F3B5}'),
    (&["mp4", "mkv", "mov", "avi", "webm", "m4v"], '\u{1F3AC}'),
    (&["zip", "tar", "gz", "tgz", "xz", "bz2", "zst", "7z", "rar"], '\u{1F4E6}'),
    (&["pdf", "epub", "mobi", "djvu"], '\u{1F4D5}'),
    (&["sh", "bash", "zsh", "fish", "bish"], '\u{1F41A}'),
    (&["py", "pyi", "pyc"], '\u{1F40D}'),
    (&["js", "mjs", "cjs", "ts", "tsx", "jsx"], '\u{1F4DC}'),
    (&["c", "h", "cc", "cpp", "hpp", "cxx", "hh"], '\u{1F528}'),
    (&["html", "htm", "css", "scss", "less", "xml"], '\u{1F310}'),
    (&["lock"], '\u{1F512}'),
    (&["rb"], '\u{1F48E}'),
    (&["go"], '\u{1F439}'),
    (&["java", "class", "jar", "kt"], '\u{1F375}'),
    (&["db", "sqlite", "sqlite3", "sql"], '\u{1F4BE}'),
    (&["csv", "tsv", "xlsx", "xls"], '\u{1F4CA}'),
    (&["pem", "key", "crt", "cer", "gpg", "asc"], '\u{1F511}'),
    (&["o", "a", "so", "dylib", "bin", "exe", "wasm"], '\u{1F9F1}'),
    (&["patch", "diff"], '\u{1FA79}'),
    (&["ttf", "otf", "woff", "woff2"], '\u{1F524}'),
];

// Extension-less files that are still instantly recognizable by name.
const BY_NAME: &[(&str, char)] = &[
    ("makefile", '\u{1F528}'),
    ("dockerfile", '\u{1F433}'),
    ("license", '\u{1F4DC}'),
    ("readme", '\u{1F4DD}'),
];

fn icon_for(e: &Entry) -> char {
    if e.is_parent {
        return '\u{1F4C2}';
    }
    if e.is_symlink {
        return '\u{1F517}';
    }
    if e.is_dir {
        return '\u{1F4C1}';
    }
    // Ahead of the name-based tables below, since is_archive comes from
    // the file's actual magic bytes -- a zip called `plugin.vsix` should
    // still look like the archive it is.
    if e.is_archive {
        return '\u{1F4E6}';
    }
    let lower = e.name.to_lowercase();
    let stem = lower.split('.').next().unwrap_or("");
    if let Some((_, icon)) = BY_NAME.iter().find(|(n, _)| *n == lower || *n == stem) {
        return *icon;
    }
    if let Some(ext) = lower.rsplit_once('.').map(|(_, ext)| ext)
        && let Some((_, icon)) = BY_EXTENSION.iter().find(|(exts, _)| exts.contains(&ext))
    {
        return *icon;
    }
    if e.is_exec {
        return '\u{1F680}';
    }
    '\u{1F4C4}'
}

fn shorten_home(path: &Path) -> String {
    let text = path.display().to_string();
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path == home => "~".to_string(),
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => text,
        },
        None => text,
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

// Pad with spaces to exactly `width` display columns (truncating first
// if it's already wider). Display columns, not chars -- an emoji icon or
// a CJK filename is two of them.
fn pad_to(s: &str, width: usize) -> String {
    let fitted = fit(s, width);
    let used = str_width(&fitted);
    format!("{fitted}{}", " ".repeat(width.saturating_sub(used)))
}

// Truncate to `width` display columns, marking the cut with an ellipsis.
// Cluster-aware (`grapheme::next_boundary`), so this can't slice a ZWJ
// emoji sequence or a combining-mark cluster in half the way a plain
// `chars().take(n)` would.
fn fit(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "\u{2026}".to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut used = 0;
    let mut i = 0;
    while i < chars.len() {
        let end = grapheme::next_boundary(&chars, i);
        let w = char_width(chars[i]);
        if used + w > width - 1 {
            break;
        }
        out.extend(chars[i..end].iter());
        used += w;
        i = end;
    }
    out.push('\u{2026}');
    out
}

// Same, but keeping the *tail* -- for a path, the leaf identifies where
// you are far better than the root does.
fn fit_left(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "\u{2026}".to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut start = chars.len();
    let mut used = 0;
    while start > 0 {
        let prev = grapheme::prev_boundary(&chars, start);
        let w = char_width(chars[prev]);
        if used + w > width - 1 {
            break;
        }
        used += w;
        start = prev;
    }
    format!("\u{2026}{}", chars[start..].iter().collect::<String>())
}

// Truncate `s` to `width` columns like `fit`, but returning it broken
// into (cluster, is-a-fuzzy-match) pieces so the renderer can underline
// exactly the characters the query matched. `positions` are char indices
// into `s` (`fuzzy::FuzzyMatch::positions`' own convention); a cluster
// counts as matched when its own first char is one of them. Returns the
// pieces plus the display width they actually consumed.
fn fit_marked(s: &str, positions: &[usize], width: usize) -> (Vec<(String, bool)>, usize) {
    let chars: Vec<char> = s.chars().collect();
    let truncating = str_width(s) > width;
    let budget = if truncating { width.saturating_sub(1) } else { width };
    let mut pieces: Vec<(String, bool)> = Vec::new();
    let mut used = 0;
    let mut i = 0;
    while i < chars.len() {
        let end = grapheme::next_boundary(&chars, i);
        let w = char_width(chars[i]);
        if used + w > budget {
            break;
        }
        pieces.push((chars[i..end].iter().collect(), positions.contains(&i)));
        used += w;
        i = end;
    }
    if truncating && used < width {
        pieces.push(("\u{2026}".to_string(), false));
        used += 1;
    }
    (pieces, used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn rect(rows: usize, cols: usize) -> Rect {
        Rect { row: 0, col: 0, rows, cols }
    }

    // A throwaway directory tree under the crate's own target dir --
    // same "no external crate, no /tmp assumptions beyond std" spirit as
    // the rest of this codebase's filesystem tests.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let dir = std::env::temp_dir().join(format!("bish-browser-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::write(&p, contents).unwrap();
            p
        }
        fn dir(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // The invariant the whole column layout rests on: every icon must
    // measure exactly two columns by *this* codebase's own width table,
    // because that's what the cell arithmetic assumes. A pictograph from
    // outside the wide block would measure 1 here while terminals draw
    // it as 2, shifting every column to its right by one cell -- exactly
    // the class of silent, spot-check-proof table bug the grapheme
    // module's own sortedness test exists to catch.
    #[test]
    fn every_icon_is_two_columns_wide() {
        let mut icons: Vec<char> = vec!['\u{1F4C1}', '\u{1F4C2}', '\u{1F517}', '\u{1F4C4}', '\u{1F680}', '\u{1F50D}'];
        icons.extend(BY_EXTENSION.iter().map(|(_, c)| *c));
        icons.extend(BY_NAME.iter().map(|(_, c)| *c));
        for icon in icons {
            assert_eq!(char_width(icon), 2, "icon U+{:04X} is not two columns wide", icon as u32);
        }
    }

    #[test]
    fn listing_puts_directories_first_with_parent_at_the_top() {
        let t = Tmp::new("sort");
        t.file("zebra.txt", "");
        t.file("apple.rs", "");
        t.dir("src");
        t.dir("Bin");
        let b = Browser::open(&t.0).unwrap();
        let names: Vec<&str> = b.view.iter().map(|&i| b.entries[i].name.as_str()).collect();
        assert_eq!(names, vec!["..", "Bin", "src", "apple.rs", "zebra.txt"]);
    }

    #[test]
    fn hidden_files_are_skipped_until_dot_toggles_them() {
        let t = Tmp::new("hidden");
        t.file("visible.txt", "");
        t.file(".secret", "");
        let mut b = Browser::open(&t.0).unwrap();
        assert!(!b.view.iter().any(|&i| b.entries[i].name == ".secret"));
        b.handle_key(Key::Char('.'), rect(10, 80));
        assert!(b.view.iter().any(|&i| b.entries[i].name == ".secret"));
    }

    // Column-major fill: with 4 grid rows, `l` from the first item lands
    // on the fifth -- the top of the next column -- not the second.
    #[test]
    fn right_moves_a_whole_column_not_one_item() {
        let t = Tmp::new("grid");
        for i in 0..12 {
            t.file(&format!("f{i:02}.txt"), "");
        }
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(5, 80); // 1 header row + 4 grid rows
        assert_eq!(b.layout(r).rows, 4);
        assert_eq!(b.cursor, 0);
        b.handle_key(Key::Char('l'), r);
        assert_eq!(b.cursor, 4);
        b.handle_key(Key::Char('j'), r);
        assert_eq!(b.cursor, 5);
        b.handle_key(Key::Char('h'), r);
        assert_eq!(b.cursor, 1);
    }

    // The bug the MAX_COL_WIDTH cap exists for, found interactively:
    // one very long name among ordinary ones used to set the uniform
    // column width for the whole grid, leaving a single column in a
    // pane wide enough for three.
    #[test]
    fn one_long_filename_does_not_collapse_the_grid_to_one_column() {
        let t = Tmp::new("longname");
        t.file("a-really-quite-extremely-long-file-name-for-testing.json", "");
        for i in 0..10 {
            t.file(&format!("s{i}.rs"), "");
        }
        let b = Browser::open(&t.0).unwrap();
        let layout = b.layout(rect(10, 100));
        assert!(layout.cols >= 3, "expected several columns in a 100-column pane, got {layout:?}");
    }

    #[test]
    fn cursor_never_moves_past_the_last_entry() {
        let t = Tmp::new("clamp");
        t.file("only.txt", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        let last = b.view.len() - 1;
        for _ in 0..20 {
            b.handle_key(Key::Char('l'), r);
            b.handle_key(Key::Char('j'), r);
        }
        assert_eq!(b.cursor, last);
        for _ in 0..20 {
            b.handle_key(Key::Char('h'), r);
            b.handle_key(Key::Char('k'), r);
        }
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn scrolling_follows_the_cursor_off_the_right_edge() {
        let t = Tmp::new("scroll");
        for i in 0..40 {
            t.file(&format!("file{i:02}.txt"), "");
        }
        let mut b = Browser::open(&t.0).unwrap();
        // Narrow enough that only a couple of columns fit at once.
        let r = rect(5, 40);
        let layout = b.layout(r);
        assert_eq!(b.scroll_col, 0);
        b.handle_key(Key::Char('G'), r);
        assert!(b.scroll_col > 0, "cursor at the end should have scrolled");
        let cursor_col = b.cursor / layout.rows;
        assert!(cursor_col >= b.scroll_col && cursor_col < b.scroll_col + layout.cols, "cursor column {cursor_col} outside visible {}..{}", b.scroll_col, b.scroll_col + layout.cols);
        b.handle_key(Key::Char('g'), r);
        assert_eq!(b.scroll_col, 0);
    }

    #[test]
    fn slash_filters_with_fuzzy_matching_and_escape_restores_everything() {
        let t = Tmp::new("filter");
        t.file("main.rs", "");
        t.file("browser.rs", "");
        t.file("notes.md", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        let all = b.view.len();
        b.handle_key(Key::Char('/'), r);
        for c in "brs".chars() {
            b.handle_key(Key::Char(c), r);
        }
        let names: Vec<&str> = b.view.iter().map(|&i| b.entries[i].name.as_str()).collect();
        assert_eq!(names, vec!["browser.rs"], "fuzzy 'brs' should match only browser.rs");
        b.handle_key(Key::Escape, r);
        assert_eq!(b.view.len(), all);
        assert!(!b.searching);
    }

    // Esc has two levels: out of the filter first, out of the browser
    // only once there's no filter left to back out of.
    #[test]
    fn escape_leaves_the_filter_before_it_leaves_the_browser() {
        let t = Tmp::new("escape");
        t.file("a.txt", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.handle_key(Key::Char('/'), r);
        assert_eq!(b.handle_key(Key::Escape, r), Outcome::Continue);
        assert_eq!(b.handle_key(Key::Escape, r), Outcome::Cancelled);
    }

    #[test]
    fn the_parent_row_can_never_be_selected() {
        let t = Tmp::new("parentsel");
        t.file("a.txt", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        assert!(b.current().unwrap().is_parent);
        b.handle_key(Key::Tab, r);
        assert!(b.selected.is_empty());
        // ...but it did advance, so the next Tab selects a real file.
        b.handle_key(Key::Tab, r);
        assert_eq!(b.selected.len(), 1);
    }

    #[test]
    fn tab_builds_a_multi_selection_that_enter_returns() {
        let t = Tmp::new("multi");
        t.file("a.txt", "");
        t.file("b.txt", "");
        t.file("c.txt", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.handle_key(Key::Char('j'), r); // off ".."
        b.handle_key(Key::Tab, r); // a.txt, advance
        b.handle_key(Key::Tab, r); // b.txt, advance
        match b.handle_key(Key::Enter, r) {
            Outcome::Accepted(paths) => {
                let names: Vec<String> = paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
                assert_eq!(names, vec!["a.txt", "b.txt"]);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn a_selection_survives_changing_directory() {
        let t = Tmp::new("crossdir");
        t.file("top.txt", "");
        let sub = t.dir("sub");
        fs::write(sub.join("inner.txt"), "").unwrap();
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.focus_name("top.txt");
        b.handle_key(Key::Tab, r);
        b.focus_name("sub");
        b.handle_key(Key::Enter, r);
        assert_eq!(b.cwd.file_name().unwrap(), "sub");
        b.focus_name("inner.txt");
        b.handle_key(Key::Tab, r);
        match b.handle_key(Key::Enter, r) {
            Outcome::Accepted(paths) => assert_eq!(paths.len(), 2, "{paths:?}"),
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_a_directory_descends_and_backspace_comes_back_to_it() {
        let t = Tmp::new("descend");
        let sub = t.dir("nested");
        fs::write(sub.join("deep.txt"), "").unwrap();
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.focus_name("nested");
        assert_eq!(b.handle_key(Key::Enter, r), Outcome::Continue);
        assert_eq!(b.cwd.file_name().unwrap(), "nested");
        b.handle_key(Key::Backspace, r);
        assert_eq!(b.cwd, fs::canonicalize(&t.0).unwrap());
        // Landed back *on* the directory just left, not at the top.
        assert_eq!(b.current().unwrap().name, "nested");
    }

    #[test]
    fn enter_on_a_file_accepts_just_that_file() {
        let t = Tmp::new("accept");
        t.file("chosen.txt", "");
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.focus_name("chosen.txt");
        match b.handle_key(Key::Enter, r) {
            Outcome::Accepted(paths) => {
                assert_eq!(paths.len(), 1);
                assert_eq!(paths[0].file_name().unwrap(), "chosen.txt");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    #[test]
    fn alt_left_and_right_walk_the_directory_history() {
        let t = Tmp::new("history");
        let a = t.dir("alpha");
        fs::create_dir_all(a.join("beta")).unwrap();
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        b.focus_name("alpha");
        b.handle_key(Key::Enter, r);
        assert_eq!(b.cwd.file_name().unwrap(), "alpha");
        b.handle_key(Key::AltLeft, r);
        assert_eq!(b.cwd, fs::canonicalize(&t.0).unwrap());
        b.handle_key(Key::AltRight, r);
        assert_eq!(b.cwd.file_name().unwrap(), "alpha");
    }

    // Rendering is mostly escape sequences, but the one thing worth
    // asserting directly is that no row it emits is wider than the rect
    // it was given -- an over-wide row wraps and corrupts every pane
    // below it.
    #[test]
    fn no_rendered_row_overflows_the_pane_rect() {
        let t = Tmp::new("width");
        t.file("a-really-quite-extremely-long-file-name-here.rs", "");
        t.file("short.md", "");
        t.dir("dir-with-a-long-name-too");
        let mut b = Browser::open(&t.0).unwrap();
        for cols in [20usize, 40, 80] {
            let r = Rect { row: 2, col: 5, rows: 6, cols };
            let frame = b.render(r, 24, 100);
            for row in visible_rows(&frame) {
                assert!(str_width(&row) <= cols, "row {:?} is {} wide, rect is {cols}", row, str_width(&row));
            }
        }
    }

    // Splits a rendered frame into the plain text painted at each
    // cursor-position escape, dropping SGR codes -- enough to measure
    // row widths without reimplementing a terminal.
    fn visible_rows(frame: &str) -> Vec<String> {
        let mut rows = Vec::new();
        let mut current = String::new();
        let mut chars = frame.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                let mut params = String::new();
                let mut final_byte = ' ';
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        final_byte = c2;
                        break;
                    }
                    params.push(c2);
                }
                if final_byte == 'H' {
                    if !current.is_empty() {
                        rows.push(std::mem::take(&mut current));
                    }
                    // A move to the global status row ends the pane's
                    // own rows -- that row is terminal-wide by design.
                    if params.split(';').next().map(|r| r.parse::<usize>().unwrap_or(0)).unwrap_or(0) > 20 {
                        break;
                    }
                }
                continue;
            }
            current.push(c);
        }
        if !current.is_empty() {
            rows.push(current);
        }
        rows
    }

    #[test]
    fn fit_truncates_on_cluster_boundaries_not_mid_emoji() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let s = format!("a{family}bcdef");
        let out = fit(&s, 4);
        assert!(str_width(&out) <= 4, "{out:?}");
        // Either the whole cluster made it or none of it did -- never a
        // lone ZWJ or half a joined sequence.
        assert!(!out.contains('\u{200D}') || out.contains('\u{1F467}'), "{out:?}");
    }

    #[test]
    fn fit_left_keeps_the_tail_of_a_path() {
        assert_eq!(fit_left("/home/jussi/bish/src", 10), "\u{2026}/bish/src");
        assert_eq!(fit_left("/short", 20), "/short");
    }

    #[test]
    fn resolve_start_handles_relative_absolute_and_home() {
        let cwd = Path::new("/home/jussi/bish");
        assert_eq!(resolve_start(cwd, None), PathBuf::from("/home/jussi/bish"));
        assert_eq!(resolve_start(cwd, Some("src")), PathBuf::from("/home/jussi/bish/src"));
        assert_eq!(resolve_start(cwd, Some("/etc")), PathBuf::from("/etc"));
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(resolve_start(cwd, Some("~")), PathBuf::from(&home));
            assert_eq!(resolve_start(cwd, Some("~/x")), PathBuf::from(&home).join("x"));
        }
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0K");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0M");
    }

    // The archive tests below all browse this one real zip (the same
    // fixture archive.rs's own tests use, so what it contains is
    // documented in one place): notes.txt at the root, dir/inner.json,
    // and dir/deep/leaf.txt, with no explicit entries for the
    // directories.
    fn zip_in(tmp: &Tmp) -> PathBuf {
        let path = tmp.0.join("sample.zip");
        fs::write(&path, include_bytes!("testdata/sample.zip")).unwrap();
        path
    }

    fn names(b: &Browser) -> Vec<String> {
        b.view.iter().map(|&i| b.entries[i].name.clone()).collect()
    }

    #[test]
    fn an_archive_is_listed_as_navigable_rather_than_as_a_plain_file() {
        let tmp = Tmp::new("archive-entry");
        zip_in(&tmp);
        tmp.file("plain.txt", "hello\n");
        let b = Browser::open(&tmp.0).unwrap();
        let zip = b.entries.iter().find(|e| e.name == "sample.zip").unwrap();
        assert!(zip.is_archive, "detected by magic bytes");
        assert!(!zip.is_dir, "still a file on disk");
        assert!(!b.entries.iter().find(|e| e.name == "plain.txt").unwrap().is_archive);
        // Archives sort with the directories, since that's how they
        // behave on Enter.
        assert_eq!(names(&b), vec!["..", "sample.zip", "plain.txt"]);
    }

    // Opening the archive directly, the way `e sample.zip` does: the
    // browser lands *inside* it rather than failing on a non-directory.
    #[test]
    fn opening_an_archive_lists_its_root() {
        let tmp = Tmp::new("archive-root");
        let zip = zip_in(&tmp);
        let b = Browser::open(&zip).unwrap();
        assert_eq!(names(&b), vec!["..", "dir", "notes.txt"]);
        assert!(b.cwd.to_string_lossy().ends_with("sample.zip!"));
    }

    #[test]
    fn enter_descends_through_an_archive_and_accepts_a_member_as_a_virtual_path() {
        let tmp = Tmp::new("archive-descend");
        zip_in(&tmp);
        let r = rect(20, 80);
        let mut b = Browser::open(&tmp.0).unwrap();

        b.focus_name("sample.zip");
        assert_eq!(b.handle_key(Key::Enter, r), Outcome::Continue, "Enter on an archive descends, it doesn't choose");
        assert_eq!(names(&b), vec!["..", "dir", "notes.txt"]);

        // `dir` exists only because members are named under it -- the
        // archive has no entry of its own for it.
        b.focus_name("dir");
        b.handle_key(Key::Enter, r);
        assert_eq!(names(&b), vec!["..", "deep", "inner.json"]);

        b.focus_name("inner.json");
        let chosen = b.handle_key(Key::Enter, r);
        match chosen {
            Outcome::Accepted(paths) => {
                assert_eq!(paths.len(), 1);
                let picked = paths[0].to_string_lossy().into_owned();
                assert!(picked.ends_with("sample.zip!/dir/inner.json"), "{picked}");
                // And it round-trips back to the archive and the member.
                let (archive, inner) = crate::archive::split(&picked).unwrap();
                assert_eq!(crate::archive::read_member(&archive, &inner).unwrap(), b"{\"a\": 1}\n");
            }
            other => panic!("expected the member to be accepted, got {other:?}"),
        }
    }

    // Backspace walks back out the way Enter came in, ending on real
    // disk again -- and lands on the archive it just left, the same as
    // leaving any directory does.
    #[test]
    fn backspace_walks_back_out_of_an_archive_onto_disk() {
        let tmp = Tmp::new("archive-out");
        zip_in(&tmp);
        let r = rect(20, 80);
        let mut b = Browser::open(&tmp.0.join("sample.zip")).unwrap();
        b.focus_name("dir");
        b.handle_key(Key::Enter, r);
        assert!(b.cwd.to_string_lossy().ends_with("sample.zip!/dir"));

        b.handle_key(Key::Backspace, r);
        assert!(b.cwd.to_string_lossy().ends_with("sample.zip!"), "{}", b.cwd.display());

        b.handle_key(Key::Backspace, r);
        assert_eq!(b.cwd, fs::canonicalize(&tmp.0).unwrap());
        assert_eq!(b.current().map(|e| e.name.as_str()), Some("sample.zip"), "lands on what it came out of");
    }

    // Ctrl-Y hands the shell the directory being *browsed* -- not
    // whatever the cursor is on, since Enter already descends.
    #[test]
    fn ctrl_y_offers_the_directory_being_browsed() {
        let tmp = Tmp::new("cd-here");
        tmp.file("a.txt", "x\n");
        let sub = tmp.dir("sub");
        let r = rect(20, 80);

        let mut b = Browser::open(&tmp.0).unwrap();
        b.set_can_change_directory(true);
        // With the cursor on a subdirectory, it is still *this*
        // directory that gets handed over.
        b.focus_name("sub");
        match b.handle_key(Key::CtrlY, r) {
            Outcome::ChangeDirectory(dir) => assert_eq!(dir, fs::canonicalize(&tmp.0).unwrap()),
            other => panic!("expected a directory change, got {other:?}"),
        }

        // ...and after descending, it is the one descended into.
        b.handle_key(Key::Enter, r);
        match b.handle_key(Key::CtrlY, r) {
            Outcome::ChangeDirectory(dir) => assert_eq!(dir, fs::canonicalize(&sub).unwrap()),
            other => panic!("expected a directory change, got {other:?}"),
        }
    }

    // Off unless the caller says it has a shell to move: `bish tool
    // edit` has none.
    #[test]
    fn ctrl_y_does_nothing_when_the_caller_has_no_shell() {
        let tmp = Tmp::new("cd-not-offered");
        tmp.file("a.txt", "x\n");
        let mut b = Browser::open(&tmp.0).unwrap();
        assert_eq!(b.handle_key(Key::CtrlY, rect(20, 80)), Outcome::Continue);
    }

    // An archive member is not a directory anything can sit in.
    #[test]
    fn ctrl_y_inside_an_archive_says_so_rather_than_offering_a_path() {
        let tmp = Tmp::new("cd-archive");
        let zip = zip_in(&tmp);
        let mut b = Browser::open(&zip).unwrap();
        b.set_can_change_directory(true);
        assert_eq!(b.handle_key(Key::CtrlY, rect(20, 80)), Outcome::Continue);
        assert!(b.error.as_deref().is_some_and(|e| e.contains("archive")), "{:?}", b.error);
    }

    #[test]
    fn the_status_hints_mention_cd_only_when_it_is_offered() {
        let tmp = Tmp::new("cd-hint");
        tmp.file("a.txt", "x\n");
        let mut b = Browser::open(&tmp.0).unwrap();
        assert!(!b.status_text(200).contains("cd here"));
        b.set_can_change_directory(true);
        assert!(b.status_text(200).contains("^y cd here"));
    }

    #[test]
    fn a_file_that_only_looks_like_an_archive_is_left_alone() {
        let tmp = Tmp::new("archive-fake");
        tmp.file("fake.zip", "not really a zip\n");
        let b = Browser::open(&tmp.0).unwrap();
        assert!(!b.entries.iter().find(|e| e.name == "fake.zip").unwrap().is_archive);
    }

    #[test]
    fn icons_pick_out_the_obvious_file_types() {
        let mk = |name: &str, is_dir: bool| Entry {
            is_archive: false,
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            is_symlink: false,
            is_exec: false,
            is_parent: false,
            size: 0,
        };
        assert_eq!(icon_for(&mk("src", true)), '\u{1F4C1}');
        assert_eq!(icon_for(&mk("main.rs", false)), '\u{1F980}');
        assert_eq!(icon_for(&mk("README.md", false)), '\u{1F4DD}');
        assert_eq!(icon_for(&mk("Cargo.lock", false)), '\u{1F512}');
        assert_eq!(icon_for(&mk("Makefile", false)), '\u{1F528}');
        assert_eq!(icon_for(&mk("mystery", false)), '\u{1F4C4}');
        let mut exe = mk("run", false);
        exe.is_exec = true;
        assert_eq!(icon_for(&exe), '\u{1F680}');
    }
}
