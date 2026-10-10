// `e DIR`: a file browser drawn into the focused pane, for picking files
// to open. This module owns the model (listing, cursor, selection,
// filter), the layout and the rendering, all testable without a terminal;
// repl.rs's `run_browse_frame` drives it, as with the diagnostics list.
//
// The grid is filled column-major, item 1 below item 0, so an alphabetical
// listing reads down a column. Overflow scrolls sideways by whole columns.
// So `hjkl` and the arrows move within the grid; Enter descends, and
// Backspace or Alt-Up goes to the parent.
//
// `Outcome::Accepted` carries paths rather than acting on them: the
// browser is a chooser, and `e DIR` opens each chosen path (see repl.rs's
// `expand_browse_targets`). Esc or `q` chooses nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::bishedit::fuzzy::fuzzy_match;
use crate::bishedit::grapheme;
use crate::bishedit::unicode_width::{char_width, str_width};
use crate::editor::Key;
use crate::window::Rect;

// One listing entry. `path` is absolute (joined to the canonicalized
// `cwd`), so the selection holds across directory changes and filtering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    // An archive, which Enter navigates into like a directory (see
    // `navigable`). Separate from `is_dir` so it's drawn differently and
    // still shows a size.
    pub(crate) is_archive: bool,
    pub(crate) is_symlink: bool,
    pub(crate) is_exec: bool,
    // The synthetic `..` row: always first, never selectable (it can't be
    // part of a multi-selection), and dropped while a filter is typed.
    pub(crate) is_parent: bool,
    // Excluded by a `.gitignore` covering this directory. Hidden by default,
    // and dimmed once `i` shows them.
    pub(crate) is_ignored: bool,
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
    // Esc (outside a search) or `q`: back to what the pane showed before,
    // nothing chosen.
    Cancelled,
    // Enter on a file, or on a non-empty selection. Absolute paths, in the
    // selection set's sorted order.
    Accepted(Vec<PathBuf>),
    // Ctrl-Y: leave and make this directory the shell's. Only offered when
    // the caller has a shell to move (`set_can_change_directory`); `bish tool
    // edit` doesn't.
    ChangeDirectory(PathBuf),
}

// The visible grid's shape for a pane and the listing's widest label.
// `cols` is how many fit on screen, not how many the listing needs (see
// `total_cols`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) rows: usize,
    pub(crate) cols: usize,
    pub(crate) col_width: usize,
}

// Blank columns between one grid column's text and the next.
const GUTTER: usize = 2;
// Narrower columns truncate every name to a few characters, so a narrow
// pane gets fewer, wider columns instead, possibly one.
const MIN_COL_WIDTH: usize = 18;
// Wider than this, one long name would set every column's width and
// collapse a wide pane to one column, so outliers are truncated with an
// ellipsis instead.
const MAX_COL_WIDTH: usize = 32;
// Mark (1), icon (2) and a space (1), fixed so names in a column line up.
const PREFIX_WIDTH: usize = 4;

pub(crate) struct Browser {
    cwd: PathBuf,
    // Everything in `cwd`, plus the `..` row, sorted: directories first, then
    // files, each case-insensitively.
    entries: Vec<Entry>,
    // Indices into `entries` in display order: ranked by the filter when
    // `query` is set, otherwise `0..entries.len()`.
    view: Vec<usize>,
    // Per `view` entry, the matched character positions from
    // `fuzzy::fuzzy_match`; empty without a query.
    matches: Vec<Vec<usize>>,
    // Index into `view`, not `entries`.
    cursor: usize,
    // Leftmost grid column currently on screen.
    scroll_col: usize,
    // By absolute path, so it survives filtering and directory changes:
    // selecting files across directories, then Enter, is intended. A
    // `BTreeSet` so `Accepted` comes out in a stable order.
    selected: BTreeSet<PathBuf>,
    query: String,
    // `/` moves the keyboard to the filter; Esc leaves it, clearing the
    // query, rather than leaving the browser.
    searching: bool,
    show_hidden: bool,
    // The gitignore rules covering `cwd`, reloaded when it changes; empty
    // outside a repository.
    ignore: crate::gitignore::Stack,
    // Whether ignored entries are listed. Off by default, so `target/` and
    // `node_modules/` stay out of the way.
    show_ignored: bool,
    // The `gitignore` bishopt. Off, nothing counts as ignored, and `i` does
    // nothing.
    honor_gitignore: bool,
    // This session's `ui_col_*` colours (see theme.rs); `None` draws the
    // defaults.
    colors: Option<crate::theme::UiColors>,
    // Back and forward stacks for Alt-Left/Alt-Right, as at the prompt
    // (`editor::DirNav`).
    back: Vec<PathBuf>,
    forward: Vec<PathBuf>,
    // The last failed directory read, shown in the status row until the next
    // successful action.
    error: Option<String>,
    // What the last colon-line command printed; the same row, without a
    // warning sign (see `set_message`).
    message: Option<String>,
    // Whether Ctrl-Y is offered; see `Outcome::ChangeDirectory`.
    can_change_directory: bool,
}

