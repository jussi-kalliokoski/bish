// The tiling window manager: the pane tree, the frame stack each pane
// holds, and the geometry that turns one into rectangles on a terminal.
//
// Tree algebra over `PaneId`, and nothing else. Nothing here knows what
// a session is, what a shell is, or how to draw -- which is the line
// the split follows. `repl.rs` keeps everything that touches a
// `SessionState` or puts bytes on a terminal: `compositor_redraw`, the
// `split_*_pane` functions (they allocate a session), the `render_*`
// family, `sync_*_pane`. This file keeps everything that is only about
// the shape of a window.
//
// It was already a separate concern; it had simply never been given a
// file. The three test modules at the bottom moved here unchanged, and
// they were already testing exactly this and nothing else.

use std::path::PathBuf;

// Vim keeps 100. Same number, same reason: a list you cannot walk out
// of is no more useful than a short one, and each entry is a path.
const MAX_JUMPS: usize = 100;

// Reserves the terminal's real last two rows for global chrome: the tab
// bar (render_compositor_frame, pinned to term_rows itself) and, one row
// above it, the global mode-line/command row (render_global_status_row,
// command_mode_row) -- everything panes/compute_regions ever divide up
// lives above both.
pub(crate) fn content_rows(term_rows: usize) -> usize {
    term_rows.saturating_sub(2).max(1)
}

pub(crate) type SessionId = u32;

pub(crate) type JobFrameId = u32;

pub(crate) type EditFrameId = u32;

pub(crate) type HexFrameId = u32;

// One layer of a pane's view stack. Session is the vim-like "same
// session shown in multiple windows" case (see WindowEntry's doc
// comment); Job is a fg'd background job, poll-driven against its own
// pty (see exec::FgJob and drive_fg_job below) instead of blocking the
// whole process the way exec.rs's old Shell-local drive_pending_fg did.
// A Job frame holds an id into `job_frames` (owned by repl::run, see
// below) rather than the FgJob itself, since -- unlike a session, which
// can legitimately be the SAME live thing shown in two places at once --
// a running job is inherently a one-place-at-a-time resource, and
// keeping Frame Copy (an FgJob owns a real OS process + pty, definitely
// not Copy) matters for how cheaply `window fg` can duplicate a Session
// frame onto another window's stack. Edit is `e`'s own equivalent of
// Job -- a builtin editor session (fileeditor::EditSession) that can
// likewise be detached (Ctrl+Space) and resumed later, holding an id
// into `edit_frames` for exactly the same Copy-ness reason (a
// TextBuffer's own content is definitely not Copy either). Diagnostics
// is different from all three: it's never the *only* frame a pane ever
// shows (see split_diagnostics_pane) -- it names the `:diag`-triggered
// sibling pane that sits below a specific Edit frame's own pane,
// browsing that same `EditFrameId`'s buffer.diagnostics. It carries no
// persistent state of its own (unlike Job/Edit, there's no third
// `diag_frames` map) -- everything it shows is re-read from
// `edit_frames[&id]` fresh each time it's focused, since nothing about
// a diagnostics list is worth preserving across a collapse.
//
// Hex is `e --hex`'s own frame: the same shape as Edit in every way
// that matters here (a detachable, resumable editor view holding an id
// into its own `hex_frames` map, for the same Copy-ness reason), just
// over a byte buffer rather than a text one -- see hexedit.rs's own
// module doc comment for why a hex view is a frame in this stack rather
// than a separate program.
//
// DebugRun is the same shape as Diagnostics (a `:dbg`-triggered sibling
// under a specific Edit frame's own pane, named by that same
// `EditFrameId`, see split_debug_run_pane) but -- like Job/Edit, unlike
// Diagnostics -- it *does* carry real persistent state worth keeping
// across a focus change (the debugged script's own Shell, output,
// pause/run state): a `debug_frames: HashMap<EditFrameId, debugger::
// DebugSession>` map, alongside `job_frames`/`edit_frames`.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Frame {
    Session(SessionId),
    Job(JobFrameId),
    Edit(EditFrameId),
    Hex(HexFrameId),
    Diagnostics(EditFrameId),
    /// A list of places -- `gr`'s references today, and whatever else
    /// answers with `lsp::Location`s later. A sibling pane of the
    /// editor frame it was asked from, exactly like `Diagnostics`, and
    /// for the same reason: the list is *about* that buffer, and
    /// collapsing to a title row when unfocused is what keeps it from
    /// costing anything while you go on editing.
    ///
    /// Its contents live in `repl::run`'s own `location_lists`, keyed
    /// the same way `debug_frames` is -- unlike diagnostics, a list of
    /// places in *other* files is not a property of any one buffer, so
    /// it has nowhere on `TextBuffer` it could honestly live.
    Locations(EditFrameId),
    DebugRun(EditFrameId),
}

pub(crate) type PaneId = u32;

// One pane of a (possibly split) window: exactly what a window's view
// stack used to be before panes existed (see Frame's doc comment) --
// the last (top) entry is what's currently rendered/driven in this
// pane. `window close`/EOF pops the top frame; once a pane's stack
// empties the pane itself closes (see apply_window_action's Close arm),
// collapsing the split, or -- if it was the window's only pane -- falls
// through to the existing "close the whole window" logic unchanged.
/// One position in the jump list: which file, and where in it.
///
/// The path is what makes this cross-file, and is the whole reason the
/// list cannot live where `VimKeys` keeps its own -- a `VimKeys`
/// belongs to one `EditSession`, so a file opened by `gd` gets a fresh
/// one and the history stops at the door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JumpEntry {
    pub(crate) path: PathBuf,
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// `Ctrl-O`/`Ctrl-I`'s history, vim's semantics: a jump pushes where
/// you were and discards the forward history, `Ctrl-O` steps back
/// pushing where you are so `Ctrl-I` can return, and `Ctrl-I` mirrors
/// it.
///
/// Lives on the `Pane` because vim's is per *window*, which is what a
/// pane is here -- and because it has to outlive any one buffer, which
/// is exactly what the first attempt at cross-file `gd` got wrong.
#[derive(Debug, Default)]
pub(crate) struct JumpList {
    pub(crate) back: Vec<JumpEntry>,
    pub(crate) forward: Vec<JumpEntry>,
}

