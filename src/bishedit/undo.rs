// An undo tree: an arena of snapshots in a `Vec`, indexed by position. It
// differs from an undo/redo stack once you undo and then make a new edit:
// the new edit becomes a sibling branch instead of discarding the old
// future. `undo`/`redo` alone can't reach an abandoned branch; vim's
// `g-`/`g+` time travel can (`time_travel_back`/`time_travel_forward`).
//
// Generic over the content `C` (a whole buffer for `TextBuffer`, one line
// for the prompt's vi mode), which is only stored, compared and cloned.
// Each consumer decides when to `checkpoint` and how to restore a
// snapshot (see `TextBuffer::checkpoint_undo`).

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
    // In creation order, which is all `redo` needs to pick a branch.
    children: Vec<usize>,
}

impl<C: Clone + PartialEq> UndoTree<C> {
    // The starting state becomes the root, which `undo` can't go past.
    pub fn new(content: C, cursor: (usize, usize)) -> Self {
        UndoTree { nodes: vec![Node { snapshot: Snapshot { content, cursor }, parent: None, children: Vec::new() }], current: 0 }
    }

    #[cfg(test)]
    pub fn current(&self) -> &Snapshot<C> {
        &self.nodes[self.current].snapshot
    }

    // A stable id for the current node, valid forever since nodes are never
    // removed or reordered: a caller can keep "the node on disk at the last
    // `:w`" and compare later. Opaque; only time travel relies on it being
    // the creation index.
    pub fn current_id(&self) -> usize {
        self.current
    }

    // Commits `content` and `cursor` as a child of the current node, unless
    // `content` equals the current node's. That comparison defines an undo
    // group: a caller checkpoints once per command (see
    // `TextBuffer::checkpoint_undo`), and navigation, or a call right after
    // undo, does nothing. Takes `&C` so the unchanged case never clones.
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

    // Moves to the parent; `None` at the root.
    pub fn undo(&mut self) -> Option<&Snapshot<C>> {
        let parent = self.nodes[self.current].parent?;
        self.current = parent;
        Some(&self.nodes[self.current].snapshot)
    }

    // Moves to the most recently created child. `checkpoint` appends, so
    // right after an `undo` that's the child just left, and `redo` retraces
    // it. A new edit after the `undo` appends a sibling, which becomes the
    // last child; the old branch stays in the tree, reachable only by time
    // travel.
    pub fn redo(&mut self) -> Option<&Snapshot<C>> {
        let &last = self.nodes[self.current].children.last()?;
        self.current = last;
        Some(&self.nodes[self.current].snapshot)
    }

    // `g-`/`g+`: to the node with the next lower or higher creation index,
    // on any branch. vim numbers changes in that order, and since nodes are
    // only ever appended, the index is that number. Walking creation order
    // rather than edges is what reaches branches `undo`/`redo` can't.
    pub fn time_travel_back(&mut self) -> Option<&Snapshot<C>> {
        let prev = self.current.checked_sub(1)?;
        self.current = prev;
        Some(&self.nodes[self.current].snapshot)
    }

    pub fn time_travel_forward(&mut self) -> Option<&Snapshot<C>> {
        if self.current + 1 >= self.nodes.len() {
            return None;
        }
        self.current += 1;
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

        // Redo from the root follows B, the newest branch; A is still in the
        // tree but out of undo/redo's reach.
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

    #[test]
    fn time_travel_reaches_a_branch_undo_redo_alone_cannot() {
        let mut t = UndoTree::new(0, (0, 0));
        t.checkpoint(&1, (0, 0)); // root(0) -> A(1)
        t.undo();
        t.checkpoint(&2, (0, 0)); // root(0) -> B(2), a sibling of A
        // At B, a leaf: `redo` has nothing, and `undo` reaches only the root.
        assert!(t.redo().is_none());

        // Time travel walks creation order: B (index 2) to A (index 1).
        let back = t.time_travel_back().unwrap();
        assert_eq!(back.content, 1);

        // And forward again to B.
        let fwd = t.time_travel_forward().unwrap();
        assert_eq!(fwd.content, 2);
    }

    #[test]
    fn time_travel_back_past_the_root_returns_none() {
        let mut t = UndoTree::new(0, (0, 0));
        assert!(t.time_travel_back().is_none());
        t.checkpoint(&1, (0, 0));
        assert_eq!(t.time_travel_back().unwrap().content, 0);
        assert!(t.time_travel_back().is_none());
    }

    #[test]
    fn time_travel_forward_past_the_newest_node_returns_none() {
        let mut t = UndoTree::new(0, (0, 0));
        t.checkpoint(&1, (0, 0));
        assert!(t.time_travel_forward().is_none());
    }
}
