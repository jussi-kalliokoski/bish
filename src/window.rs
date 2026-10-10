// The tiling window manager: the pane tree, each pane's frame stack, and
// the geometry turning them into rectangles on a terminal.
//
// Tree algebra over `PaneId` only: nothing here knows about sessions,
// shells or drawing. repl.rs keeps what touches a `SessionState` or the
// terminal (`compositor_redraw`, the `split_*_pane` functions, which
// allocate sessions, the `render_*` family, `sync_*_pane`).

use std::path::PathBuf;

// Long enough: a list you can't walk out of is no more useful than a
// short one, and each entry holds a path.
const MAX_JUMPS: usize = 100;

// Reserves the terminal's last two rows: the tab bar (on the last row)
// and above it the global mode line and command row
// (`render_global_status_row`, `command_mode_row`). Panes divide what's
// above both.
pub(crate) fn content_rows(term_rows: usize) -> usize {
    term_rows.saturating_sub(2).max(1)
}

pub(crate) type SessionId = u32;

pub(crate) type JobFrameId = u32;

pub(crate) type EditFrameId = u32;

pub(crate) type HexFrameId = u32;

// One layer of a pane's view stack; the top one is shown.
//
// - `Session`: a shell; one session can show in several panes.
// - `Job`: a foregrounded job, driven by polling its pty
//   (`exec::FgJob`, `drive_fg_job` in repl.rs). An id into `job_frames`
//   rather than the job itself, as a job is in one place at a time and
//   this stays `Copy`.
// - `Edit`: an `e` editor session (`fileeditor::EditSession`),
//   detachable with Ctrl+G; an id into `edit_frames`, for the same reason.
// - `Hex`: `e --hex`, like `Edit` over bytes (see hexedit.rs); an id
//   into `hex_frames`.
// - `Diagnostics`: the `:diag` sibling pane below an `Edit` frame's
//   pane, listing that buffer's diagnostics, named by its
//   `EditFrameId`. Holds no state: it's reread from `edit_frames` each
//   time (see `split_diagnostics_pane`).
// - `DebugRun`: the `:dbg` sibling under an `Edit` frame's pane (see
//   `split_debug_run_pane`), with state worth keeping (the script's
//   shell, output, pause state) in `debug_frames`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Frame {
    Session(SessionId),
    Job(JobFrameId),
    Edit(EditFrameId),
    Hex(HexFrameId),
    Diagnostics(EditFrameId),
    /// A list of places, such as `gr`'s references: a sibling pane of the
    /// editor frame it was asked from, like `Diagnostics`, collapsing to a
    /// title row when unfocused. Its contents are in `repl::run`'s
    /// `location_lists`: places in other files aren't a property of one
    /// buffer.
    Locations(EditFrameId),
    DebugRun(EditFrameId),
}

pub(crate) type PaneId = u32;

/// One position in the jump list: which file, and where. The path makes
/// it cross-file, and is why the list can't live in `VimKeys`, which
/// belongs to one `EditSession`: a file opened by `gd` gets a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JumpEntry {
    pub(crate) path: PathBuf,
    pub(crate) row: usize,
    pub(crate) col: usize,
}

/// `Ctrl-O`/`Ctrl-I`'s history, as in vim: a jump pushes where you were
/// and drops the forward history; `Ctrl-O` steps back, pushing where you
/// are so `Ctrl-I` can return; `Ctrl-I` mirrors it. On the `Pane`, as
/// vim's is per window, and it must outlive any one buffer.
#[derive(Debug, Default)]
pub(crate) struct JumpList {
    pub(crate) back: Vec<JumpEntry>,
    pub(crate) forward: Vec<JumpEntry>,
}

impl JumpList {
    /// Records where a jump leaves from, dropping the forward history, as
    /// following a new link in a browser does.
    pub(crate) fn push(&mut self, entry: JumpEntry) {
        // A repeat of the same line collapses rather than stacks, as in
        // vim, so `Ctrl-O` leaves a spot in one press.
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

// One pane of a window, with its own frame stack; the top frame is what's
// shown and driven. `window close` or EOF pops it; an emptied pane closes
// (see `apply_window_action`'s Close arm), collapsing its split, or closes
// the window if it was the only pane.
pub(crate) struct Pane {
    pub(crate) id: PaneId,
    pub(crate) stack: Vec<Frame>,
    /// See `JumpList`: per pane, as vim's is per window.
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

// One child of a Split: its subtree and its share of the split relative
// to its siblings. Weights aren't normalised: `compute_regions` divides
// by weight over the sum, so two children at 1.0 split evenly, and
// `window +`/`-`/`size` (`resize_focused_pane`,
// `set_focused_pane_size` in repl.rs) change only this child's weight.
//
// `fixed`, when set, gives this child exactly that many rows (horizontal
// split) or columns (vertical), taken before the rest divide by weight;
// `weight` is ignored meanwhile. The expanded diagnostics pane uses it to
// fit its list, capped at half the space by `diagnostics_pane_rows`.
//
// `minimized` folds the child to one row or column, and
// `compute_regions` reserves no divider beside it, as that row reads as
// the divider (see `render_diagnostics_title` for the pill drawn there).
// `fixed` and `weight` don't apply meanwhile.
pub(crate) struct SplitChild {
    pub(crate) layout: PaneLayout,
    pub(crate) weight: f64,
    pub(crate) fixed: Option<usize>,
    pub(crate) minimized: bool,
}

// How a window's area is divided among its panes. `Leaf` is one pane;
// `Split` recurses for nested splits. `horizontal` names the divider's
// orientation, as vim's `:split` (stacked) versus `:vsplit` (side by
// side); see `compute_regions`.
pub(crate) enum PaneLayout {
    Leaf(PaneId),
    Split { horizontal: bool, children: Vec<SplitChild> },
}

// A window: panes arranged by `layout`, `focused_pane` taking input.
// `stack()`/`stack_mut()` resolve to the focused pane's frame stack. A
// session can top several panes' stacks, so it's dropped only when no pane
// references it at any depth (`close_orphaned_sessions`).
pub(crate) struct WindowEntry {
    pub(crate) id: u32,
    // The window's name, from `window create --name` or `window rename`:
    // shown in the tab bar instead of the cwd, and what `window select`
    // finds. Usually `None`.
    pub(crate) name: Option<String>,
    pub(crate) layout: PaneLayout,
    pub(crate) panes: Vec<Pane>,
    pub(crate) focused_pane: PaneId,
    pub(crate) next_pane_id: PaneId,
    // The `divider_budget` bishopt, kept here because it's a layout
    // parameter and the geometry helpers (`pane_rect` above all) are pure
    // functions of a window, called where there's no shell to ask. Refreshed
    // from the owning session on every compositor redraw.
    pub(crate) divider_budget: usize,
    // The pane temporarily filling the window (`<C-w>z`/`<C-w>o`). The
    // split tree is left as it was, so unzooming restores it, weights and
    // all, and `pane_rect` keeps answering for hidden panes from that tree,
    // so no hidden grid is resized (a resize reflows scrollback). Only ever
    // the focused pane: every focus move clears it first, so `pane_rect` can
    // treat "not zoomed" as "not visible".
    pub(crate) zoomed: Option<PaneId>,
    // The last named arrangement `::bish window layout` applied, which
    // `layout next` cycles from; not a claim the window still is that
    // layout. Later splits and resizes leave it alone.
    pub(crate) last_layout: Option<NamedLayout>,
}

// `divider_budget`'s default, as in KNOWN_BISHOPTS, for a window built
// outside a session (a test).
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
            zoomed: None,
            last_layout: None,
        }
    }

