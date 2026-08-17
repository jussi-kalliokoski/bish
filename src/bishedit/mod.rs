// The first slice of bish-edit's headless core (see plan.md): buffers,
// cursors, and vim's normal-mode motion set, built ahead of its consumer.
// Wired into repl.rs as of the shell-integration stage. #![allow(dead_code)]
// stays regardless -- this crate isn't the only intended consumer long-term.
#![allow(dead_code)]

pub mod completion;
pub mod fuzzy;
pub mod highlight;
pub mod lint;
pub mod manpages;
pub mod motion;
pub mod registers;
pub mod suggestion;
pub mod textbuffer;
pub mod undo;
pub mod vimkeys;

/// The first slice of bish-edit's headless core: read-only line/cursor
/// accessors that motions operate over. Real editing landed as
/// `textbuffer::TextBuffer` without extending this trait: mutation has
/// never gone through it, even for `editor.rs`'s own single-line
/// `LineBuffer` (which already reaches straight into its own `Vec<char>`)
/// -- keeping it bespoke per concrete type stayed the simpler, established
/// shape once it actually came time to add it, not a shortcut taken here.
pub trait Buffer {
    fn line_count(&self) -> usize;
    fn line_len(&self, line: usize) -> usize;
    fn char_at(&self, line: usize, col: usize) -> Option<char>;

    fn line_chars(&self, line: usize) -> Vec<char> {
        (0..self.line_len(line))
            .filter_map(|c| self.char_at(line, c))
            .collect()
    }

    fn cursor(&self) -> (usize, usize);
    fn set_cursor(&mut self, line: usize, col: usize);

    fn viewport_top(&self) -> usize;
    fn set_viewport_top(&mut self, line: usize);
    fn viewport_height(&self) -> usize;

    /// `m{a-z}` / `` `{mark} `` / `'{mark}`. Marks are per-buffer state, same
    /// as the cursor and viewport -- every implementor owns its own storage.
    fn set_mark(&mut self, name: char, pos: (usize, usize));
    fn get_mark(&self, name: char) -> Option<(usize, usize)>;
}