impl JumpList {
    /// Records where a jump is leaving from. Discards the forward
    /// history, exactly as taking a new branch does in a browser -- and
    /// as vim does.
    pub(crate) fn push(&mut self, entry: JumpEntry) {
        // Vim collapses a repeat of the same line rather than stacking
        // it, which is what keeps `Ctrl-O` from needing several presses
        // to leave one spot.
        if self.back.last().is_some_and(|last| last.path == entry.path && last.row == entry.row) {
            self.back.pop();
        }
        self.back.push(entry);
        if self.back.len() > MAX_JUMPS {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    pub(crate) fn back(&mut self, current: JumpEntry) -> Option<JumpEntry> {
        let target = self.back.pop()?;
        self.forward.push(current);
        Some(target)
    }

    pub(crate) fn forward(&mut self, current: JumpEntry) -> Option<JumpEntry> {
        let target = self.forward.pop()?;
        self.back.push(current);
        Some(target)
    }
}

pub(crate) struct Pane {
    pub(crate) id: PaneId,
    pub(crate) stack: Vec<Frame>,
    /// See `JumpList` -- per pane, the way vim's is per window.
    pub(crate) jumps: JumpList,
}

impl Pane {
    pub(crate) fn owning_session(&self) -> SessionId {
        for frame in self.stack.iter().rev() {
            if let Frame::Session(id) = frame {
                return *id;
            }
        }
        panic!("a pane's stack always has an underlying session frame")
    }
}

// One child of a Split: its own subtree, plus how large a share of the
// split's space it gets relative to its siblings there. Weights are
// relative, not normalized to any fixed total -- compute_regions always
// divides a Split's available space in proportion to weight/(sum of all
// siblings' weights), so a fresh 2-way split with both sides at the
// default 1.0 divides evenly (1/2 each), and `window +`/`-`/`size` (see
// repl.rs's resize_focused_pane/set_focused_pane_size) only ever need
// to change *this* pane's own weight, never touch its siblings' -- the
// division naturally renormalizes around whatever's there.
//
// `fixed`, when `Some`, overrides all of that for this one child: it
// always gets exactly that many rows (a horizontal split) or columns (a
// vertical one), taken off the top before whatever's left divides among
// the `fixed: None` siblings by weight exactly as before (see
// compute_regions/split_sizes). `weight` is simply ignored while `fixed`
// is set, rather than the two interacting -- there's no case yet where
// a pane needs both "resizable" and "pinned" at once. The diagnostics
// pane (see Frame::Diagnostics) uses this while *expanded*, to size
// itself to its own list content -- capped by its own caller at half of
// whatever space was available before it expanded (see
// diagnostics_pane_rows), so this alone can't be used to demand more
// than half the split.
//
// `minimized` is a separate, more drastic state, for the same pane
// while *collapsed*: it folds down to exactly one row/column, and --
// unlike an ordinary `fixed: Some(1)` -- compute_regions skips
// reserving a *separate* divider line on either side of it, since a
// minimized pane's own single row already reads as that divider (see
// its own doc comment for the exact rule and render_diagnostics_title
// for what actually gets drawn into it: a title "pill" set into an
// otherwise ordinary-looking divider line, not a full-width bar).
// `fixed`/`weight` are simply irrelevant while `minimized` is set. Every
// `SplitChild` anywhere else stays `fixed: None, minimized: false` and
// behaves identically to before these fields existed.
pub(crate) struct SplitChild {
    pub(crate) layout: PaneLayout,
    pub(crate) weight: f64,
    pub(crate) fixed: Option<usize>,
    pub(crate) minimized: bool,
}

// How a window's screen area is currently divided among its panes.
// Leaf is the common case (an unsplit window, or one of a split's
// occupied regions); Split recurses, so nested splits (a horizontal
// divide with one side further vsplit) are represented naturally.
// `horizontal` names the divider's own orientation, matching vim's
// :split (horizontal divider, panes stacked top/bottom) vs :vsplit
// (vertical divider, panes side by side) -- see compute_regions for how
// this becomes actual screen rectangles.
pub(crate) enum PaneLayout {
    Leaf(PaneId),
    Split { horizontal: bool, children: Vec<SplitChild> },
}

pub(crate) struct WindowEntry {
    pub(crate) id: u32,
    // What this window is called, if anything -- `window create --name`
    // or `window rename`. Shown in the tab bar instead of the cwd, and
    // what `window select` finds it by. `None` is the ordinary case and
    // shows the cwd, which is what every window showed before names
    // existed.
    pub(crate) name: Option<String>,
    pub(crate) layout: PaneLayout,
    pub(crate) panes: Vec<Pane>,
    pub(crate) focused_pane: PaneId,
    pub(crate) next_pane_id: PaneId,
    // The `divider_budget` bishopt, kept here rather than read where it
    // is used: it is a *layout* parameter, and the geometry helpers that
    // need it (`pane_rect` above all) are pure functions of a window,
    // called from far more places than have a Shell to ask. Refreshed
    // from the owning session on every compositor redraw, so a change
    // lands on the next frame -- one frame's staleness for a setting
    // nobody changes mid-keystroke.
    pub(crate) divider_budget: usize,
}

// What `divider_budget` is when nothing has said otherwise -- the same
// number KNOWN_BISHOPTS defaults it to, and what a window built outside
// a session (a test) uses.
const DEFAULT_DIVIDER_BUDGET: usize = 25;

impl WindowEntry {
    pub(crate) fn single(id: u32, initial_frame: Frame) -> WindowEntry {
        WindowEntry {
            id,
            name: None,
            layout: PaneLayout::Leaf(0),
            panes: vec![Pane { id: 0, stack: vec![initial_frame], jumps: JumpList::default() }],
            focused_pane: 0,
            next_pane_id: 1,
            divider_budget: DEFAULT_DIVIDER_BUDGET,
        }
    }

    pub(crate) fn pane(&self, id: PaneId) -> &Pane {
        self.panes.iter().find(|p| p.id == id).expect("pane id always refers to a live pane in this window")
    }

    pub(crate) fn pane_mut(&mut self, id: PaneId) -> &mut Pane {
        self.panes.iter_mut().find(|p| p.id == id).expect("pane id always refers to a live pane in this window")
    }

    pub(crate) fn focused(&self) -> &Pane {
        self.pane(self.focused_pane)
    }

    // The focused pane's own frame stack -- everywhere this used to be
    // a plain field access (`window.stack`) before panes existed.
    pub(crate) fn stack(&self) -> &Vec<Frame> {
        &self.focused().stack
    }

    pub(crate) fn stack_mut(&mut self) -> &mut Vec<Frame> {
        let id = self.focused_pane;
        &mut self.pane_mut(id).stack
    }

    // The nearest Session frame at or below the top of the *focused*
    // pane's stack. In practice this only ever needs to look one level
    // down (a Job frame is always pushed onto a stack that already has
    // a Session beneath it, and nothing currently pushes a *second* Job
    // frame on top of a first), but walks generally rather than
    // assuming that.
    pub(crate) fn owning_session(&self) -> SessionId {
        self.focused().owning_session()
    }
}

// Finds `target`'s Leaf within the tree and turns it into a 2-way
// Split holding the original pane plus `new_id` -- UNLESS `target` is
// already a direct child of a Split whose own orientation matches
// `horizontal`, in which case `new_id` is simply inserted as another
// sibling of that same Split instead of nesting. That distinction is
// what keeps repeated same-direction splits an even N-way division
// (compute_regions splits one Split's area evenly among however many
// children it has) rather than each new split only ever halving
// whatever was there before.
pub(crate) fn insert_sibling(
    layout: PaneLayout,
    target: PaneId,
    new_id: PaneId,
    horizontal: bool,
    new_fixed: Option<usize>,
    new_minimized: bool,
) -> PaneLayout {
    match layout {
        PaneLayout::Leaf(id) if id == target => PaneLayout::Split {
            horizontal,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(id), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0, fixed: new_fixed, minimized: new_minimized },
            ],
        },
        PaneLayout::Leaf(id) => PaneLayout::Leaf(id),
        PaneLayout::Split { horizontal: h, children } => {
            let direct_child_idx = children.iter().position(|c| matches!(&c.layout, PaneLayout::Leaf(id) if *id == target));
            if let Some(idx) = direct_child_idx {
                if h == horizontal {
                    let mut children = children;
                    children
                        .insert(idx + 1, SplitChild { layout: PaneLayout::Leaf(new_id), weight: 1.0, fixed: new_fixed, minimized: new_minimized });
                    return PaneLayout::Split { horizontal: h, children };
                }
            }
            let children = children
                .into_iter()
                .map(|c| SplitChild {
                    layout: insert_sibling(c.layout, target, new_id, horizontal, new_fixed, new_minimized),
                    weight: c.weight,
                    fixed: c.fixed,
                    minimized: c.minimized,
                })
                .collect();
            PaneLayout::Split { horizontal: h, children }
        }
    }
}