impl Browser {
    // Opens `start`. An unreadable directory is an error; no ancestor is
    // tried instead.
    pub(crate) fn open(start: &Path) -> Result<Self, String> {
        if !cfg!(feature = "browser") {
            return Err("the file browser is not part of this build".to_string());
        }
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
            ignore: crate::gitignore::Stack::new(),
            show_ignored: false,
            honor_gitignore: true,
            colors: None,
            back: Vec::new(),
            forward: Vec::new(),
            error: None,
            message: None,
            can_change_directory: false,
        };
        b.reload()?;
        Ok(b)
    }

    // Enables Ctrl-Y; off by default, so a caller without a shell can't offer
    // a key that does nothing.
    pub(crate) fn set_can_change_directory(&mut self, allow: bool) {
        self.can_change_directory = allow;
    }

    // A message for the status row until the next action: what a colon-line
    // command printed. Not `error`, since `bishopt wrap` answering `off`
    // isn't a warning.
    pub(crate) fn set_message(&mut self, message: String) {
        self.message = Some(message);
    }

    pub(crate) fn set_colors(&mut self, colors: crate::theme::UiColors) {
        self.colors = Some(colors);
    }

    // The `gitignore` bishopt from the opening shell. Set before the first
    // listing is used; it reloads.
    pub(crate) fn set_honor_gitignore(&mut self, honor: bool) -> Result<(), String> {
        if self.honor_gitignore == honor {
            return Ok(());
        }
        self.honor_gitignore = honor;
        self.reload()
    }

    // Re-reads `cwd` and rebuilds the view. The cursor keeps its position if
    // it still exists; callers wanting a specific name use `focus_name`.
    fn reload(&mut self) -> Result<(), String> {
        // Before `read_here`, which consults it. Not for a path inside an
        // archive, where nothing is on disk.
        let inside_archive = crate::archive::split(&self.cwd.to_string_lossy()).is_some();
        self.ignore = match self.honor_gitignore && !inside_archive {
            true => crate::gitignore::Stack::for_directory(&self.cwd),
            false => crate::gitignore::Stack::new(),
        };
        let mut entries = self.read_here()?;
        // `Path::parent` already walks out of a virtual path:
        // `/a/b.zip!/dir` to `/a/b.zip!` to `/a`, since `!` sits inside a
        // component.
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
                    is_ignored: false,
                    size: 0,
                },
            );
        }
        // Directories first (`..` is one, and sorts first by name), then
        // case-insensitively with a case-sensitive tiebreak, so the order
        // is total.
        crate::sort::sort_by(&mut entries, |a, b| {
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

    // Everything in `cwd`: from `read_dir`, or for a virtual path
    // (`archive::split`) from the archive's member list. Everything
    // downstream works on `Entry` and never learns which.
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
                    // Archives don't nest (see `archive::split`), so one
                    // inside is a file.
                    is_archive: false,
                    is_symlink: false,
                    is_exec: false,
                    is_parent: false,
                    is_ignored: false,
                    size: m.size,
                })
                .collect());
        }
        let read = std::fs::read_dir(&self.cwd).map_err(|e| format!("{}: {e}", self.cwd.display()))?;
        // If `cwd` itself is ignored, nothing in it is filtered: you walked
        // in on purpose.
        let filtering = !self.ignore.matched(&self.cwd, true).is_ignored();
        let mut entries = Vec::new();
        for dirent in read.flatten() {
            let name = dirent.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let mut entry = describe(dirent.path(), name);
            entry.is_ignored = filtering && self.ignore.matched(&entry.path, entry.is_dir).is_ignored();
            if entry.is_ignored && !self.show_ignored {
                continue;
            }
            entries.push(entry);
        }
        Ok(entries)
    }

    // Rebuilds `view` and `matches` from `entries` and `query`, then clamps
    // the cursor. An empty query keeps the listing order; a query ranks by
    // score, which breaks the directories-first grouping, since when
    // searching rank is what matters.
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
            crate::sort::sort_by(&mut scored, |a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            for (_, i, positions) in scored {
                self.view.push(i);
                self.matches.push(positions);
            }
        }
        if self.cursor >= self.view.len() {
            self.cursor = self.view.len().saturating_sub(1);
        }
    }

    // Puts the cursor on the entry with this name if it's in the view, so
    // going up lands on the directory just left.
    fn focus_name(&mut self, name: &str) {
        if let Some(pos) = self.view.iter().position(|&i| self.entries[i].name == name) {
            self.cursor = pos;
        }
    }

    fn current(&self) -> Option<&Entry> {
        self.view.get(self.cursor).map(|&i| &self.entries[i])
    }

    // The grid for `rect`: a header row, then the grid. Column width is the
    // widest label present, clamped so one long name can't collapse a wide
    // pane to one column and a narrow pane still gets usable columns.
    pub(crate) fn layout(&self, rect: Rect) -> Layout {
        let rows = rect.rows.saturating_sub(1).max(1);
        let widest = self
            .view
            .iter()
            .map(|&i| PREFIX_WIDTH + str_width(&self.entries[i].name) + if self.entries[i].is_dir { 1 } else { 0 })
            .max()
            .unwrap_or(0);
        let cols_avail = rect.cols.max(1);
        let col_width = (widest + GUTTER).clamp(MIN_COL_WIDTH.min(cols_avail), MAX_COL_WIDTH.min(cols_avail));
        let cols = (cols_avail / col_width).max(1);
        Layout { rows, cols, col_width }
    }

    // How many columns the filtered listing needs, usually more than
    // `Layout::cols`.
    fn total_cols(&self, rows: usize) -> usize {
        self.view.len().div_ceil(rows.max(1))
    }

    // Scrolls just enough to show the cursor's column. Also called from
    // `render`, so a resize between keys can't strand the cursor off-screen.
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
            // A failed read leaves the browser where it was.
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
        self.message = None;
        self.query.clear();
        self.searching = false;
        self.cursor = 0;
        self.scroll_col = 0;
        self.refilter();
        // Coming up from a child, land on it. Leaving an archive root
        // arrives with `some.zip!`, whose trailing separator isn't part of
        // the listed name.
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

    // Enter: on a directory it descends, even with a selection, or a
    // selection could never span directories. On a file, an existing
    // selection is returned, else just the file under the cursor.
    fn accept(&mut self) -> Outcome {
        let selection = || -> Outcome { Outcome::Accepted(self.selected.iter().cloned().collect()) };
        let Some(entry) = self.current() else {
            // Nothing under the cursor (an empty directory or filter); a
            // selection made elsewhere is still returned.
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
        self.message = None;

        // While filtering, printable keys go to the query, and the rest
        // fall through. Arrows, Ctrl-N and Ctrl-P still move the cursor, so
        // filtering and picking are one gesture.
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
                // Esc leaves the filter, clearing it, not the browser: one
                // Esc per level.
                Key::Escape => {
                    self.searching = false;
                    self.query.clear();
                    self.cursor = 0;
                    self.scroll_col = 0;
                    self.refilter();
                    return Outcome::Continue;
                }
                // Commits the filter and returns the keyboard to the grid,
                // keeping the query, so `/rs<Enter>` leaves a narrowed
                // listing rather than opening the top hit.
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
            // Ctrl-Y: take the shell to the directory being browsed, not
            // the entry under the cursor; Enter already descends.
            Key::CtrlY if self.can_change_directory => {
                if crate::archive::split(&self.cwd.to_string_lossy()).is_some() {
                    self.error = Some("inside an archive -- nothing to cd into".to_string());
                    return Outcome::Continue;
                }
                Outcome::ChangeDirectory(self.cwd.clone())
            }
            Key::Char('/') => {
                self.searching = true;
                Outcome::Continue
            }
            // Toggle and advance, so holding Tab walks a run of files into
            // the selection.
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
            // Clears the selection without leaving; Esc is already taken.
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
            Key::Char('.') | Key::Char('i') => {
                match key {
                    Key::Char('i') => self.show_ignored = !self.show_ignored,
                    _ => self.show_hidden = !self.show_hidden,
                }
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
                // Both wheels move the same way: the grid fills
                // column-major, so even the vertical wheel scrolls it
                // sideways.
                if ev.is_scroll_down() || ev.is_scroll_right() {
                    self.scroll_col = (self.scroll_col + 1).min(self.total_cols(layout.rows).saturating_sub(1));
                } else if ev.is_scroll_up() || ev.is_scroll_left() {
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

    // The view index under a 1-indexed terminal cell, if any. The header row
    // and cells past the listing give `None`, so clicking empty space does
    // nothing.
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

    // The whole frame: the pane's header and grid, plus the terminal's
    // global status row (`repl::render_global_status_row`, shared with the
    // file editor). The cursor is left in the filter input while searching,
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
            // The `/` and the query are in the status row; the cursor goes
            // after the query.
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

        // A directory's `/` and an archive's `!` both mean Enter goes into
        // it; `!` is what the path gains (`archive::SEPARATOR`). Names are
        // drawn as raw SGR text, bypassing `render_linked`'s gate, so
        // control characters are made safe here, one character for one so
        // `self.matches` still lines up.
        let label = crate::term::safe_text(&match entry {
            e if e.is_parent => e.name.clone(),
            e if e.is_dir => format!("{}/", e.name),
            e if e.is_archive => format!("{}{}", e.name, crate::archive::SEPARATOR),
            e => e.name.clone(),
        });
        let name_width = width.saturating_sub(PREFIX_WIDTH + GUTTER.min(width.saturating_sub(PREFIX_WIDTH)));
        let empty = Vec::new();
        let positions = self.matches.get(idx).unwrap_or(&empty);
        let (pieces, used) = fit_marked(&label, positions, name_width);

        let mut out = String::new();
        out.push_str(&color_for(entry, self.colors.as_ref()));
        // Over the type colour, not instead of it: an ignored directory is
        // still a directory.
        if entry.is_ignored {
            out.push_str("\x1b[2m");
        }
        // After the colour: a themed colour is an SGR run starting with a
        // reset, which would switch reverse video off again and hide the
        // cursor on every coloured entry.
        if focused {
            out.push_str("\x1b[7m");
        }
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

    // `📁 ~/bish/src` left, counts right. The path is truncated from the
    // left, keeping the leaf, which says where you are.
    fn header_text(&self, cols: usize) -> String {
        // While filtering, "5 items" in a directory of 22 would mislead, so
        // it says what it's five of.
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
        if used + GUTTER > cols { fit(&left, cols) } else { format!("{left}{}{right}", " ".repeat(cols - used)) }
    }

    fn status_text(&self, cols: usize) -> String {
        if self.searching {
            return fit(&format!("\u{1F50D} {}", self.query), cols);
        }
        if let Some(err) = &self.error {
            return fit(&format!("\u{26A0} {err}"), cols);
        }
        if let Some(message) = &self.message {
            return fit(message, cols);
        }
        let detail = match self.current() {
            Some(e) if e.is_parent => "parent directory".to_string(),
            Some(e) if e.is_dir => crate::term::safe_text(&format!("{}/", e.name)),
            Some(e) => crate::term::safe_text(&format!("{}  {}", e.name, human_size(e.size))),
            None => {
                if self.query.is_empty() {
                    "empty directory".to_string()
                } else {
                    format!("no matches for '{}'", self.query)
                }
            }
        };
        // No `i` hint when `gitignore` is off: nothing to reveal.
        let hints = match (self.can_change_directory, self.honor_gitignore) {
            (true, true) => "enter open  ^y cd here  tab select  / filter  . hidden  i ignored  : cmd  bksp up  esc back",
            (true, false) => "enter open  ^y cd here  tab select  / filter  . hidden  : cmd  bksp up  esc back",
            (false, true) => "enter open  tab select  / filter  . hidden  i ignored  : cmd  bksp up  esc back",
            (false, false) => "enter open  tab select  / filter  . hidden  : cmd  bksp up  esc back",
        };
        let used = str_width(&detail) + str_width(hints);
        if used + GUTTER > cols { fit(&detail, cols) } else { format!("{detail}{}{hints}", " ".repeat(cols - used)) }
    }
}


// Where a browse starts: the argument, if any (relative to the session's
// cwd, not the process's: each pane has its own), otherwise that cwd.
// `~` and `~/...` expand here too, for callers with a raw path.
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
    if expanded.is_absolute() { expanded } else { cwd.join(expanded) }
}

// The absolute, symlink-free spelling of where the browser will sit,
// including inside archives, which `fs::canonicalize` can't resolve. An
// archive file lands at its root: `e some.zip` and Enter on it both mean
// "show what's inside".
fn canonical(path: &Path) -> Result<PathBuf, String> {
    let raw = path.to_string_lossy().into_owned();
    if let Some((archive, inner)) = crate::archive::split(&raw) {
        let archive = std::fs::canonicalize(&archive).map_err(|e| format!("{}: {e}", archive.display()))?;
        return Ok(PathBuf::from(crate::archive::join(&archive, &inner)));
    }
    let real = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if crate::archive::holds_members(&real) {
        return Ok(PathBuf::from(crate::archive::join(&real, "")));
    }
    Ok(real)
}

fn describe(path: PathBuf, name: String) -> Entry {
    // `symlink_metadata` says whether this name is a link, and `metadata`
    // what it points at: a link to a directory navigates like one, with the
    // link icon.
    let link = std::fs::symlink_metadata(&path).ok();
    let meta = std::fs::metadata(&path).ok();
    let is_symlink = link.as_ref().map(|m| m.file_type().is_symlink()).unwrap_or(false);
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let is_exec = meta.as_ref().map(crate::platform::has_execute_bit).unwrap_or(false) && !is_dir;
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    // By magic bytes, not name (`archive::kind_of`), so a `.jar` or `.whl`
    // opens and a fake `.zip` doesn't. One more open and 4-byte read per
    // file.
    let is_archive = !is_dir && crate::archive::looks_like_archive(&path);
    Entry { name, path, is_dir, is_archive, is_symlink, is_exec, is_parent: false, is_ignored: false, size }
}

// Joins an archive directory and a child name: archive member names are
// `/`-separated by definition, not host paths.
fn join_inner(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn color_for(e: &Entry, colors: Option<&crate::theme::UiColors>) -> String {
    use crate::theme::Ui;
    let element = if e.is_symlink {
        Ui::Symlink
    } else if e.is_archive {
        // Its own colour: it navigates like a directory but is one read-only
        // file on disk.
        Ui::Archive
    } else if e.is_dir {
        Ui::Directory
    } else if e.is_exec {
        Ui::Executable
    } else {
        // Ordinary files use the terminal's foreground, so the common case
        // doesn't stand out.
        return "\x1b[39m".to_string();
    };
    crate::theme::sgr(element, colors)
}

// Every icon is from U+1F300..=U+1FAFF, the block
// `unicode_width::char_width` reports as width 2, which the column
// arithmetic assumes. One from Miscellaneous Symbols (U+2600..) would
// measure 1 here but draw as 2, shifting every column after it.
// `every_icon_is_two_columns_wide` enforces this.
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

// Files without an extension that are recognizable by name.
const BY_NAME: &[(&str, char)] = &[("makefile", '\u{1F528}'), ("dockerfile", '\u{1F433}'), ("license", '\u{1F4DC}'), ("readme", '\u{1F4DD}')];

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
    // Before the name tables, since `is_archive` comes from magic bytes: a
    // zip called `plugin.vsix` looks like an archive.
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

// Display text, so control characters are made safe here: a directory
// can be named anything.
fn shorten_home(path: &Path) -> String {
    crate::term::safe_text(&shorten_home_raw(path))
}

fn shorten_home_raw(path: &Path) -> String {
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
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1}{}", UNITS[unit]) }
}

// Pads to exactly `width` display columns, truncating first if wider.
// Columns, not chars: an icon or a CJK name takes two.
fn pad_to(s: &str, width: usize) -> String {
    let fitted = fit(s, width);
    let used = str_width(&fitted);
    format!("{fitted}{}", " ".repeat(width.saturating_sub(used)))
}

// Truncates to `width` columns with an ellipsis, by grapheme cluster, so a
// ZWJ sequence or combining mark is never split.
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

// Like `fit`, keeping the tail: for a path, the leaf matters most.
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

// Truncates like `fit`, returning (cluster, matched) pieces so the
// renderer can underline what the query matched. `positions` are char
// indices (`fuzzy::FuzzyMatch::positions`); a cluster is matched if its
// first char is. Also returns the width used.
pub(crate) fn fit_marked(s: &str, positions: &[usize], width: usize) -> (Vec<(String, bool)>, usize) {
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
    // Names are spliced into the escape stream as raw SGR text, so a file
    // named `evil<ESC>[2J.txt` must not clear the screen.
    #[test]
    fn a_hostile_filename_cannot_paint_with_the_terminals_own_escapes() {
        let dir = std::env::temp_dir().join(format!("bish-browser-escapes-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("evil\x1b[2J.txt"), "").unwrap();
        std::fs::write(dir.join("plain.txt"), "").unwrap();

        let mut b = super::Browser::open(&dir).unwrap();
        let drawn = b.render(super::Rect { row: 0, col: 0, rows: 20, cols: 60 }, 24, 80);
        assert!(drawn.contains("plain.txt"), "the control renders normally");
        assert!(!drawn.contains("\x1b[2J"), "but the hostile one does not clear the screen");
        assert!(drawn.contains('\u{FFFD}'), "it is shown, just not obeyed");
        std::fs::remove_dir_all(&dir).ok();
    }

    use super::*;
    use std::fs;

    fn rect(rows: usize, cols: usize) -> Rect {
        Rect { row: 0, col: 0, rows, cols }
    }

    // A throwaway tree under the crate's target dir.
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

    // Every icon measures two columns by this codebase's width table, which
    // the layout assumes; one that measures 1 but draws as 2 would shift
    // every column after it.
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

    #[test]
    fn gitignored_entries_are_hidden_until_i_asks_for_them() {
        let t = Tmp::new("ignored");
        t.dir(".git");
        t.file(".gitignore", "target/\n*.log\n!keep.log\n");
        t.dir("target");
        t.file("main.rs", "");
        t.file("debug.log", "");
        t.file("keep.log", "");

        let mut b = Browser::open(&t.0).unwrap();
        let mut shown = names(&b);
        shown.retain(|n| n != "..");
        shown.sort();
        assert_eq!(shown, vec!["keep.log", "main.rs"], "target/ and debug.log are ignored; the `!` rescues keep.log");

        b.handle_key(Key::Char('i'), rect(10, 80));
        let mut all = names(&b);
        all.retain(|n| n != "..");
        all.sort();
        assert_eq!(all, vec!["debug.log", "keep.log", "main.rs", "target"]);
        // ...and marked as ignored.
        let ignored: Vec<&str> = b.entries.iter().filter(|e| e.is_ignored).map(|e| e.name.as_str()).collect();
        assert_eq!(ignored, vec!["target", "debug.log"]);
    }

    // With `gitignore` off, nothing is hidden or marked.
    #[test]
    fn the_gitignore_bishopt_turns_the_whole_thing_off() {
        let t = Tmp::new("optout");
        t.dir(".git");
        t.file(".gitignore", "*.log\n");
        t.file("debug.log", "");
        let mut b = Browser::open(&t.0).unwrap();
        assert!(!names(&b).contains(&"debug.log".to_string()));
        b.set_honor_gitignore(false).unwrap();
        assert!(names(&b).contains(&"debug.log".to_string()));
        assert!(b.entries.iter().all(|e| !e.is_ignored), "nothing is marked ignored either");
        // ...and `i` has nothing to reveal.
        b.handle_key(Key::Char('i'), rect(10, 80));
        assert!(names(&b).contains(&"debug.log".to_string()));
    }

    // Outside a repository there's no root for a `.gitignore`, so nothing is
    // ignored, even by a stray one.
    #[test]
    fn nothing_is_ignored_outside_a_repository() {
        let t = Tmp::new("norepo");
        t.file(".gitignore", "*.log\n");
        t.file("debug.log", "");
        let b = Browser::open(&t.0).unwrap();
        assert!(names(&b).contains(&"debug.log".to_string()));
    }

    // Walking into an ignored directory on purpose shows its contents.
    #[test]
    fn an_ignored_directory_still_lists_its_contents_once_you_are_in_it() {
        let t = Tmp::new("intarget");
        t.dir(".git");
        t.file(".gitignore", "target/\n");
        t.dir("target");
        fs::write(t.0.join("target/build.o"), "").unwrap();
        let b = Browser::open(&t.0.join("target")).unwrap();
        assert!(names(&b).contains(&"build.o".to_string()));
    }

    // A deeper `.gitignore` is loaded too, and wins.
    #[test]
    fn a_nested_gitignore_applies_and_overrides() {
        let t = Tmp::new("nested");
        t.dir(".git");
        t.file(".gitignore", "*.log\n");
        t.dir("sub");
        fs::write(t.0.join("sub/.gitignore"), "!*.log\n").unwrap();
        fs::write(t.0.join("sub/a.log"), "").unwrap();
        let b = Browser::open(&t.0.join("sub")).unwrap();
        assert!(names(&b).contains(&"a.log".to_string()), "the nested `!` line re-includes it");
    }

    // The vertical and horizontal wheels agree: both scroll sideways.
    #[test]
    fn either_wheel_scrolls_the_grid_sideways() {
        let t = Tmp::new("hwheel");
        for i in 0..40 {
            t.file(&format!("f{i:02}.txt"), "");
        }
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(6, 40);
        let wheel = |button| Key::Mouse(crate::editor::MouseEvent { button, col: 1, row: 1, pressed: true });
        assert_eq!(b.scroll_col, 0);
        b.handle_key(wheel(67), r); // right
        assert_eq!(b.scroll_col, 1);
        b.handle_key(wheel(65), r); // down -- the same direction here
        assert_eq!(b.scroll_col, 2);
        b.handle_key(wheel(66), r); // left
        assert_eq!(b.scroll_col, 1);
        b.handle_key(wheel(64), r); // up
        assert_eq!(b.scroll_col, 0);
        b.handle_key(wheel(66), r);
        assert_eq!(b.scroll_col, 0, "and it stops rather than going negative");
    }

    // Column-major: with 4 rows, `l` from the first item goes to the fifth.
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

    // One very long name among ordinary ones mustn't set every column's width
    // (see `MAX_COL_WIDTH`).
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
        // Narrow enough that only a couple of columns fit.
        let r = rect(5, 40);
        let layout = b.layout(r);
        assert_eq!(b.scroll_col, 0);
        b.handle_key(Key::Char('G'), r);
        assert!(b.scroll_col > 0, "cursor at the end should have scrolled");
        let cursor_col = b.cursor / layout.rows;
        assert!(
            cursor_col >= b.scroll_col && cursor_col < b.scroll_col + layout.cols,
            "cursor column {cursor_col} outside visible {}..{}",
            b.scroll_col,
            b.scroll_col + layout.cols
        );
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

    // Esc leaves the filter first, then the browser.
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

    // Whether reverse video is still on when the entry's text starts, which
    // is what marks the cursor's row.
    fn reverse_survives(prefix: &str) -> bool {
        let mut on = false;
        for esc in prefix.split('\u{1b}').skip(1) {
            let Some(body) = esc.strip_prefix('[').and_then(|b| b.split('m').next()) else { continue };
            for code in body.split(';') {
                match code {
                    "0" | "" => on = false,
                    "7" => on = true,
                    "27" => on = false,
                    _ => {}
                }
            }
        }
        on
    }

    /// The cursor shows on every kind of entry, including those with colours
    /// of their own, whose SGR runs start with a reset.
    #[test]
    fn the_cursor_is_visible_on_an_entry_that_has_its_own_colour() {
        let t = Tmp::new("cursorcolour");
        t.dir("adir");
        t.file("plain.txt", "");
        t.file("run.sh", "");
        let exec = t.0.join("run.sh");
        std::os::unix::fs::symlink(&exec, t.0.join("link")).unwrap();
        fs::set_permissions(&exec, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut b = Browser::open(&t.0).unwrap();
        let r = rect(10, 80);
        for name in ["adir", "plain.txt", "run.sh", "link"] {
            b.focus_name(name);
            let painted = b.render(r, 10, 80);
            let at = painted.find(name).expect("the entry is on screen");
            let row_start = painted[..at].rfind('H').map(|i| i + 1).unwrap_or(0);
            assert!(reverse_survives(&painted[row_start..at]), "the cursor is invisible on {name}: {:?}", &painted[row_start..at]);
        }
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
        // ...but it advanced, so the next Tab selects a real file.
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
        // Back on the directory just left, not at the top.
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

    // No row is wider than its rect: an over-wide row wraps and corrupts
    // every pane below.
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

    // The plain text painted at each cursor-position escape, without SGR
    // codes: enough to measure row widths.
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
                    // A move to the global status row ends the pane's rows.
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
        // Either the whole cluster or none of it: never a lone ZWJ.
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

    // The zip fixture archive.rs's tests use: notes.txt, dir/inner.json and
    // dir/deep/leaf.txt, with no entries for the directories.
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
        // Archives sort with directories, since they behave like them on
        // Enter.
        assert_eq!(names(&b), vec!["..", "sample.zip", "plain.txt"]);
    }

    // Opened directly, as `e sample.zip` does: the browser lands inside.
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

        // `dir` exists only because members are named under it.
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
                // And it round-trips to the archive and member.
                let (archive, inner) = crate::archive::split(&picked).unwrap();
                assert_eq!(crate::archive::read_member(&archive, &inner).unwrap(), b"{\"a\": 1}\n");
            }
            other => panic!("expected the member to be accepted, got {other:?}"),
        }
    }

    // Backspace walks back out to real disk, landing on the archive just
    // left.
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

    // Ctrl-Y hands over the directory being browsed, not the cursor's entry.
    #[test]
    fn ctrl_y_offers_the_directory_being_browsed() {
        let tmp = Tmp::new("cd-here");
        tmp.file("a.txt", "x\n");
        let sub = tmp.dir("sub");
        let r = rect(20, 80);

        let mut b = Browser::open(&tmp.0).unwrap();
        b.set_can_change_directory(true);
        // With the cursor on a subdirectory, it's still this directory.
        b.focus_name("sub");
        match b.handle_key(Key::CtrlY, r) {
            Outcome::ChangeDirectory(dir) => assert_eq!(dir, fs::canonicalize(&tmp.0).unwrap()),
            other => panic!("expected a directory change, got {other:?}"),
        }

        // ...and after descending, the one descended into.
        b.handle_key(Key::Enter, r);
        match b.handle_key(Key::CtrlY, r) {
            Outcome::ChangeDirectory(dir) => assert_eq!(dir, fs::canonicalize(&sub).unwrap()),
            other => panic!("expected a directory change, got {other:?}"),
        }
    }

    // Only when the caller has a shell to move: `bish tool edit` has none.
    #[test]
    fn ctrl_y_does_nothing_when_the_caller_has_no_shell() {
        let tmp = Tmp::new("cd-not-offered");
        tmp.file("a.txt", "x\n");
        let mut b = Browser::open(&tmp.0).unwrap();
        assert_eq!(b.handle_key(Key::CtrlY, rect(20, 80)), Outcome::Continue);
    }

    // An archive member isn't a directory anything can sit in.
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
            is_ignored: false,
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