    /// Every pane's rectangle and every divider for this window now, zoom
    /// included. The only way to a window's geometry: `compute_regions`, the
    /// tree half, is private, so nobody can lay a window out forgetting the
    /// zoom.
    pub(crate) fn regions(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<Divider>) {
        match self.zoomed {
            // A zoomed pane has no siblings on screen, so no dividers.
            Some(id) => out.push((id, area)),
            None => compute_regions(&self.layout, area, self.focused_pane, self.divider_budget, out, dividers),
        }
    }

    /// Adds a pane holding `stack` by splitting the focused one, and returns
    /// its id. It isn't focused: an ordinary split does that itself, and
    /// sibling panes (diagnostics, locations, a debug run) don't want it.
    ///
    /// The only way a pane enters a window, so callers can't forget a zoom
    /// (the split comes back first, or the new pane would be invisible) or
    /// get the `SplitChild` flags wrong; `insert_sibling` is private for the
    /// same reason.
    pub(crate) fn add_pane(&mut self, stack: Vec<Frame>, horizontal: bool, minimized: bool) -> PaneId {
        self.unzoom();
        let new_id = self.next_pane_id;
        self.next_pane_id += 1;
        self.panes.push(Pane { id: new_id, stack, jumps: JumpList::default() });
        let focused = self.focused_pane;
        let old_layout = std::mem::replace(&mut self.layout, PaneLayout::Leaf(0));
        self.layout = insert_sibling(old_layout, focused, new_id, horizontal, None, minimized);
        new_id
    }

    /// Rebuilds the layout as a named arrangement (`::bish window layout
    /// tiled`). Panes keep their identity and contents; weights go back to
    /// even and minimized or fixed sizes are cleared, as an arrangement asked
    /// for by name shouldn't carry the last drag's proportions.
    ///
    /// "Main" in the `Main*` layouts is the focused pane, so the key that
    /// arranges the window also says what it's arranged around.
    pub(crate) fn apply_layout(&mut self, layout: NamedLayout) {
        self.unzoom();
        let mut panes = Vec::new();
        leaves_of(&self.layout, &mut panes);
        if panes.len() < 2 {
            return;
        }
        // The focused pane leads, as the `Main*` layouts mean, keeping the
        // others' order stable around it.
        if let Some(at) = panes.iter().position(|id| *id == self.focused_pane) {
            panes.rotate_left(at);
        }
        self.layout = build_named_layout(layout, &panes);
        self.last_layout = Some(layout);
    }

    /// `::bish window layout next`: the layout after the last one applied, or
    /// the first.
    pub(crate) fn next_layout(&self) -> NamedLayout {
        match self.last_layout {
            Some(current) => current.next(),
            None => NamedLayout::ALL[0],
        }
    }

    /// Exchanges two panes' places (`<C-w>x`). Only the leaf labels move, so
    /// each position keeps its size and no weight, fold or minimized flag can
    /// be lost.
    pub(crate) fn swap_panes(&mut self, a: PaneId, b: PaneId) {
        fn relabel(layout: &mut PaneLayout, a: PaneId, b: PaneId) {
            match layout {
                PaneLayout::Leaf(id) if *id == a => *id = b,
                PaneLayout::Leaf(id) if *id == b => *id = a,
                PaneLayout::Leaf(_) => {}
                PaneLayout::Split { children, .. } => children.iter_mut().for_each(|c| relabel(&mut c.layout, a, b)),
            }
        }
        if a != b {
            relabel(&mut self.layout, a, b);
        }
    }

    /// Every pane in layout order: what `::bish window focus <n>` counts, and
    /// how a saved layout replays.
    pub(crate) fn panes_in_layout_order(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        leaves_of(&self.layout, &mut out);
        out
    }

    /// The focused pane's place in that order, from 1: what `window focus`
    /// would take to come back.
    pub(crate) fn focused_position(&self) -> usize {
        self.panes_in_layout_order().iter().position(|id| *id == self.focused_pane).map_or(1, |at| at + 1)
    }

    /// `::bish window focus <n>`: focuses the nth pane in layout order, from
    /// 1. `false` if there's none.
    pub(crate) fn focus_position(&mut self, position: usize) -> bool {
        let panes = self.panes_in_layout_order();
        match position.checked_sub(1).and_then(|at| panes.get(at)) {
            Some(id) => {
                self.unzoom();
                self.focused_pane = *id;
                true
            }
            None => false,
        }
    }

    /// The `::bish window` commands that rebuild this window's pane tree
    /// from a new window's single pane.
    ///
    /// Replay relies on `insert_sibling`: a split puts the new pane right
    /// after the one split, and the new pane takes focus, so consecutive
    /// same-orientation splits lay panes out in order. A split matching its
    /// parent's orientation flattens into it, so no tree nests two alike,
    /// and splitting inside a subtree nests as the target says.
    ///
    /// Sizes aren't included: the commands that set them work in rows and
    /// columns of the terminal at the time, which another terminal would
    /// get wrong.
    pub(crate) fn rebuild_commands(&self) -> Vec<String> {
        // Planned against a mirror of the real tree (the same
        // `insert_sibling` and focus rule), so the positions emitted are
        // the ones the commands will find.
        struct Replay {
            layout: PaneLayout,
            focused: PaneId,
            next_id: PaneId,
            commands: Vec<String>,
        }
        impl Replay {
            fn split(&mut self, horizontal: bool) -> PaneId {
                let id = self.next_id;
                self.next_id += 1;
                let old = std::mem::replace(&mut self.layout, PaneLayout::Leaf(0));
                self.layout = insert_sibling(old, self.focused, id, horizontal, None, false);
                self.focused = id;
                self.commands.push(match horizontal {
                    true => "::bish window vsplit".to_string(),
                    false => "::bish window split".to_string(),
                });
                id
            }
            fn focus(&mut self, pane: PaneId) {
                if self.focused == pane {
                    return;
                }
                let mut order = Vec::new();
                leaves_of(&self.layout, &mut order);
                if let Some(at) = order.iter().position(|id| *id == pane) {
                    self.commands.push(format!("::bish window focus {}", at + 1));
                    self.focused = pane;
                }
            }
            // Builds `node` into the slot `holder` currently occupies.
            fn build(&mut self, node: &PaneLayout, holder: PaneId) {
                let PaneLayout::Split { horizontal, children } = node else { return };
                self.focus(holder);
                let mut slots = vec![holder];
                for _ in 1..children.len() {
                    slots.push(self.split(*horizontal));
                }
                for (child, slot) in children.iter().zip(slots) {
                    self.build(&child.layout, slot);
                }
            }
        }

        let root = self.panes_in_layout_order().first().copied().unwrap_or(0);
        let mut replay = Replay { layout: PaneLayout::Leaf(root), focused: root, next_id: root + 1, commands: Vec::new() };
        replay.build(&self.layout, root);
        replay.commands
    }