// Removes `target`'s Leaf from the tree, collapsing any Split left with
// only one child down to that child directly (so a 2-way split closing
// one side goes back to a plain unsplit Leaf, and a 3-way split closing
// one side stays a 2-way split, not a redundant 1-child Split node --
// the survivors keep whatever weights they already had, same as
// closing a pane never touches its siblings' sizes).
fn remove_from_layout(layout: PaneLayout, target: PaneId) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf(id) => {
            if id == target {
                None
            } else {
                Some(PaneLayout::Leaf(id))
            }
        }
        PaneLayout::Split { horizontal, children } => {
            let new_children: Vec<SplitChild> = children
                .into_iter()
                .filter_map(|c| {
                    remove_from_layout(c.layout, target).map(|layout| SplitChild { layout, weight: c.weight, fixed: c.fixed, minimized: c.minimized })
                })
                .collect();
            match new_children.len() {
                0 => None,
                1 => new_children.into_iter().next().map(|c| c.layout),
                _ => Some(PaneLayout::Split { horizontal, children: new_children }),
            }
        }
    }
}

fn first_leaf(layout: &PaneLayout) -> PaneId {
    match layout {
        PaneLayout::Leaf(id) => *id,
        PaneLayout::Split { children, .. } => first_leaf(&children[0].layout),
    }
}

// Closes the currently focused pane of a window that has more than one
// -- called only once WindowAction::Close has already confirmed the
// focused pane's own stack has nothing left to reveal underneath (see
// its call site). Picks the new focused pane deterministically (the
// layout tree's own first leaf) rather than trying to guess a
// spatially "nearest" one -- simple and predictable, matching this
// first pane-support pass's plain-layout scope.
pub(crate) fn close_focused_pane(window: &mut WindowEntry) {
    close_pane(window, window.focused_pane);
}

// Generalizes close_focused_pane to any pane in the window, not just the
// focused one -- used to close a diagnostics sibling (never focused at
// the moment its own Edit frame quits, see run_edit_frame's NavExit::
// Quit arm) alongside close_focused_pane's own original call site. Only
// reassigns `focused_pane` when `pane_id` actually was the focused one;
// closing some *other* pane leaves focus exactly where it was.
pub(crate) fn close_pane(window: &mut WindowEntry, pane_id: PaneId) {
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = remove_from_layout(old_layout, pane_id).expect("closing one of >1 panes always leaves at least one behind");
    window.panes.retain(|p| p.id != pane_id);
    if window.focused_pane == pane_id {
        window.focused_pane = first_leaf(&window.layout);
    }
}

// A screen-coordinate rectangle within the compositor's content area
// (everything above the pinned tab-bar row), 0-indexed. Produced by
// compute_regions from a window's PaneLayout tree. pub(crate): fileeditor
// needs this shape too (a pane's own rect, exactly the same meaning) --
// see its own `drive`/`build_editor_frame` signatures.
#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
}

// Floor under any single child's weight when computing shares below --
// keeps a pane resized all the way down (or a pathological/negative
// weight from ever existing in the first place) from making its own
// share, or another sibling's via a tiny total, degenerate. The actual
// on-screen size still can't go below 1 row/col either way (see the
// `.max(1)` below), this only guards the weight arithmetic itself.
pub(crate) const MIN_PANE_WEIGHT: f64 = 0.05;

// Whether a divider line is drawn between `children[i]` and
// `children[i + 1]` -- every adjacent pair, except where either side is
// `minimized` (see SplitChild's own doc comment): a minimized pane's
// own single row already reads as the boundary between it and its
// neighbor, so a further, separate divider line right next to it would
// just be a redundant blank line. Shared by split_sizes (to know how
// much of the axis dividers actually consume) and compute_regions (to
// know where to actually draw one).
// Which children of a split are folded away behind a single "flipper"
// divider, as half-open index ranges.
//
// Every divider costs a row (or a column), and past a certain number of
// panes the dividers are most of what a split contains. So once they
// take more than `divider_budget` of the space (see that bishopt), the
// panes that are furthest from what you are actually looking at fold
// into one divider that stands for all of them, and unfold again as the
// focus moves back towards them.
//
// What stays is: the first child, the last child, the focused one, and
// the one either side of it. Keeping the ends is what makes the split
// still readable as a whole; keeping the neighbours is what makes moving
// the focus feel continuous rather than teleporting.
//
// **A run of exactly one child is never folded**, which is the rule that
// makes this behave sensibly rather than mechanically: a folded run and
// an ordinary divider both cost exactly one line, so folding a single
// child away buys nothing and costs you a pane. With eight panes `a`..`h`
// that produces, as the focus moves:
//
//     a [b] c ... h        focus b -- d,e,f,g fold
//     a b [c] d ... h      focus c -- e,f,g fold
//     a b c [d] e ... h    focus d -- b alone stays, f,g fold
//     a ... d [e] f g h    focus e -- b,c fold, g alone stays
fn collapsed_runs(n: usize, focused: usize) -> Vec<std::ops::Range<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let focused = focused.min(n - 1);
    let mut anchors = vec![0, n - 1, focused];
    if focused > 0 {
        anchors.push(focused - 1);
    }
    if focused + 1 < n {
        anchors.push(focused + 1);
    }
    anchors.sort_unstable();
    anchors.dedup();
    anchors.windows(2).map(|pair| pair[0] + 1..pair[1]).filter(|run| run.len() >= 2).collect()
}

// Whether the dividers a split needs take more of it than they are
// allowed to. `budget` is a percentage; 100 means never fold.
fn dividers_overflow(divider_count: usize, axis: usize, budget: usize) -> bool {
    budget < 100 && axis > 0 && divider_count * 100 > axis * budget
}

fn dividers_after(children: &[SplitChild]) -> Vec<bool> {
    (0..children.len().saturating_sub(1)).map(|i| !(children[i].minimized || children[i + 1].minimized)).collect()
}

// One Split's own children resolved down to how many cells each gets
// along the split's own axis (`usable` -- the area already minus every
// *actually drawn* divider strip, see dividers_after). A `minimized`
// child always claims exactly one cell, first, regardless of `fixed`/
// `weight` (both irrelevant while minimized). Of what's left, every
// `fixed`-child (see SplitChild's own doc comment) claims its own
// requested count next, clamped so it can never squeeze a plain
// `fixed: None` sibling below 1 cell; whatever's left after *that* then
// divides among those weighted siblings exactly the way this used to
// work before `fixed`/`minimized` existed -- proportional to weight/
// (sum of weighted siblings' weights), with the *last* weighted child
// (not necessarily the last child overall) absorbing the rounding
// remainder so the weighted children's own total always adds up
// exactly. With no `fixed`/`minimized` children at all (every existing
// call site, before this pane type), `weighted_count == children.len()`
// and this is byte-for-byte the original formula.
fn split_sizes(children: &[SplitChild], usable: usize) -> Vec<usize> {
    let mut sizes = vec![0usize; children.len()];
    let mut budget = usable;
    for (i, child) in children.iter().enumerate() {
        if child.minimized {
            let size = 1.min(budget);
            sizes[i] = size;
            budget = budget.saturating_sub(size);
        }
    }
    let weighted_count = children.iter().filter(|c| !c.minimized && c.fixed.is_none()).count();
    for (i, child) in children.iter().enumerate() {
        if let (false, Some(f)) = (child.minimized, child.fixed) {
            let reserve_for_weighted = weighted_count.min(budget);
            let size = f.min(budget.saturating_sub(reserve_for_weighted)).max(1.min(budget));
            sizes[i] = size;
            budget = budget.saturating_sub(size);
        }
    }
    let total_weight: f64 = children.iter().filter(|c| !c.minimized && c.fixed.is_none()).map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
    let mut allocated = 0usize;
    let mut seen = 0usize;
    for (i, child) in children.iter().enumerate() {
        if child.minimized || child.fixed.is_some() {
            continue;
        }
        seen += 1;
        let h = if seen == weighted_count {
            budget.saturating_sub(allocated).max(1)
        } else {
            (((budget as f64) * child.weight.max(MIN_PANE_WEIGHT) / total_weight).round() as usize).max(1)
        };
        sizes[i] = h;
        allocated += h;
    }
    sizes
}

