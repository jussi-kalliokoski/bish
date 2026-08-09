// The first slice of bish-edit's headless core (see plan.md): buffers,
// cursors, and vim's normal-mode motion set, built ahead of its consumer.
// Wired into repl.rs as of the shell-integration stage. #![allow(dead_code)]
// stays regardless -- this crate isn't the only intended consumer long-term.
#![allow(dead_code)]

pub mod motion;

/// The first slice of bish-edit's headless core: read-only line/cursor
/// accessors that motions operate over. Mutation (insert/delete) is a later
/// addition to this same trait, once real editing lands.
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
}