    /// The pane after `from` in layout order, wrapping: what `<C-w>x`
    /// exchanges with by default. Layout order is stable and visits every
    /// pane, so repeated `<C-w>x` cycles rather than oscillating.
    pub(crate) fn pane_after(&self, from: PaneId) -> PaneId {
        let mut leaves = Vec::new();
        leaves_of(&self.layout, &mut leaves);
        match leaves.iter().position(|id| *id == from) {
            Some(at) => leaves[(at + 1) % leaves.len()],
            None => from,
        }
    }

    /// Takes `pane_id` out of this window, returning it, for `window break`.
    /// `None` when it's the only pane: breaking it out would just rename the
    /// window.
    pub(crate) fn detach_pane(&mut self, pane_id: PaneId) -> Option<Pane> {
        if self.panes.len() < 2 {
            return None;
        }
        let at = self.panes.iter().position(|p| p.id == pane_id)?;
        let pane = self.panes.remove(at);
        let old_layout = std::mem::replace(&mut self.layout, PaneLayout::Leaf(0));
        self.layout = remove_from_layout(old_layout, pane_id).expect("removing one of >1 panes always leaves at least one behind");
        if self.focused_pane == pane_id {
            self.focused_pane = first_leaf(&self.layout);
        }
        if self.zoomed == Some(pane_id) || self.panes.len() == 1 {
            self.unzoom();
        }
        Some(pane)
    }

    /// `detach_pane`'s other half: puts a pane in this window beside the
    /// focused one, with a new id, as ids are unique only per window.
    pub(crate) fn attach_pane(&mut self, pane: Pane, horizontal: bool) -> PaneId {
        let id = self.add_pane(pane.stack, horizontal, false);
        self.pane_mut(id).jumps = pane.jumps;
        id
    }

    /// `<C-w>z` / `::bish window zoom`. Returns whether the focused pane is
    /// now zoomed: false after unzooming, or when one pane already fills
    /// the window.
    pub(crate) fn toggle_zoom(&mut self) -> bool {
        self.zoomed = match self.zoomed {
            Some(_) => None,
            None if self.panes.len() > 1 => Some(self.focused_pane),
            None => None,
        };
        self.zoomed.is_some()
    }

    /// Called by everything that moves focus or reshapes the layout
    /// (splitting, focusing a neighbour, balancing, resizing), whose effect
    /// would otherwise happen unseen under the zoomed pane.
    pub(crate) fn unzoom(&mut self) {
        self.zoomed = None;
    }

    /// A new window whose single pane came from another (`window break`),
    /// keeping its frame stack and jump list.
    pub(crate) fn from_pane(id: u32, pane: Pane) -> WindowEntry {
        let mut window = WindowEntry::single(id, pane.stack[0]);
        window.panes[0].stack = pane.stack;
        window.panes[0].jumps = pane.jumps;
        window
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

    // The focused pane's frame stack.
    pub(crate) fn stack(&self) -> &Vec<Frame> {
        &self.focused().stack
    }

    pub(crate) fn stack_mut(&mut self) -> &mut Vec<Frame> {
        let id = self.focused_pane;
        &mut self.pane_mut(id).stack
    }

    // The nearest Session frame at or below the top of the focused pane's
    // stack. A Job is always pushed onto a Session, so one level suffices
    // in practice, but this walks the whole stack.
    pub(crate) fn owning_session(&self) -> SessionId {
        self.focused().owning_session()
    }
}

/// The arrangements `::bish window layout` knows, by tmux's names.
/// `horizontal`/`vertical` there mean the axis panes spread along, the
/// opposite of `PaneLayout::Split`'s `horizontal` (the divider), as with
/// `split`/`vsplit`: the user's word wins on the outside.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum NamedLayout {
    /// Panes in one row, left to right.
    EvenHorizontal,
    /// Panes in one column, top to bottom.
    EvenVertical,
    /// The main pane down the left, the rest stacked to its right.
    MainVertical,
    /// The main pane across the top, the rest in a row beneath it.
    MainHorizontal,
    /// A grid, as square as the count allows.
    Tiled,
}

impl NamedLayout {
    /// The name `::bish window layout` takes, and prints back.
    pub(crate) fn name(self) -> &'static str {
        match self {
            NamedLayout::EvenHorizontal => "even-horizontal",
            NamedLayout::EvenVertical => "even-vertical",
            NamedLayout::MainVertical => "main-vertical",
            NamedLayout::MainHorizontal => "main-horizontal",
            NamedLayout::Tiled => "tiled",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<NamedLayout> {
        NamedLayout::ALL.iter().copied().find(|l| l.name() == name)
    }

    /// In the order `window layout next` cycles them.
    pub(crate) const ALL: [NamedLayout; 5] =
        [NamedLayout::EvenHorizontal, NamedLayout::EvenVertical, NamedLayout::MainHorizontal, NamedLayout::MainVertical, NamedLayout::Tiled];

    /// The one after this in `ALL`, wrapping.
    pub(crate) fn next(self) -> NamedLayout {
        let at = NamedLayout::ALL.iter().position(|l| *l == self).unwrap_or(0);
        NamedLayout::ALL[(at + 1) % NamedLayout::ALL.len()]
    }
}

// One `Split` of `parts` at even weight. A single part comes back bare:
// a split of one isn't a split, so `main-vertical` with two panes gains
// no pointless level.
fn even_split(horizontal: bool, parts: Vec<PaneLayout>) -> PaneLayout {
    match parts.len() {
        1 => parts.into_iter().next().expect("length checked"),
        _ => PaneLayout::Split {
            horizontal,
            children: parts.into_iter().map(|layout| SplitChild { layout, weight: 1.0, fixed: None, minimized: false }).collect(),
        },
    }
}

fn leaf_row(ids: &[PaneId]) -> Vec<PaneLayout> {
    ids.iter().map(|id| PaneLayout::Leaf(*id)).collect()
}

// `apply_layout`'s pure half: the tree a named layout makes of `panes`,
// at least two, led by the main pane.
fn build_named_layout(layout: NamedLayout, panes: &[PaneId]) -> PaneLayout {
    match layout {
        NamedLayout::EvenHorizontal => even_split(false, leaf_row(panes)),
        NamedLayout::EvenVertical => even_split(true, leaf_row(panes)),
        // The main pane gets half; the rest share the other half.
        NamedLayout::MainVertical => even_split(false, vec![PaneLayout::Leaf(panes[0]), even_split(true, leaf_row(&panes[1..]))]),
        NamedLayout::MainHorizontal => even_split(true, vec![PaneLayout::Leaf(panes[0]), even_split(false, leaf_row(&panes[1..]))]),
        NamedLayout::Tiled => {
            // As square as the count allows, rows filled left to right: 4
            // panes make 2x2, 5 a row of 3 over a row of 2.
            let columns = (1..).find(|c| c * c >= panes.len()).unwrap_or(1);
            let rows: Vec<PaneLayout> = panes.chunks(columns).map(|row| even_split(false, leaf_row(row))).collect();
            even_split(true, rows)
        }
    }
}

// Turns `target`'s Leaf into a 2-way Split of it and `new_id`, unless
// `target`'s parent Split already has the same orientation, in which
// case `new_id` joins it as a sibling, so repeated same-direction splits
// divide evenly instead of halving the last pane each time.
fn insert_sibling(layout: PaneLayout, target: PaneId, new_id: PaneId, horizontal: bool, new_fixed: Option<usize>, new_minimized: bool) -> PaneLayout {
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

// Removes `target`'s Leaf, collapsing a Split left with one child into
// that child, so closing one side of a 2-way split leaves a Leaf. The
// survivors keep their weights.
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

// Closes the focused pane of a window with more than one, once
// `WindowAction::Close` has found nothing left under it in the stack.
// Focus goes to the tree's first leaf: predictable, not spatial.
pub(crate) fn close_focused_pane(window: &mut WindowEntry) {
    close_pane(window, window.focused_pane);
}

// Closes any pane of the window (a diagnostics sibling, say, when its
// editor quits; see `run_edit_frame`'s `NavExit::Quit` arm). Focus moves
// only if it was on `pane_id`.
pub(crate) fn close_pane(window: &mut WindowEntry, pane_id: PaneId) {
    let old_layout = std::mem::replace(&mut window.layout, PaneLayout::Leaf(0));
    window.layout = remove_from_layout(old_layout, pane_id).expect("closing one of >1 panes always leaves at least one behind");
    window.panes.retain(|p| p.id != pane_id);
    if window.focused_pane == pane_id {
        window.focused_pane = first_leaf(&window.layout);
    }
    // A zoom on a pane that's gone would hand out a dead id; a zoom on the
    // last pane means nothing. Both end it.
    if window.zoomed == Some(pane_id) || window.panes.len() == 1 {
        window.unzoom();
    }
}

// A rectangle in the content area (above the tab bar), 0-based, made by
// `compute_regions` from a layout. Also what fileeditor draws into.
#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub(crate) row: usize,
    pub(crate) col: usize,
    pub(crate) rows: usize,
    pub(crate) cols: usize,
}

// Floor under a child's weight in the share arithmetic, so a pane
// shrunk all the way (or a bad weight) can't make a share degenerate.
// Sizes are also at least 1 (the `.max(1)` below).
pub(crate) const MIN_PANE_WEIGHT: f64 = 0.05;

// Which children of a split fold behind one divider, as half-open
// ranges.
//
// Each divider costs a row or column, and with many panes the dividers
// are most of a split. Once they take more than `divider_budget` of the
// space, the panes furthest from focus fold into one divider for all of
// them, unfolding as focus moves back.
//
// The first child, the last, the focused one and one either side of it
// stay: the ends keep the split readable, the neighbours make moving
// focus continuous.
//
// A run of one child never folds: a folded run costs a line, like an
// ordinary divider, so folding one child gains nothing. With eight
// panes `a`..`h`, as focus moves:
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
    crate::sort::sort(&mut anchors);
    anchors.dedup();
    anchors.windows(2).map(|pair| pair[0] + 1..pair[1]).filter(|run| run.len() >= 2).collect()
}