// Walks `layout`, splitting `area` among each Split's children (see
// split_sizes, above, for how much each one gets) down to each Leaf's
// own rectangle. `dividers` collects the reserved divider strips
// separately (row=true for a horizontal divider line, running
// left-right; false for a vertical one, running top-bottom) so the
// caller can draw them after every pane's own content, rather than each
// Split trying to draw into space a child might otherwise want --
// skipped entirely between a minimized child and its neighbor (see
// dividers_after).
pub(crate) fn compute_regions(
    layout: &PaneLayout,
    area: Rect,
    focused: PaneId,
    budget: usize,
    out: &mut Vec<(PaneId, Rect)>,
    dividers: &mut Vec<Divider>,
) {
    compute_regions_at(layout, area, focused, budget, out, dividers, &mut Vec::new());
}

// Which child of this split holds `pane`, if any.
fn child_holding(children: &[SplitChild], pane: PaneId) -> Option<usize> {
    children.iter().position(|c| layout_holds(&c.layout, pane))
}

fn layout_holds(layout: &PaneLayout, pane: PaneId) -> bool {
    match layout {
        PaneLayout::Leaf(id) => *id == pane,
        PaneLayout::Split { children, .. } => children.iter().any(|c| layout_holds(&c.layout, pane)),
    }
}

fn leaves_of(layout: &PaneLayout, out: &mut Vec<PaneId>) {
    match layout {
        PaneLayout::Leaf(id) => out.push(*id),
        PaneLayout::Split { children, .. } => children.iter().for_each(|c| leaves_of(&c.layout, out)),
    }
}

// compute_regions' own recursion, carrying the path from the layout root
// down to whichever Split is being laid out right now -- each entry is
// the child index taken at that level. A divider records that path plus
// the index of the child it follows, which is the only way to name the
// thing a drag has to resize: a `SplitChild` has no identity of its own,
// and the pane ids underneath it belong to leaves that can be nested
// arbitrarily deep. `split_axis` is the axis length that child's own size
// is a share of, so a drag can turn a screen position straight into a
// fraction without re-deriving the layout.
fn compute_regions_at(
    layout: &PaneLayout,
    area: Rect,
    focused: PaneId,
    budget: usize,
    out: &mut Vec<(PaneId, Rect)>,
    dividers: &mut Vec<Divider>,
    path: &mut Vec<usize>,
) {
    match layout {
        PaneLayout::Leaf(id) => out.push((*id, area)),
        PaneLayout::Split { horizontal, children } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let draws_divider = dividers_after(children);
            let axis = if *horizontal { area.rows } else { area.cols };
            // Folded when the dividers would otherwise take more of this
            // split than they are allowed to. The runs are measured from
            // whichever child holds the focus -- an unfocused split
            // (the focus is in some other branch entirely) folds around
            // its first child, which is as good an anchor as any when
            // nothing here is being looked at.
            let folded = if dividers_overflow(draws_divider.iter().filter(|d| **d).count(), axis, budget) {
                collapsed_runs(n, child_holding(children, focused).unwrap_or(0))
            } else {
                Vec::new()
            };
            let hidden = |i: usize| folded.iter().any(|r| r.contains(&i));
            // A folded run costs exactly one line, and the ordinary
            // divider that would have followed the run's last child is
            // the line it costs -- so it is drawn instead of, not as
            // well as, that one.
            let divider_count = (0..n.saturating_sub(1)).filter(|i| draws_divider[*i] && !hidden(*i)).count() + folded.len();
            // Only the children that are actually drawn are sized:
            // handing `split_sizes` a zero-weight child would still
            // spend whatever minimum it guarantees on something with no
            // room on screen. `sizes` is indexed by *visible* position,
            // which `visible_index` maps back from a child index.
            let visible: Vec<SplitChild> = children
                .iter()
                .enumerate()
                .filter(|(i, _)| !hidden(*i))
                .map(|(_, c)| SplitChild { layout: PaneLayout::Leaf(0), weight: c.weight, fixed: c.fixed, minimized: c.minimized })
                .collect();
            let visible_index = |upto: usize| (0..upto).filter(|i| !hidden(*i)).count();
            if *horizontal {
                // Panes stacked top/bottom; the divider is the horizontal
                // line between them.
                let usable = area.rows.saturating_sub(divider_count);
                let sizes = split_sizes(&visible, usable);
                let mut row = area.row;
                for (i, child) in children.iter().enumerate() {
                    // A folded child gets no space, and its panes are
                    // still reported -- at the fold's own row, with no
                    // height -- so that focusing one still works and
                    // brings it straight back (see focus_pane_direction).
                    let h = if hidden(i) { 0 } else { sizes[visible_index(i)] };
                    path.push(i);
                    compute_regions_at(&child.layout, Rect { row, col: area.col, rows: h, cols: area.cols }, focused, budget, out, dividers, path);
                    path.pop();
                    row += h;
                    let ends_fold = folded.iter().find(|r| r.end == i + 1);
                    if let Some(run) = ends_fold {
                        dividers.push(Divider {
                            rect: Rect { row, col: area.col, rows: 1, cols: area.cols },
                            horizontal: true,
                            path: path.clone(),
                            child: i,
                            split_start: area.row,
                            split_axis: usable,
                            folded: Some(run.len()),
                        });
                        row += 1;
                    } else if i + 1 < n && draws_divider[i] && !hidden(i) {
                        dividers.push(Divider {
                            rect: Rect { row, col: area.col, rows: 1, cols: area.cols },
                            horizontal: true,
                            path: path.clone(),
                            child: i,
                            split_start: area.row,
                            split_axis: usable,
                            folded: None,
                        });
                        row += 1;
                    }
                }
            } else {
                // Panes side by side; the divider is the vertical line
                // between them.
                let usable = area.cols.saturating_sub(divider_count);
                let sizes = split_sizes(&visible, usable);
                let mut col = area.col;
                for (i, child) in children.iter().enumerate() {
                    let w = if hidden(i) { 0 } else { sizes[visible_index(i)] };
                    path.push(i);
                    compute_regions_at(&child.layout, Rect { row: area.row, col, rows: area.rows, cols: w }, focused, budget, out, dividers, path);
                    path.pop();
                    col += w;
                    let ends_fold = folded.iter().find(|r| r.end == i + 1);
                    if let Some(run) = ends_fold {
                        dividers.push(Divider {
                            rect: Rect { row: area.row, col, rows: area.rows, cols: 1 },
                            horizontal: false,
                            path: path.clone(),
                            child: i,
                            split_start: area.col,
                            split_axis: usable,
                            folded: Some(run.len()),
                        });
                        col += 1;
                    } else if i + 1 < n && draws_divider[i] && !hidden(i) {
                        dividers.push(Divider {
                            rect: Rect { row: area.row, col, rows: area.rows, cols: 1 },
                            horizontal: false,
                            path: path.clone(),
                            child: i,
                            split_start: area.col,
                            split_axis: usable,
                            folded: None,
                        });
                        col += 1;
                    }
                }
            }
        }
    }
}

