// A small, content-agnostic undo tree: an arena of snapshots (a plain
// `Vec`, indexed by position -- no `Rc<RefCell<_>>` graph, same pattern
// this codebase already uses for pane/window relationships) rather than a
// linear undo/redo stack. The distinction only matters once you undo and
// then make a *new* edit: a stack would silently discard whatever "future"
// existed past the point you undid to; a tree instead appends the new
// edit as a sibling branch, so nothing is ever lost even if `undo`/`redo`
// alone can't reach it anymore (see `redo`'s own doc comment for exactly
// what `undo`/`redo` alone *can* reach -- reaching an abandoned sibling
// branch needs real vim's own g-/g+ time travel, not built here yet).
//
// Deliberately generic over the snapshot's own content type `C` (a whole
// multi-line buffer for `TextBuffer`, a single line for the live prompt's
// own vi-mode) -- this module only ever needs to store/compare/clone `C`
// wholesale, never look inside it, so there's nothing buffer-specific to
// leak in here. Each consumer defines its own thin wrapper (see
// `TextBuffer::checkpoint_undo`/`undo`/`redo`) that decides *when* to call
// `checkpoint` and how to splice a restored snapshot back into its own
// buffer/cursor fields.

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot<C> {
    pub content: C,
    pub cursor: (usize, usize),
}

pub struct UndoTree<C> {
    nodes: Vec<Node<C>>,
    current: usize,
}

struct Node<C> {
    snapshot: Snapshot<C>,
    parent: Option<usize>,
    // Creation order -- `redo`'s own doc comment is why that's enough to
    // know which branch it should follow, with no separate "preferred
    // child" bookkeeping needed.
    children: Vec<usize>,
}

impl<C: Clone + PartialEq> UndoTree<C> {
    // `content`/`cursor`: the buffer's own state at the moment this tree
    // is created -- e.g. a freshly opened file's content, before any edit
    // -- becomes the root node. `undo()` can never go past this.
    pub fn new(content: C, cursor: (usize, usize)) -> Self {
        UndoTree { nodes: vec![Node { snapshot: Snapshot { content, cursor }, parent: None, children: Vec::new() }], current: 0 }
    }

    pub fn current(&self) -> &Snapshot<C> {
        &self.nodes[self.current].snapshot
    }

    // Commits `content`/`cursor` as a new child of the current node --
    // unless `content` already equals what's already there, in which case
    // this is a no-op. That comparison, not an explicit begin/end bracket
    // around each command, is what defines a "group" here: a caller calls
    // this once per logical command (see TextBuffer::checkpoint_undo's own
    // doc comment for where that call actually lands), and it silently
    // does nothing on a pure-navigation keystroke that never touched the
    // buffer, or on a second call after undo/redo already made `content`
    // match the node it just moved to. Takes `&C` specifically so the
    // common (unchanged) case never has to clone `content` at all.
    pub fn checkpoint(&mut self, content: &C, cursor: (usize, usize)) {
        if *content == self.nodes[self.current].snapshot.content {
            return;
        }
        let node = Node { snapshot: Snapshot { content: content.clone(), cursor }, parent: Some(self.current), children: Vec::new() };
        let new_idx = self.nodes.len();
        self.nodes[self.current].children.push(new_idx);
        self.nodes.push(node);
        self.current = new_idx;
    }

    // Moves to the parent, if any -- `None` (and no movement) at the root.
    pub fn undo(&mut self) -> Option<&Snapshot<C>> {
        let parent = self.nodes[self.current].parent?;
        self.current = parent;
        Some(&self.nodes[self.current].snapshot)
    }

    // Moves to `children.last()` -- the most recently *created* child of
    // the current node. `checkpoint` always appends, so this is exactly
    // "whichever branch was diverged into most recently from here," which
    // is what makes plain `redo` retrace an `undo` you just did: `undo`
    // moved to a parent that already had this child; nothing has been
    // committed since, so it's still the last one. If you instead make a
    // *new* edit right after that `undo`, `checkpoint` appends a sibling
    // after the old child, and that new one becomes `children.last()`
    // instead -- the old branch is still in the tree (nothing was
    // discarded), just no longer reachable via `undo`/`redo` alone.
    pub fn redo(&mut self) -> Option<&Snapshot<C>> {
        let &last = self.nodes[self.current].children.last()?;
        self.current = last;
        Some(&self.nodes[self.current].snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_is_a_noop_when_content_is_unchanged() {
        let mut t = UndoTree::new("a".to_string(), (0, 0));
        t.checkpoint(&"a".to_string(), (0, 0));
        assert!(t.undo().is_none());
    }

    #[test]
    fn checkpoint_commits_a_child_and_undo_redo_round_trip() {
        let mut t = UndoTree::new("a".to_string(), (0, 0));
        t.checkpoint(&"ab".to_string(), (0, 1));
        assert_eq!(t.current().content, "ab");

        let undone = t.undo().unwrap();
        assert_eq!(undone.content, "a");
        assert_eq!(undone.cursor, (0, 0));

        let redone = t.redo().unwrap();
        assert_eq!(redone.content, "ab");
        assert_eq!(redone.cursor, (0, 1));
    }

    #[test]
    fn undo_past_the_root_returns_none_and_stays_put() {
        let mut t = UndoTree::new(1, (0, 0));
        assert!(t.undo().is_none());
        assert_eq!(t.current().content, 1);
    }

    #[test]
    fn redo_with_no_children_returns_none() {
        let mut t = UndoTree::new(1, (0, 0));
        assert!(t.redo().is_none());
        t.checkpoint(&2, (0, 0));
        assert!(t.redo().is_none()); // at the tip, nothing further to redo
    }

    #[test]
    fn a_new_edit_after_undo_branches_instead_of_overwriting() {
        let mut t = UndoTree::new(0, (0, 0));
        t.checkpoint(&1, (0, 0)); // root -> A(1)
        t.undo(); // back to root
        t.checkpoint(&2, (0, 0)); // root -> B(2), a sibling of A

        // redo from root now follows B, the most recently created branch
        // -- A still exists in the tree (nothing was discarded), it's
        // just unreachable via undo/redo alone.
        t.undo();
        assert_eq!(t.current().content, 0);
        let redone = t.redo().unwrap();
        assert_eq!(redone.content, 2);
        assert_eq!(t.nodes.len(), 3); // root, A, B -- A is still there
    }

    #[test]
    fn multiple_checkpoints_build_a_linear_chain_undoable_one_at_a_time() {
        let mut t = UndoTree::new(0, (0, 0));
        t.checkpoint(&1, (0, 0));
        t.checkpoint(&2, (0, 0));
        t.checkpoint(&3, (0, 0));
        assert_eq!(t.current().content, 3);
        assert_eq!(t.undo().unwrap().content, 2);
        assert_eq!(t.undo().unwrap().content, 1);
        assert_eq!(t.undo().unwrap().content, 0);
        assert!(t.undo().is_none());
    }
}