// Whether a split's dividers take more of it than allowed. `budget` is
// a percentage; 100 never folds.
fn dividers_overflow(divider_count: usize, axis: usize, budget: usize) -> bool {
    budget < 100 && axis > 0 && divider_count * 100 > axis * budget
}

// Whether a divider is drawn between `children[i]` and `children[i + 1]`:
// every adjacent pair, unless either is minimized, whose one row already
// reads as the boundary (see `SplitChild`). Shared by `split_sizes` and
// `compute_regions`.
fn dividers_after(children: &[SplitChild]) -> Vec<bool> {
    (0..children.len().saturating_sub(1)).map(|i| !(children[i].minimized || children[i + 1].minimized)).collect()
}

// One Split's children as cell counts along its axis, from `usable`
// (the area minus drawn dividers; see `dividers_after`). A minimized
// child takes one cell first; then each `fixed` child its count,
// clamped so no weighted sibling drops below one; the rest divides among
// weighted children by weight, the last weighted child taking the
// rounding remainder so the total is exact.
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

// Walks `layout`, dividing `area` among each Split's children (see
// `split_sizes`) down to each Leaf's rectangle. Divider strips go into
// `dividers` (`row` true for a horizontal line) to be drawn after the
// panes; none between a minimized child and its neighbour (see
// `dividers_after`).
fn compute_regions(layout: &PaneLayout, area: Rect, focused: PaneId, budget: usize, out: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<Divider>) {
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

// `compute_regions`' recursion, carrying the path of child indices from
// the root to the Split being laid out. A divider records that path and
// the child it follows, the only way to name what a drag resizes: a
// `SplitChild` has no identity, and pane ids belong to leaves nested
// anywhere. `split_axis`, the axis length the child's size shares,
// lets a drag turn a position straight into a fraction.
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
            // Folded when the dividers would take more than allowed. Runs
            // are measured from the child holding the focus; a split
            // without it folds around its first child.
            let folded = if dividers_overflow(draws_divider.iter().filter(|d| **d).count(), axis, budget) {
                collapsed_runs(n, child_holding(children, focused).unwrap_or(0))
            } else {
                Vec::new()
            };
            let hidden = |i: usize| folded.iter().any(|r| r.contains(&i));
            // A folded run costs one line, which is the divider that would
            // have followed its last child: drawn instead of that one.
            let divider_count = (0..n.saturating_sub(1)).filter(|i| draws_divider[*i] && !hidden(*i)).count() + folded.len();
            // Only drawn children are sized: a zero-weight child would
            // still get `split_sizes`' minimum. `sizes` is by visible
            // position, mapped back through `visible_index`.
            let visible: Vec<SplitChild> = children
                .iter()
                .enumerate()
                .filter(|(i, _)| !hidden(*i))
                .map(|(_, c)| SplitChild { layout: PaneLayout::Leaf(0), weight: c.weight, fixed: c.fixed, minimized: c.minimized })
                .collect();
            let visible_index = |upto: usize| (0..upto).filter(|i| !hidden(*i)).count();
            if *horizontal {
                // Panes stacked; the divider is the horizontal line between
                // them.
                let usable = area.rows.saturating_sub(divider_count);
                let sizes = split_sizes(&visible, usable);
                let mut row = area.row;
                for (i, child) in children.iter().enumerate() {
                    // A folded child gets no space but is still reported,
                    // at the fold's row with no height, so focusing it
                    // works and unfolds it (see `focus_pane_direction`).
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

// One divider strip, and what a drag needs to resize what it separates;
// see `compute_regions_at`.
#[derive(Clone)]
pub(crate) struct Divider {
    pub(crate) rect: Rect,
    pub(crate) horizontal: bool,
    pub(crate) path: Vec<usize>,
    pub(crate) child: usize,
    // Where the owning Split's area starts on the divider's axis, and how
    // much of it the children divide (minus the divider strips).
    pub(crate) split_start: usize,
    pub(crate) split_axis: usize,
    // How many panes this divider stands for when it's a folded run (see
    // `collapsed_runs`); `None` for an ordinary one. A folded divider shows
    // a count and can't be dragged: there's no single boundary to move.
    pub(crate) folded: Option<usize>,
}

// The first pane hidden behind a folded divider, which a click on it
// focuses. `Divider::child` is the run's last child, so this walks back.
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

// `(prompt_row, pane_bottom_row)` for the completion menu, terminal rows,
// 0-based. The menu draws one row below the prompt, skipped when that's
// past the pane (see `redraw_with_completion_row`). `None` without a
// grid (unpromoted), where the menu draws relative to the prompt with
// `\n` and `\x1b[1A`, which scrolls the terminal and so is kept away
// from panes.
//
// `cursor_row` is the session's grid cursor row, which tracks where the
// next prompt goes without querying the terminal.
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

// `(col_origin, width)`: the terminal column the focused pane's column 0
// is at, and how many columns of the row it owns (see `editor::read_line`).
// `(0, term_cols)` when unpromoted or unsplit.
pub(crate) fn focused_col_origin(window: &WindowEntry, sinks_are_grid: bool, term_rows: usize, term_cols: usize) -> (usize, usize) {
    if !sinks_are_grid || window.panes.len() <= 1 {
        return (0, term_cols);
    }
    let rect = pane_rect(window, window.focused_pane, term_rows, term_cols);
    (rect.col, rect.cols)
}

// The rectangle `pane_id` occupies in `window` at this terminal size:
// `snapshot_window`'s geometry for one pane, without the screens.
pub(crate) fn pane_rect(window: &WindowEntry, pane_id: PaneId, term_rows: usize, term_cols: usize) -> Rect {
    let area = Rect { row: 0, col: 0, rows: content_rows(term_rows), cols: term_cols };
    let mut regions = Vec::new();
    let mut dividers = Vec::new();
    window.regions(area, &mut regions, &mut dividers);
    if let Some((_, rect)) = regions.iter().find(|(id, _)| *id == pane_id) {
        return *rect;
    }
    // Hidden behind a zoom: its rect is what the layout tree says, as its
    // grid stays that size (see `WindowEntry::zoomed`).
    regions.clear();
    compute_regions(&window.layout, area, window.focused_pane, window.divider_budget, &mut regions, &mut dividers);
    regions.into_iter().find(|(id, _)| *id == pane_id).map(|(_, r)| r).expect("pane id always present in its own window's layout")
}

// How much one `window +`/`-` press changes the focused pane's weight;
// changing one weight resizes the split, as shares renormalise (see
// `SplitChild`).
pub(crate) const RESIZE_STEP: f64 = 0.2;

// Walks `path` (see `compute_regions_at`) down to the Split it names.
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

// Gives `children[idx]` the share `fraction` of its split, solving
// weight = f * sum_others / (1 - f). Clamped short of 0 and 1, so a drag
// to the edge leaves both sides usable rather than a one-cell pane.
pub(crate) fn set_child_weight_to_fraction(children: &mut [SplitChild], idx: usize, fraction: f64) {
    let old_weight = children[idx].weight.max(MIN_PANE_WEIGHT);
    let total_weight: f64 = children.iter().map(|c| c.weight.max(MIN_PANE_WEIGHT)).sum();
    let sum_others = (total_weight - old_weight).max(MIN_PANE_WEIGHT);
    let fraction = fraction.clamp(0.05, 0.95);
    children[idx].weight = (fraction * sum_others / (1.0 - fraction)).max(MIN_PANE_WEIGHT);
}

// Drags `divider` to terminal row or column `to`, resizing the child
// before it. Returns whether the layout changed, so a stream of motion
// events repaints only on change. A `fixed` child (the expanded
// diagnostics pane) has `fixed` set directly, as its weight is ignored.
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

// The Split directly containing `target`: its orientation, its children,
// and `target`'s index, for adjusting only `target`'s weight. `None` if
// the window isn't split.
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

// `window +` (delta > 0) and `-` (delta < 0): changes the focused pane's
// weight in its parent split by `RESIZE_STEP`, floored at
// `MIN_PANE_WEIGHT`; with the rendered `.max(1)`, a pane gets small but
// never invisible. A no-op if the window isn't split.
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

    // One list across files: `Ctrl-O` after a cross-file `gd` steps back
    // over the boundary and `Ctrl-I` forward again.
    #[test]
    fn back_and_forward_step_across_files_in_one_history() {
        let mut jumps = JumpList::default();
        jumps.push(at("a.rs", 1));
        jumps.push(at("a.rs", 30));
        assert_eq!(jumps.back(at("b.rs", 5)), Some(at("a.rs", 30)));
        assert_eq!(jumps.back(at("a.rs", 30)), Some(at("a.rs", 1)));
        assert_eq!(jumps.back(at("a.rs", 1)), None, "nothing further back");
        // ...and forward returns the same way.
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
        // A new jump discards the forward history, as in vim.
        jumps.push(at("a.rs", 20));
        assert_eq!(jumps.forward(at("a.rs", 20)), None);
    }

    // A repeat on the same line collapses, as in vim.
    #[test]
    fn repeated_jumps_from_one_line_leave_a_single_entry() {
        let mut jumps = JumpList::default();
        jumps.push(at("a.rs", 5));
        jumps.push(JumpEntry { path: PathBuf::from("a.rs"), row: 5, col: 40 });
        assert_eq!(jumps.back.len(), 1);
        // The same line in another file is another place.
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
            zoomed: None,
            last_layout: None,
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
        // 60 columns minus the divider's one.
        assert_eq!((d[0].split_start, d[0].split_axis), (0, 59));
    }

    #[test]
    fn a_nested_splits_divider_carries_the_path_to_reach_it() {
        // The left half is split top/bottom; its divider must name that inner
        // split, or a drag would resize the wrong boundary.
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
        // The expanded diagnostics pane (see `SplitChild`): its weight is
        // ignored while `fixed` is set.
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
        // No overlap: the divider column is in no pane's rect.
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
        // 10 rows less 1 divider is 9; the fixed child takes 1, the weighted
        // child the other 8.
        assert_eq!(r1.rows, 1);
        assert_eq!(r0.rows, 8);
    }

    #[test]
    fn every_existing_split_still_divides_evenly_with_no_fixed_children() {
        // Three even weighted children in 21 rows (20 after 2 dividers) split
        // 7/7/6, the last taking the remainder.
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
        // 3 rows less 1 divider is 2; a fixed request of 10 is clamped so the
        // weighted sibling keeps a row.
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
        // No divider: the minimized pane's row is the boundary, so 9 rows
        // plus 1 fill the 10, unlike a fixed child, which still gets a
        // divider.
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
        // Ordinary, ordinary, minimized: the first two still get a divider;
        // only the one beside the minimized child is skipped.
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
        // 11 rows less 1 divider and 1 minimized row leaves 9 for the two
        // weighted siblings, split evenly.
        let rows_of = |id: PaneId| out.iter().find(|(i, _)| *i == id).unwrap().1.rows;
        assert_eq!(rows_of(0) + rows_of(1), 9);
    }
}

#[cfg(test)]
mod tab_fold_tests {
    use super::*;

    // Eight panes `a`..`h`, the focus moving right one at a time.
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

    // A folded run costs a line, like a divider, so a lone child isn't
    // folded.
    #[test]
    fn a_run_of_one_is_never_folded() {
        // Focus d (index 3) leaves only `b` before the anchors: kept.
        assert_eq!(collapsed_runs(8, 3), vec![5..7]);
        // ...and focus e leaves only `g` at the other end: kept.
        assert_eq!(collapsed_runs(8, 4), vec![1..3]);
    }

    // Four panes never fold: the ends, the focus and its neighbours are all
    // of them, and any leftover is a run of one.
    #[test]
    fn four_panes_or_fewer_never_fold() {
        for n in 0..=4 {
            for focused in 0..n.max(1) {
                assert!(collapsed_runs(n, focused).is_empty(), "n={n} focused={focused}");
            }
        }
    }

    // Five is the first count that folds, and only from an end; from the
    // middle every child is an anchor or a lone leftover.
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
        // 100 means never.
        assert!(!dividers_overflow(39, 40, 100));
        // ...and nothing divides by zero with no room.
        assert!(!dividers_overflow(7, 0, 25));
    }

    // Folded children get no space, their dividers go, and one divider
    // carrying the count takes their place.
    #[test]
    fn a_folded_run_gives_up_its_rows_to_the_panes_that_are_left() {
        // Ten panes in twenty rows: nine dividers is 45%, over 25%.
        let children: Vec<SplitChild> =
            (0..10).map(|i| SplitChild { layout: PaneLayout::Leaf(i), weight: 1.0, fixed: None, minimized: false }).collect();
        let layout = PaneLayout::Split { horizontal: true, children };
        let area = Rect { row: 0, col: 0, rows: 20, cols: 40 };
        let mut regions = Vec::new();
        let mut dividers = Vec::new();
        compute_regions(&layout, area, 1, 25, &mut regions, &mut dividers);

        // Focus on pane 1, so 0, 1, 2 and 9 keep rows and 3..=8 fold.
        let rows = |id: PaneId| regions.iter().find(|(p, _)| *p == id).unwrap().1.rows;
        for id in [0, 1, 2, 9] {
            assert!(rows(id) > 0, "pane {id} should still be drawn");
        }
        for id in 3..=8 {
            assert_eq!(rows(id), 0, "pane {id} should be folded away");
        }
        // Every pane is still reported, folded or not, so it can be focused.
        assert_eq!(regions.len(), 10);
        // Three ordinary dividers and one fold, not nine.
        assert_eq!(dividers.len(), 4);
        assert_eq!(dividers.iter().filter(|d| d.folded == Some(6)).count(), 1);
        // ...and the rows add up to the area.
        let used: usize = regions.iter().map(|(_, r)| r.rows).sum::<usize>() + dividers.len();
        assert_eq!(used, area.rows);
    }

    // Under the budget nothing folds, however many panes.
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

    // Moving the focus unfolds: a different pane in front hides a
    // different set.
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
        // The focused pane is never hidden.
        for focused in 0..10 {
            assert!(!hidden_for(focused).contains(&focused));
        }
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    const AREA: Rect = Rect { row: 0, col: 0, rows: 24, cols: 80 };

    // Three panes in a row, focus on the middle one.
    fn three_panes() -> WindowEntry {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        let middle = w.add_pane(vec![Frame::Session(0)], false, false);
        w.add_pane(vec![Frame::Session(0)], false, false);
        w.focused_pane = middle;
        w
    }

    fn regions_of(w: &WindowEntry) -> Vec<(PaneId, Rect)> {
        let (mut out, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut out, &mut dividers);
        out
    }

    #[test]
    fn a_zoomed_pane_is_the_only_one_on_screen_and_fills_the_window() {
        let mut w = three_panes();
        assert_eq!(regions_of(&w).len(), 3);

        assert!(w.toggle_zoom());
        let regions = regions_of(&w);
        assert_eq!(regions.len(), 1, "the other two are hidden, not resized");
        assert_eq!(regions[0].0, w.focused_pane);
        assert_eq!(regions[0].1.rows, AREA.rows);
        assert_eq!(regions[0].1.cols, AREA.cols);

        // No dividers: nothing on screen to divide from.
        let (mut out, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut out, &mut dividers);
        assert!(dividers.is_empty());
    }

    #[test]
    fn unzooming_restores_the_layout_exactly() {
        let mut w = three_panes();
        // Resized first, so "restored" means the weights too; an unzoom that
        // rebuilt the layout would even them out.
        resize_focused_pane(&mut w, RESIZE_STEP);
        let before: Vec<(PaneId, usize)> = regions_of(&w).into_iter().map(|(id, r)| (id, r.cols)).collect();

        w.toggle_zoom();
        assert!(!w.toggle_zoom(), "the second press is what puts it back");

        let after: Vec<(PaneId, usize)> = regions_of(&w).into_iter().map(|(id, r)| (id, r.cols)).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn a_hidden_pane_keeps_the_rectangle_it_had() {
        // What its grid stays sized to (see `WindowEntry::zoomed`): resizing
        // reflows scrollback, and an unseen pane shouldn't be reflowed.
        let mut w = three_panes();
        let hidden = w.panes.iter().map(|p| p.id).find(|id| *id != w.focused_pane).expect("three panes, one focused");
        let before = pane_rect(&w, hidden, 26, 80);

        w.toggle_zoom();
        let after = pane_rect(&w, hidden, 26, 80);
        assert_eq!((before.row, before.col, before.rows, before.cols), (after.row, after.col, after.rows, after.cols));
    }

    #[test]
    fn one_pane_has_nothing_to_zoom() {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        assert!(!w.toggle_zoom());
        assert_eq!(regions_of(&w).len(), 1);
    }

    #[test]
    fn closing_the_zoomed_pane_ends_the_zoom() {
        // Otherwise `regions` would hand out a dead pane id, and lookups
        // would panic.
        let mut w = three_panes();
        w.toggle_zoom();
        let zoomed = w.focused_pane;
        close_pane(&mut w, zoomed);
        assert_eq!(w.zoomed, None);
        assert_eq!(regions_of(&w).len(), 2);
    }

    #[test]
    fn closing_down_to_one_pane_ends_the_zoom_too() {
        let mut w = three_panes();
        w.toggle_zoom();
        let others: Vec<PaneId> = w.panes.iter().map(|p| p.id).filter(|id| *id != w.focused_pane).collect();
        for id in others {
            close_pane(&mut w, id);
        }
        assert_eq!(w.zoomed, None, "a zoom on the last pane standing means nothing");
    }

    #[test]
    fn adding_a_pane_puts_the_split_back() {
        // A new pane under a zoom would be invisible, so `add_pane` unzooms;
        // hence it's the only way in.
        let mut w = three_panes();
        w.toggle_zoom();
        w.add_pane(vec![Frame::Session(0)], true, false);
        assert_eq!(w.zoomed, None);
        assert_eq!(regions_of(&w).len(), 4);
    }
}

#[cfg(test)]
mod pane_move_tests {
    use super::*;

    const AREA: Rect = Rect { row: 0, col: 0, rows: 24, cols: 90 };

    // Three panes side by side, focus on the first.
    fn three() -> WindowEntry {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        w.add_pane(vec![Frame::Session(1)], false, false);
        w.add_pane(vec![Frame::Session(2)], false, false);
        w
    }

    // Which pane is in which position, left to right.
    fn order(w: &WindowEntry) -> Vec<PaneId> {
        let (mut out, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut out, &mut dividers);
        out.sort_by_key(|(_, r)| r.col);
        out.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn swapping_moves_the_pane_and_focus_goes_with_it() {
        let mut w = three();
        let before = order(&w);
        let focused = w.focused_pane;

        let other = w.pane_after(focused);
        w.swap_panes(focused, other);

        let after = order(&w);
        assert_eq!(after[0], before[1], "the two traded places");
        assert_eq!(after[1], before[0]);
        assert_eq!(after[2], before[2], "and nothing else moved");
        // Focus stays with the pane that moved, as in vim.
        assert_eq!(w.focused_pane, focused);
    }

    #[test]
    fn swapping_leaves_each_position_the_size_it_had() {
        let mut w = three();
        resize_focused_pane(&mut w, RESIZE_STEP);
        let (mut before, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut before, &mut dividers);
        before.sort_by_key(|(_, r)| r.col);
        let widths: Vec<usize> = before.iter().map(|(_, r)| r.cols).collect();

        let other = w.pane_after(w.focused_pane);
        w.swap_panes(w.focused_pane, other);

        let (mut after, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut after, &mut dividers);
        after.sort_by_key(|(_, r)| r.col);
        assert_eq!(widths, after.iter().map(|(_, r)| r.cols).collect::<Vec<_>>());
    }

    #[test]
    fn pressing_swap_repeatedly_walks_the_pane_around_the_window() {
        // Layout order wraps, so repeated `<C-w>x` moves the pane on.
        let mut w = three();
        let moving = w.focused_pane;
        let mut visited = vec![order(&w).iter().position(|id| *id == moving).unwrap()];
        for _ in 0..3 {
            let other = w.pane_after(moving);
            w.swap_panes(moving, other);
            visited.push(order(&w).iter().position(|id| *id == moving).unwrap());
        }
        assert_eq!(visited, vec![0, 1, 2, 0], "every position, then back to the start");
    }

    #[test]
    fn breaking_a_pane_out_takes_its_whole_stack_with_it() {
        let mut w = three();
        // A frame on top of the session: the stack must survive the move.
        w.pane_mut(1).stack.push(Frame::Job(7));
        let taken = w.detach_pane(1).expect("three panes, so one can go");

        assert_eq!(taken.stack, vec![Frame::Session(1), Frame::Job(7)]);
        assert_eq!(w.panes.len(), 2);
        assert_eq!(order(&w).len(), 2);

        let moved = WindowEntry::from_pane(9, taken);
        assert_eq!(moved.id, 9);
        assert_eq!(moved.panes.len(), 1);
        assert_eq!(moved.stack(), &vec![Frame::Session(1), Frame::Job(7)]);
    }

    #[test]
    fn the_last_pane_cannot_be_broken_out_of_its_own_window() {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        assert!(w.detach_pane(0).is_none());
        assert_eq!(w.panes.len(), 1, "and the refusal leaves it alone");
    }

    #[test]
    fn detaching_the_focused_pane_moves_focus_and_ends_a_zoom() {
        let mut w = three();
        w.focused_pane = 1;
        w.toggle_zoom();
        w.detach_pane(1).expect("three panes");
        assert_ne!(w.focused_pane, 1);
        assert_eq!(w.zoomed, None);
    }

    #[test]
    fn a_joined_pane_gets_an_id_from_the_window_it_lands_in() {
        // Ids are unique only within a window, so the old one means nothing.
        let mut source = three();
        let taken = source.detach_pane(2).expect("three panes");

        let mut target = WindowEntry::single(5, Frame::Session(9));
        let landed = target.attach_pane(taken, true);
        assert_eq!(landed, target.next_pane_id - 1);
        assert_eq!(target.panes.len(), 2);
        assert_eq!(target.pane(landed).stack, vec![Frame::Session(2)]);
    }
}

#[cfg(test)]
mod named_layout_tests {
    use super::*;

    const AREA: Rect = Rect { row: 0, col: 0, rows: 24, cols: 80 };

    fn window_of(n: usize) -> WindowEntry {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        for _ in 1..n {
            w.add_pane(vec![Frame::Session(0)], false, false);
        }
        w.focused_pane = 0;
        w
    }

    fn rects(w: &WindowEntry) -> Vec<(PaneId, Rect)> {
        let (mut out, mut dividers) = (Vec::new(), Vec::new());
        w.regions(AREA, &mut out, &mut dividers);
        out
    }

    // How many distinct rows and columns panes start at: enough to tell one
    // row of four from 2x2 without pinning arithmetic divider budgets
    // change.
    fn shape(w: &WindowEntry) -> (usize, usize) {
        let r = rects(w);
        let mut rows: Vec<usize> = r.iter().map(|(_, rect)| rect.row).collect();
        let mut cols: Vec<usize> = r.iter().map(|(_, rect)| rect.col).collect();
        rows.sort_unstable();
        rows.dedup();
        cols.sort_unstable();
        cols.dedup();
        (rows.len(), cols.len())
    }

    #[test]
    fn even_horizontal_is_one_row_and_even_vertical_is_one_column() {
        let mut w = window_of(4);
        w.apply_layout(NamedLayout::EvenHorizontal);
        assert_eq!(shape(&w), (1, 4), "four panes, one row");

        w.apply_layout(NamedLayout::EvenVertical);
        assert_eq!(shape(&w), (4, 1), "four panes, one column");
    }

    // How many panes are in each row, top to bottom: a grid's shape,
    // without pinning column arithmetic.
    fn rows_of(w: &WindowEntry) -> Vec<usize> {
        let mut by_row: Vec<(usize, usize)> = Vec::new();
        for (_, rect) in rects(w) {
            match by_row.iter_mut().find(|(row, _)| *row == rect.row) {
                Some((_, count)) => *count += 1,
                None => by_row.push((rect.row, 1)),
            }
        }
        by_row.sort_unstable();
        by_row.into_iter().map(|(_, count)| count).collect()
    }

    #[test]
    fn tiled_is_a_grid() {
        let mut w = window_of(4);
        w.apply_layout(NamedLayout::Tiled);
        assert_eq!(rows_of(&w), vec![2, 2]);

        // Rows fill left to right, so an odd count leaves the last row short.
        let mut w = window_of(5);
        w.apply_layout(NamedLayout::Tiled);
        assert_eq!(rows_of(&w), vec![3, 2]);

        let mut w = window_of(9);
        w.apply_layout(NamedLayout::Tiled);
        assert_eq!(rows_of(&w), vec![3, 3, 3]);
    }

    #[test]
    fn main_vertical_gives_the_focused_pane_the_whole_left_side() {
        let mut w = window_of(4);
        let main = w.focused_pane;
        w.apply_layout(NamedLayout::MainVertical);

        let r = rects(&w);
        let main_rect = r.iter().find(|(id, _)| *id == main).unwrap().1;
        assert_eq!(main_rect.row, 0);
        assert_eq!(main_rect.rows, AREA.rows, "full height, whatever else is beside it");
        // And everything else starts to its right.
        assert!(r.iter().filter(|(id, _)| *id != main).all(|(_, rect)| rect.col > main_rect.col));
    }

    #[test]
    fn main_horizontal_gives_it_the_whole_top() {
        let mut w = window_of(3);
        let main = w.focused_pane;
        w.apply_layout(NamedLayout::MainHorizontal);

        let r = rects(&w);
        let main_rect = r.iter().find(|(id, _)| *id == main).unwrap().1;
        assert_eq!(main_rect.col, 0);
        assert_eq!(main_rect.cols, AREA.cols);
        assert!(r.iter().filter(|(id, _)| *id != main).all(|(_, rect)| rect.row > main_rect.row));
    }

    #[test]
    fn main_is_whichever_pane_has_focus() {
        // The main pane is the focused one, so the key that arranges the
        // window says what it's arranged around.
        let mut w = window_of(3);
        w.focused_pane = 2;
        w.apply_layout(NamedLayout::MainVertical);

        let r = rects(&w);
        let main_rect = r.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(main_rect.rows, AREA.rows);
        assert_eq!(main_rect.col, 0);
    }

    #[test]
    fn a_layout_evens_out_whatever_resizing_came_before() {
        let mut w = window_of(3);
        w.apply_layout(NamedLayout::EvenHorizontal);
        let even: Vec<usize> = rects(&w).iter().map(|(_, r)| r.cols).collect();

        resize_focused_pane(&mut w, RESIZE_STEP);
        assert_ne!(rects(&w).iter().map(|(_, r)| r.cols).collect::<Vec<_>>(), even, "the resize took");

        w.apply_layout(NamedLayout::EvenHorizontal);
        assert_eq!(rects(&w).iter().map(|(_, r)| r.cols).collect::<Vec<_>>(), even, "and asking for the layout again undoes it");
    }

    #[test]
    fn next_cycles_through_all_five_and_wraps() {
        let mut w = window_of(2);
        let mut seen = Vec::new();
        for _ in 0..6 {
            let next = w.next_layout();
            w.apply_layout(next);
            seen.push(next.name());
        }
        assert_eq!(seen, vec!["even-horizontal", "even-vertical", "main-horizontal", "main-vertical", "tiled", "even-horizontal"]);
    }

    #[test]
    fn one_pane_has_no_arrangement_to_apply() {
        let mut w = window_of(1);
        w.apply_layout(NamedLayout::Tiled);
        assert_eq!(rects(&w).len(), 1);
    }

    #[test]
    fn every_layout_keeps_every_pane_exactly_once() {
        // A rebuild dropping or duplicating a leaf would leave a pane
        // without a rectangle, which geometry callers panic on.
        for named in NamedLayout::ALL {
            for n in 2..7 {
                let mut w = window_of(n);
                w.apply_layout(named);
                let mut ids: Vec<PaneId> = rects(&w).into_iter().map(|(id, _)| id).collect();
                ids.sort_unstable();
                assert_eq!(ids, (0..n as PaneId).collect::<Vec<_>>(), "{} with {n} panes", named.name());
            }
        }
    }

    #[test]
    fn a_layout_ends_a_zoom() {
        let mut w = window_of(3);
        w.toggle_zoom();
        w.apply_layout(NamedLayout::Tiled);
        assert_eq!(w.zoomed, None, "rearranging under a zoom would be invisible");
    }
}

#[cfg(test)]
mod layout_replay_tests {
    use super::*;

    // Applies `rebuild_commands` to a fresh window as the saved script
    // does when sourced, so this checks the commands themselves.
    fn replay(commands: &[String]) -> WindowEntry {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        for command in commands {
            match command.strip_prefix("::bish window ").expect("every emitted command is a window command") {
                "split" => {
                    let id = w.add_pane(vec![Frame::Session(0)], false, false);
                    w.focused_pane = id;
                }
                "vsplit" => {
                    let id = w.add_pane(vec![Frame::Session(0)], true, false);
                    w.focused_pane = id;
                }
                rest => {
                    let n: usize = rest.strip_prefix("focus ").expect("focus is the only other one").parse().expect("a number");
                    assert!(w.focus_position(n), "focus {n} should name a live pane");
                }
            }
        }
        w
    }

    // A tree's shape with pane ids replaced by layout position: windows
    // built differently match if these do.
    fn shape(w: &WindowEntry) -> String {
        fn go(layout: &PaneLayout, order: &[PaneId], out: &mut String) {
            match layout {
                PaneLayout::Leaf(id) => {
                    out.push_str(&order.iter().position(|other| other == id).expect("a leaf is in the order").to_string());
                }
                PaneLayout::Split { horizontal, children } => {
                    out.push(if *horizontal { 'V' } else { 'H' });
                    out.push('(');
                    for (i, child) in children.iter().enumerate() {
                        if i > 0 {
                            out.push(' ');
                        }
                        go(&child.layout, order, out);
                    }
                    out.push(')');
                }
            }
        }
        let mut out = String::new();
        go(&w.layout, &w.panes_in_layout_order(), &mut out);
        out
    }

    fn assert_round_trips(w: &WindowEntry, expected: &str) {
        assert_eq!(shape(w), expected, "the window under test is not the shape the test says it is");
        assert_eq!(shape(&replay(&w.rebuild_commands())), expected, "replaying {:?} did not rebuild it", w.rebuild_commands());
    }

    #[test]
    fn one_pane_needs_no_commands_at_all() {
        let w = WindowEntry::single(0, Frame::Session(0));
        assert!(w.rebuild_commands().is_empty());
        assert_round_trips(&w, "0");
    }

    #[test]
    fn a_flat_row_replays_in_order() {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        for _ in 0..3 {
            let id = w.add_pane(vec![Frame::Session(0)], false, false);
            w.focused_pane = id;
        }
        assert_round_trips(&w, "H(0 1 2 3)");
    }

    #[test]
    fn a_nested_split_replays_nested() {
        // Split side by side, then stack the right half.
        let mut w = WindowEntry::single(0, Frame::Session(0));
        let right = w.add_pane(vec![Frame::Session(0)], false, false);
        w.focused_pane = right;
        let lower = w.add_pane(vec![Frame::Session(0)], true, false);
        w.focused_pane = lower;
        assert_round_trips(&w, "H(0 V(1 2))");
    }

    #[test]
    fn every_named_layout_replays_exactly() {
        // The arrangements most likely saved, and the most nested.
        for named in NamedLayout::ALL {
            for n in 2..7 {
                let mut w = WindowEntry::single(0, Frame::Session(0));
                for _ in 1..n {
                    let id = w.add_pane(vec![Frame::Session(0)], false, false);
                    w.focused_pane = id;
                }
                w.focused_pane = w.panes_in_layout_order()[0];
                w.apply_layout(named);
                let want = shape(&w);
                assert_eq!(shape(&replay(&w.rebuild_commands())), want, "{} with {n} panes", named.name());
            }
        }
    }

    #[test]
    fn a_deep_alternating_tree_replays() {
        // Split, focus the new half, split the other way, again: what a few
        // minutes of work produces.
        let mut w = WindowEntry::single(0, Frame::Session(0));
        for depth in 0..5 {
            let id = w.add_pane(vec![Frame::Session(0)], depth % 2 == 0, false);
            w.focused_pane = id;
        }
        let want = shape(&w);
        assert_eq!(shape(&replay(&w.rebuild_commands())), want);
    }

    #[test]
    fn focus_counts_panes_from_one_in_layout_order() {
        let mut w = WindowEntry::single(0, Frame::Session(0));
        for _ in 0..2 {
            let id = w.add_pane(vec![Frame::Session(0)], false, false);
            w.focused_pane = id;
        }
        let order = w.panes_in_layout_order();
        assert!(w.focus_position(1));
        assert_eq!(w.focused_pane, order[0]);
        assert_eq!(w.focused_position(), 1);
        assert!(w.focus_position(3));
        assert_eq!(w.focused_pane, order[2]);
        assert_eq!(w.focused_position(), 3);
        assert!(!w.focus_position(4), "past the end is a miss, not a panic");
        assert!(!w.focus_position(0), "and so is counting from zero");
        assert_eq!(w.focused_pane, order[2], "a miss leaves focus where it was");
    }
}