// One divider strip, plus everything a drag needs to resize what it
// separates -- see compute_regions_at.
#[derive(Clone)]
pub(crate) struct Divider {
    pub(crate) rect: Rect,
    pub(crate) horizontal: bool,
    pub(crate) path: Vec<usize>,
    pub(crate) child: usize,
    // Where the owning Split's own area starts on the divider's axis, and
    // how much of that axis its children actually divide up (the area
    // minus the strips the dividers themselves occupy).
    pub(crate) split_start: usize,
    pub(crate) split_axis: usize,
    // How many panes this divider stands in for, when it is a folded
    // run rather than an ordinary line between two neighbours (see
    // collapsed_runs). `None` for every ordinary divider; a folded one
    // draws a count into itself and is not draggable, since there is no
    // single boundary for a drag to move.
    pub(crate) folded: Option<usize>,
}

// The first pane hidden behind a folded divider -- what clicking one
// focuses. `Divider::child` is the index of the run's last child, so the
// run's own panes are the ones this walks back over.
pub(crate) fn folded_divider_pane(window: &WindowEntry, divider: &Divider) -> Option<PaneId> {
    let folded = divider.folded?;
    let mut layout = &window.layout;
    for step in &divider.path {
        match layout {
            PaneLayout::Split { children, .. } => layout = &children.get(*step)?.layout,
            PaneLayout::Leaf(_) => return None,
        }
    }
    let PaneLayout::Split { children, .. } = layout else { return None };
    let first = (divider.child + 1).checked_sub(folded)?;
    let mut leaves = Vec::new();
    leaves_of(&children.get(first)?.layout, &mut leaves);
    leaves.first().copied()
}

// (col_origin, width): the real terminal column the given window's
// *focused* pane's own column 0 sits at, and how many columns of that
// row belong to it -- see editor::read_line's own doc comment for why
// both matter. (0, term_cols) -- the whole real terminal row --
// whenever there's nothing to offset by: not yet promoted
// (sinks_are_grid false -- read_line is drawing straight to the plain,
// unpaned real terminal), or promoted but this window isn't split (a
// single pane always owns the terminal's whole own row regardless of
// the window's own position in a next/previous cycle, since only one
// window is ever full-screen at a time).
// `(prompt_row, pane_bottom_row)` for the completion menu's own
// absolute-positioned path -- both real terminal rows, 0-indexed. The
// menu draws one row below the prompt and is skipped when that would
// fall past the pane (see redraw_with_completion_row).
//
// `None` only when there's no grid at all (an unpromoted terminal),
// where the menu falls back to its relative `\n`/draw/`\x1b[1A` dance
// instead. This used to return `None` for a *split* window too, on the
// grounds that a split needs neighbour-pane clamping the absolute path
// didn't have -- but it does: `pane_bottom_row` bounds it vertically and
// `focused_col_origin`'s own `(col, cols)` bounds it horizontally, both
// already the focused pane's rather than the terminal's. What genuinely
// can't be let near a pane is the *relative* path, whose newline scrolls
// the real terminal; that one is still reached only when there are no
// panes to spill into.
//
// `cursor_row` is the session's own vt100::Screen cursor row, which
// already tracks which row the upcoming prompt will occupy (it's fed a
// trailing "\r\n" every time a line is submitted) with none of real-
// terminal relative movement's scrolling ambiguity -- so no live query
// is needed to learn it.
pub(crate) fn completion_menu_rows(
    window: &WindowEntry,
    sinks_are_grid: bool,
    cursor_row: usize,
    term_rows: usize,
    term_cols: usize,
) -> Option<(usize, usize)> {
    if !sinks_are_grid {
        return None;
    }
    let rect = pane_rect(window, window.focused_pane, term_rows, term_cols);
    Some((rect.row + cursor_row, rect.row + rect.rows - 1))
}

pub(crate) fn focused_col_origin(window: &WindowEntry, sinks_are_grid: bool, term_rows: usize, term_cols: usize) -> (usize, usize) {
    if !sinks_are_grid || window.panes.len() <= 1 {
        return (0, term_cols);
    }
    let rect = pane_rect(window, window.focused_pane, term_rows, term_cols);
    (rect.col, rect.cols)
}

// The screen rectangle `pane_id` currently occupies within `window`,
// resolved against the real terminal size -- the read-only half of
// what snapshot_window computes, for callers (focused_col_origin, pane
// focus-change handling) that only need one pane's geometry rather than
// every pane's live screen reference too.
pub(crate) fn pane_rect(window: &WindowEntry, pane_id: PaneId, term_rows: usize, term_cols: usize) -> Rect {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    compute_regions(&window.layout, area, window.focused_pane, window.divider_budget, &mut regions, &mut dividers);
    regions.into_iter().find(|(id, _)| *id == pane_id).map(|(_, r)| r).expect("pane id always present in its own window's layout")
}

// How much one `window +`/`sizeup` or `-`/`sizedown` press changes the
// focused pane's own weight -- see SplitChild's own doc comment for why
// changing just one pane's weight (not its siblings') is enough to
// resize the whole split: compute_regions always divides by weight
// *share*, so growing one pane's weight relative to an unchanged total
// for the others already shrinks them proportionally.
pub(crate) const RESIZE_STEP: f64 = 0.2;

// Finds the Split node that directly contains `target` as one of its
// own children (not a further-nested grandchild), returning that
// Split's own orientation, a mutable handle onto its children, and
// target's index within them -- everything resize_focused_pane/
// set_focused_pane_size need to read or adjust *only* the target's own
// weight. None if the window isn't split at all (target is the whole
// layout, a bare Leaf with no enclosing Split).
// Walks `path` (see compute_regions_at) down to the Split it names.
fn split_children_at<'a>(layout: &'a mut PaneLayout, path: &[usize]) -> Option<&'a mut Vec<SplitChild>> {
    let mut node = layout;
    for step in path {
        match node {
            PaneLayout::Split { children, .. } => node = &mut children.get_mut(*step)?.layout,
            PaneLayout::Leaf(_) => return None,
        }
    }
    match node {
        PaneLayout::Split { children, .. } => Some(children),
        PaneLayout::Leaf(_) => None,
    }
}

// Gives `children[idx]` the share of its split's axis that `fraction`
// names, by solving for the weight that produces it: split_sizes divides
// the axis in proportion to weight/(sum of all siblings'), so a target
// fraction f wants weight = f * sum_others / (1 - f). Clamped well short
// of 0 and 1 so a drag to the very edge leaves both sides usable rather
// than collapsing one -- compute_regions' own `.max(1)` would keep a pane
// one cell tall, which is not a state anyone wants to drag back out of.
pub(crate) fn set_child_weight_to_fraction(children: &mut [SplitChild], idx: usize, fraction: f64) {
    let old_weight = children[idx].weight.max(MIN_PANE_WEIGHT);
    let total_weight: f64 = children.iter().map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
    let sum_others = (total_weight - old_weight).max(MIN_PANE_WEIGHT);
    let fraction = fraction.clamp(0.05, 0.95);
    children[idx].weight = (fraction * sum_others / (1.0 - fraction)).max(MIN_PANE_WEIGHT);
}

// Drags `divider` to real terminal row/column `to` (whichever axis it
// runs across), by resizing the child immediately before it. Returns
// whether the layout actually changed, so a caller driving a stream of
// motion events only repaints when something moved.
//
// A `fixed`-sized child (the diagnostics pane while expanded -- see
// SplitChild's own doc comment) has its `fixed` set directly instead:
// its weight is ignored while that's set, so solving for one would do
// nothing at all.
pub(crate) fn resize_split_child_to(window: &mut WindowEntry, divider: &Divider, to: usize) -> bool {
    if divider.split_axis == 0 {
        return false;
    }
    let desired = to.saturating_sub(divider.split_start).max(1).min(divider.split_axis.saturating_sub(1));
    let Some(children) = split_children_at(&mut window.layout, &divider.path) else {
        return false;
    };
    if divider.child >= children.len() {
        return false;
    }
    if children[divider.child].fixed.is_some() {
        if children[divider.child].fixed == Some(desired) {
            return false;
        }
        children[divider.child].fixed = Some(desired);
        return true;
    }
    let before = children[divider.child].weight;
    set_child_weight_to_fraction(children, divider.child, desired as f64 / divider.split_axis as f64);
    children[divider.child].weight != before
}

pub(crate) fn find_parent_split_mut(layout: &mut PaneLayout, target: PaneId) -> Option<(bool, &mut Vec<SplitChild>, usize)> {
    match layout {
        PaneLayout::Leaf(_) => None,
        PaneLayout::Split { horizontal, children } => {
            if let Some(idx) = children.iter().position(|c| matches!(&c.layout, PaneLayout::Leaf(id) if *id == target)) {
                return Some((*horizontal, children, idx));
            }
            for child in children.iter_mut() {
                if let Some(found) = find_parent_split_mut(&mut child.layout, target) {
                    return Some(found);
                }
            }
            None
        }
    }
}

// `window +`/`sizeup` (delta > 0) and `-`/`sizedown` (delta < 0): grows
// or shrinks the focused pane's own weight within its immediate parent
// split by RESIZE_STEP, floored at MIN_PANE_WEIGHT so repeated presses
// can never zero it out (compute_regions' own `.max(1)` on the
// resulting *rendered* size is the other half of that guarantee -- a
// pane can get very small, never invisible). A no-op if the window
// isn't split.
pub(crate) fn resize_focused_pane(window: &mut WindowEntry, delta: f64) {
    let focused = window.focused_pane;
    if let Some((_, children, idx)) = find_parent_split_mut(&mut window.layout, focused) {
        children[idx].weight = (children[idx].weight + delta).max(MIN_PANE_WEIGHT);
    }
}

#[cfg(test)]
mod jump_list_tests {
    use super::*;

    fn at(path: &str, row: usize) -> JumpEntry {
        JumpEntry { path: PathBuf::from(path), row, col: 0 }
    }

    // The property the whole feature exists for: one list spanning
    // files, so `Ctrl-O` after a cross-file `gd` steps back over the
    // file boundary and `Ctrl-I` steps forward over it again.
    #[test]
    fn back_and_forward_step_across_files_in_one_history() {
        let mut jumps = JumpList::default();
        jumps.push(at("a.rs", 1));
        jumps.push(at("a.rs", 30));
        assert_eq!(jumps.back(at("b.rs", 5)), Some(at("a.rs", 30)));
        assert_eq!(jumps.back(at("a.rs", 30)), Some(at("a.rs", 1)));
        assert_eq!(jumps.back(at("a.rs", 1)), None, "nothing further back");
        // ...and forward returns along the same path.
        assert_eq!(jumps.forward(at("a.rs", 1)), Some(at("a.rs", 30)));
        assert_eq!(jumps.forward(at("a.rs", 30)), Some(at("b.rs", 5)));
        assert_eq!(jumps.forward(at("b.rs", 5)), None);
    }

    #[test]
    fn a_new_jump_discards_the_forward_history() {
        let mut jumps = JumpList::default();
        jumps.push(at("a.rs", 1));
        assert_eq!(jumps.back(at("a.rs", 9)), Some(at("a.rs", 1)));
        assert!(jumps.forward.len() == 1, "there is somewhere to go forward to");
        // Taking a new branch discards it, as it does in a browser and
        // as it does in vim.
        jumps.push(at("a.rs", 20));
        assert_eq!(jumps.forward(at("a.rs", 20)), None);
    }

    // Vim collapses a repeat on the same line rather than stacking it,
    // which is what stops `Ctrl-O` needing several presses to leave one
    // spot.
    #[test]
    fn repeated_jumps_from_one_line_leave_a_single_entry() {
        let mut jumps = JumpList::default();
        jumps.push(at("a.rs", 5));
        jumps.push(JumpEntry { path: PathBuf::from("a.rs"), row: 5, col: 40 });
        assert_eq!(jumps.back.len(), 1);
        // The same line in a *different* file is a different place.
        jumps.push(at("b.rs", 5));
        assert_eq!(jumps.back.len(), 2);
    }

    #[test]
    fn the_list_is_bounded() {
        let mut jumps = JumpList::default();
        for row in 0..MAX_JUMPS + 20 {
            jumps.push(at("a.rs", row));
        }
        assert_eq!(jumps.back.len(), MAX_JUMPS);
        // The oldest went, not the newest.
        assert_eq!(jumps.back.last(), Some(&at("a.rs", MAX_JUMPS + 19)));
        assert!(!jumps.back.contains(&at("a.rs", 0)));
    }
}

#[cfg(test)]
mod divider_drag_tests {
    use super::*;

    fn leaf(id: PaneId, weight: f64) -> SplitChild {
        SplitChild { layout: PaneLayout::Leaf(id), weight, fixed: None, minimized: false }
    }

    fn window(layout: PaneLayout, panes: &[PaneId]) -> WindowEntry {
        WindowEntry {
            id: 0,
            name: None,
            layout,
            panes: panes.iter().map(|id| Pane { id: *id, stack: vec![Frame::Session(0)], jumps: JumpList::default() }).collect(),
            focused_pane: panes[0],
            divider_budget: DEFAULT_DIVIDER_BUDGET,
            next_pane_id: panes.len() as PaneId,
        }
    }

    fn dividers_of(w: &WindowEntry, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(&w.layout, area, w.focused_pane, w.divider_budget, &mut out, &mut dividers);
        dividers
    }

    const AREA: Rect = Rect { row: 0, col: 0, rows: 20, cols: 60 };

    #[test]
    fn a_divider_names_the_child_it_follows() {
        let w = window(PaneLayout::Split { horizontal: false, children: vec![leaf(0, 1.0), leaf(1, 1.0)] }, &[0, 1]);
        let d = dividers_of(&w, AREA);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, Vec::<usize>::new(), "the root split");
        assert_eq!(d[0].child, 0, "the divider follows the first child");
        assert!(!d[0].horizontal);
        // 60 columns minus the one the divider itself occupies.
        assert_eq!((d[0].split_start, d[0].split_axis), (0, 59));
    }

    #[test]
    fn a_nested_splits_divider_carries_the_path_to_reach_it() {
        // Left half is itself split top/bottom -- its divider must name
        // that inner split, not the outer one, or a drag would resize
        // completely the wrong boundary.
        let inner = PaneLayout::Split { horizontal: true, children: vec![leaf(1, 1.0), leaf(2, 1.0)] };
        let layout = PaneLayout::Split {
            horizontal: false,
            children: vec![SplitChild { layout: inner, weight: 1.0, fixed: None, minimized: false }, leaf(3, 1.0)],
        };
        let w = window(layout, &[1, 2, 3]);
        let d = dividers_of(&w, AREA);
        let nested = d.iter().find(|d| d.horizontal).expect("the inner top/bottom divider");
        assert_eq!(nested.path, vec![0], "reached by taking the outer split's first child");
        assert_eq!(nested.child, 0);
        let outer = d.iter().find(|d| !d.horizontal).expect("the outer left/right divider");
        assert_eq!(outer.path, Vec::<usize>::new());
    }

    #[test]
    fn dragging_moves_the_boundary_to_where_it_was_dropped() {
        let mut w = window(PaneLayout::Split { horizontal: false, children: vec![leaf(0, 1.0), leaf(1, 1.0)] }, &[0, 1]);
        let d = dividers_of(&w, AREA)[0].clone();
        assert_eq!(d.rect.col, 30, "an even split of 59 usable columns");
        assert!(resize_split_child_to(&mut w, &d, 15));
        assert_eq!(dividers_of(&w, AREA)[0].rect.col, 15, "the divider followed the pointer");
        let d = dividers_of(&w, AREA)[0].clone();
        assert!(resize_split_child_to(&mut w, &d, 45));
        assert_eq!(dividers_of(&w, AREA)[0].rect.col, 45);
    }

    #[test]
    fn dragging_a_horizontal_divider_works_on_the_other_axis() {
        let mut w = window(PaneLayout::Split { horizontal: true, children: vec![leaf(0, 1.0), leaf(1, 1.0)] }, &[0, 1]);
        let d = dividers_of(&w, AREA)[0].clone();
        assert!(d.horizontal);
        assert!(resize_split_child_to(&mut w, &d, 5));
        assert_eq!(dividers_of(&w, AREA)[0].rect.row, 5);
    }

    #[test]
    fn dragging_past_either_end_leaves_both_panes_usable() {
        let mut w = window(PaneLayout::Split { horizontal: false, children: vec![leaf(0, 1.0), leaf(1, 1.0)] }, &[0, 1]);
        let d = dividers_of(&w, AREA)[0].clone();
        resize_split_child_to(&mut w, &d, 0);
        let col = dividers_of(&w, AREA)[0].rect.col;
        assert!(col > 0 && col < 59, "collapsed to {col}");
        let d = dividers_of(&w, AREA)[0].clone();
        resize_split_child_to(&mut w, &d, 10_000);
        let col = dividers_of(&w, AREA)[0].rect.col;
        assert!(col > 0 && col < 59, "collapsed to {col}");
    }

    #[test]
    fn a_fixed_size_child_is_resized_by_its_fixed_size_not_its_weight() {
        // The diagnostics pane while expanded (see SplitChild) -- its
        // weight is ignored entirely while `fixed` is set, so solving for
        // one would move nothing at all.
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: Some(6), minimized: false }, leaf(1, 1.0)],
        };
        let mut w = window(layout, &[0, 1]);
        let d = dividers_of(&w, AREA)[0].clone();
        assert_eq!(d.rect.row, 6);
        assert!(resize_split_child_to(&mut w, &d, 12));
        assert_eq!(dividers_of(&w, AREA)[0].rect.row, 12);
        match &w.layout {
            PaneLayout::Split { children, .. } => assert_eq!(children[0].fixed, Some(12)),
            _ => panic!("layout shape changed"),
        }
    }

    #[test]
    fn a_press_on_the_strip_is_a_divider_not_the_pane_beside_it() {
        let w = window(PaneLayout::Split { horizontal: false, children: vec![leaf(0, 1.0), leaf(1, 1.0)] }, &[0, 1]);
        let d = dividers_of(&w, AREA)[0].clone();
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(&w.layout, AREA, w.focused_pane, w.divider_budget, &mut out, &mut dividers);
        // Nothing overlaps: the divider column belongs to no pane rect.
        assert!(!out.iter().any(|(_, r)| d.rect.col >= r.col && d.rect.col < r.col + r.cols));
    }
}

#[cfg(test)]
mod pane_layout_tests {
    use super::*;

    fn regions(layout: &PaneLayout, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(layout, area, 0, DEFAULT_DIVIDER_BUDGET, &mut out, &mut dividers);
        out
    }

    #[test]
    fn a_fixed_child_gets_exactly_its_own_size_and_the_weighted_sibling_absorbs_the_rest() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(1), minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let out = regions(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        // 10 rows - 1 divider = 9 usable; the fixed child takes exactly
        // 1, the lone weighted child absorbs the remaining 8.
        assert_eq!(r1.rows, 1);
        assert_eq!(r0.rows, 8);
    }

    #[test]
    fn every_existing_split_still_divides_evenly_with_no_fixed_children() {
        // Byte-for-byte the pre-`fixed`-field formula: three even
        // weighted children in a 21-row area (20 usable after 2
        // dividers) split 7/7/6, the last absorbing the remainder.
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(2), weight: 1.0, fixed: None, minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 22, cols: 20 };
        let out = regions(&layout, area);
        let rows_of = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1.rows;
        assert_eq!(rows_of(0) + rows_of(1) + rows_of(2), 20);
        assert_eq!(rows_of(0), rows_of(1));
    }

    #[test]
    fn a_fixed_request_larger_than_the_area_is_clamped_so_the_weighted_sibling_still_gets_a_row() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(10), minimized: false },
            ],
        };
        // 3 rows - 1 divider = 2 usable; a fixed request of 10 must be
        // clamped so the weighted sibling isn't squeezed to 0.
        let area = Rect { row: 0, col: 0, rows: 3, cols: 20 };
        let out = regions(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r0.rows, 1);
        assert_eq!(r1.rows, 1);
    }

    #[test]
    fn a_fixed_child_works_along_the_column_axis_of_a_vertical_split_too() {
        let layout = PaneLayout::Split {
            horizontal: false,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: Some(5), minimized: false },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 30 };
        let out = regions(&layout, area);
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r1.cols, 5);
        assert_eq!(r1.rows, 10);
    }

    fn regions_and_dividers(layout: &PaneLayout, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        let mut out = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(layout, area, 0, DEFAULT_DIVIDER_BUDGET, &mut out, &mut dividers);
        (out, dividers)
    }

    #[test]
    fn a_minimized_child_takes_exactly_one_row_with_no_separate_divider_reserved() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let (out, dividers) = regions_and_dividers(&layout, area);
        let r0 = out.iter().find(|(id, _)| *id == 0).unwrap().1;
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        // No divider at all between them: the minimized pane's own row
        // is the whole boundary. 0's own 9 rows + 1's own 1 row fill
        // the entire 10-row area exactly, unlike the ordinary
        // fixed-child case (which still reserves 1 row for a real
        // divider on top of the fixed child's own size).
        assert!(dividers.is_empty());
        assert_eq!(r1.rows, 1);
        assert_eq!(r0.rows, 9);
        assert_eq!(r0.row, 0);
        assert_eq!(r1.row, 9);
    }

    #[test]
    fn minimized_ignores_its_own_fixed_and_weight() {
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 5.0, fixed: Some(4), minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 10, cols: 20 };
        let out = regions(&layout, area);
        let r1 = out.iter().find(|(id, _)| *id == 1).unwrap().1;
        assert_eq!(r1.rows, 1);
    }

    #[test]
    fn a_normal_divider_still_separates_two_ordinary_siblings_next_to_a_minimized_one() {
        // Three children: ordinary, ordinary, minimized -- the boundary
        // between the first two still gets a real divider; only the one
        // next to the minimized child is skipped.
        let layout = PaneLayout::Split {
            horizontal: true,
            children: vec![
                SplitChild { layout: PaneLayout::Leaf(0), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(1), weight: 1.0, fixed: None, minimized: false },
                SplitChild { layout: PaneLayout::Leaf(2), weight: 1.0, fixed: None, minimized: true },
            ],
        };
        let area = Rect { row: 0, col: 0, rows: 11, cols: 20 };
        let (out, dividers) = regions_and_dividers(&layout, area);
        assert_eq!(dividers.len(), 1);
        let r2 = out.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(r2.rows, 1);
        // 11 rows - 1 real divider - 1 minimized row = 9 usable for the
        // two ordinary weighted siblings, split evenly.
        let rows_of = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1.rows;
        assert_eq!(rows_of(0) + rows_of(1), 9);
    }
}

#[cfg(test)]
mod tab_fold_tests {
    use super::*;

    // The worked examples from the feature's own description, with eight
    // panes `a`..`h` and the focus moving right one at a time.
    #[test]
    fn folding_keeps_the_ends_the_focus_and_its_neighbours() {
        let show = |focused: usize| {
            let runs = collapsed_runs(8, focused);
            let mut out = String::new();
            let mut i = 0;
            while i < 8 {
                if let Some(run) = runs.iter().find(|r| r.start == i) {
                    out.push_str("... ");
                    i = run.end;
                    continue;
                }
                let name = (b'a' + i as u8) as char;
                if i == focused {
                    out.push_str(&format!("[{name}] "));
                } else {
                    out.push_str(&format!("{name} "));
                }
                i += 1;
            }
            out.trim_end().to_string()
        };
        assert_eq!(show(1), "a [b] c ... h");
        assert_eq!(show(2), "a b [c] d ... h");
        assert_eq!(show(3), "a b c [d] e ... h");
        assert_eq!(show(4), "a ... d [e] f g h");
    }

    // The rule that keeps this from being merely mechanical: a folded
    // run and an ordinary divider cost the same one line, so folding a
    // lone child buys nothing and costs you a pane.
    #[test]
    fn a_run_of_one_is_never_folded() {
        // focus d (index 3) leaves exactly `b` between the first child
        // and the focus's own neighbour -- kept.
        assert_eq!(collapsed_runs(8, 3), vec![5..7]);
        // ...and focus e leaves exactly `g` at the other end -- kept.
        assert_eq!(collapsed_runs(8, 4), vec![1..3]);
    }

    // Four panes can't fold from any focus: the ends, the focus and its
    // two neighbours are already all of them, and whatever is left over
    // is a run of one.
    #[test]
    fn four_panes_or_fewer_never_fold() {
        for n in 0..=4 {
            for focused in 0..n.max(1) {
                assert!(collapsed_runs(n, focused).is_empty(), "n={n} focused={focused}");
            }
        }
    }

    // Five is the first count that can, and only from an end -- from the
    // middle every child is still an anchor or a lone leftover.
    #[test]
    fn five_panes_fold_only_from_an_end() {
        assert_eq!(collapsed_runs(5, 0), vec![2..4], "[a] b ... e");
        assert_eq!(collapsed_runs(5, 4), vec![1..3], "a ... d [e]");
        assert!(collapsed_runs(5, 2).is_empty(), "a b [c] d e");
    }

    #[test]
    fn folding_never_hides_the_focus_its_neighbours_or_the_ends() {
        for n in 1..40 {
            for focused in 0..n {
                let runs = collapsed_runs(n, focused);
                let hidden = |i: usize| runs.iter().any(|r| r.contains(&i));
                assert!(!hidden(0) && !hidden(n - 1), "n={n} focused={focused}: an end folded");
                assert!(!hidden(focused), "n={n} focused={focused}: the focus folded");
                assert!(focused == 0 || !hidden(focused - 1), "n={n} focused={focused}");
                assert!(focused + 1 == n || !hidden(focused + 1), "n={n} focused={focused}");
                // ...and the runs stay ordered and disjoint.
                assert!(runs.windows(2).all(|w| w[0].end < w[1].start), "n={n} focused={focused}: runs touch");
            }
        }
    }

    #[test]
    fn the_budget_decides_whether_anything_folds_at_all() {
        // Seven dividers in forty rows is 17.5%.
        assert!(!dividers_overflow(7, 40, 25));
        assert!(dividers_overflow(7, 40, 10));
        // 100 means never, whatever the numbers say.
        assert!(!dividers_overflow(39, 40, 100));
        // ...and nothing divides by zero on a split with no room.
        assert!(!dividers_overflow(7, 0, 25));
    }

    // The layout half: folded children really do get no space, the
    // dividers they would have needed are gone, and one divider stands
    // in their place carrying the count.
    #[test]
    fn a_folded_run_gives_up_its_rows_to_the_panes_that_are_left() {
        // Ten panes stacked in twenty rows: nine dividers is 45%, over
        // the 25% budget.
        let children: Vec<SplitChild> =
            (0..10).map(|i| SplitChild { layout: PaneLayout::Leaf(i), weight: 1.0, fixed: None, minimized: false }).collect();
        let layout = PaneLayout::Split { horizontal: true, children };
        let area = Rect { row: 0, col: 0, rows: 20, cols: 40 };
        let mut regions = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(&layout, area, 1, 25, &mut regions, &mut dividers);

        // Focused pane 1, so panes 0,1,2 and 9 keep their rows and
        // 3..=8 fold.
        let rows = |id: PaneId| regions.iter().find(|(p, _)| *p == id).unwrap().1.rows;
        for id in [0, 1, 2, 9] {
            assert!(rows(id) > 0, "pane {id} should still be drawn");
        }
        for id in 3..=8 {
            assert_eq!(rows(id), 0, "pane {id} should be folded away");
        }
        // Every pane is still reported, folded or not -- that is what
        // keeps focusing one possible.
        assert_eq!(regions.len(), 10);
        // Three ordinary dividers and one fold, not nine.
        assert_eq!(dividers.len(), 4);
        assert_eq!(dividers.iter().filter(|d| d.folded == Some(6)).count(), 1);
        // ...and the rows add up to the area.
        let used: usize = regions.iter().map(|(_, r)| r.rows).sum::<usize>() + dividers.len();
        assert_eq!(used, area.rows);
    }

    // Under the budget nothing folds, however many panes there are.
    #[test]
    fn a_generous_budget_folds_nothing() {
        let children: Vec<SplitChild> =
            (0..10).map(|i| SplitChild { layout: PaneLayout::Leaf(i), weight: 1.0, fixed: None, minimized: false }).collect();
        let layout = PaneLayout::Split { horizontal: true, children };
        let mut regions = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(&layout, Rect { row: 0, col: 0, rows: 20, cols: 40 }, 1, 100, &mut regions, &mut dividers);
        assert_eq!(dividers.len(), 9);
        assert!(dividers.iter().all(|d| d.folded.is_none()));
        assert!(regions.iter().all(|(_, r)| r.rows > 0));
    }

    // Moving the focus is what unfolds: the same layout, a different
    // pane in front, and a different set is hidden.
    #[test]
    fn the_fold_follows_the_focus() {
        let hidden_for = |focused: PaneId| {
            let children: Vec<SplitChild> =
                (0..10).map(|i| SplitChild { layout: PaneLayout::Leaf(i), weight: 1.0, fixed: None, minimized: false }).collect();
            let layout = PaneLayout::Split { horizontal: true, children };
            let mut regions = Vec::new();
            let mut dividers = Vec::new();
            compute_regions(&layout, Rect { row: 0, col: 0, rows: 20, cols: 40 }, focused, 25, &mut regions, &mut dividers);
            let mut hidden: Vec<PaneId> = regions.iter().filter(|(_, r)| r.rows == 0).map(|(p, _)| *p).collect();
            hidden.sort_unstable();
            hidden
        };
        assert_eq!(hidden_for(1), vec![3, 4, 5, 6, 7, 8]);
        assert_eq!(hidden_for(5), vec![1, 2, 3, 7, 8]);
        // Whatever is focused is never hidden, at any position.
        for focused in 0..10 {
            assert!(!hidden_for(focused).contains(&focused));
        }
    }
}
