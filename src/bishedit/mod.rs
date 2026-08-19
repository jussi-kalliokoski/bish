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

    /// Horizontal counterpart to `viewport_top`/`viewport_height`: how
    /// many columns of the current line are scrolled off to the left of
    /// whatever width the caller last rendered at. Defaults to always `0`
    /// (an implementor that never overrides `set_viewport_left` just
    /// keeps rendering from column 0 forever) -- correct for `ScreenBuffer`
    /// (`repl.rs`), whose content is already fixed-width, pre-wrapped
    /// terminal output with nothing to scroll sideways *into*; the one
    /// implementor that needs the real thing is `TextBuffer`, where a
    /// single logical line can run arbitrarily wider than the pane (a
    /// long URL, an unwrapped log line, ...) with nothing else to reflow
    /// it. Kept in sync by `fileeditor::scroll_to_show_cursor`/`repl.rs`'s
    /// own copy for `NavBuffer`, exactly the way `viewport_top` already
    /// is -- see those for why the width to scroll by is passed in fresh
    /// each call rather than also being trait state here: `viewport_
    /// height` is set once at construction and never resynced on a later
    /// resize (`TextBuffer::vheight`/`ScreenBuffer::vheight` are both
    /// plain fields, no setter exists for either), which is close enough
    /// for a scroll-trigger heuristic on rows that rarely change; column
    /// width changes on ordinary vsplit/window resizes far more often, so
    /// this axis doesn't repeat that shortcut.
    fn viewport_left(&self) -> usize {
        0
    }
    fn set_viewport_left(&mut self, _col: usize) {}

    /// `m{a-z}` / `` `{mark} `` / `'{mark}`. Marks are per-buffer state, same
    /// as the cursor and viewport -- every implementor owns its own storage.
    fn set_mark(&mut self, name: char, pos: (usize, usize));
    fn get_mark(&self, name: char) -> Option<(usize, usize)>;

    /// Whether `line`'s content is a soft-wrap continuation into
    /// `line + 1` -- both are still separate storage lines, but joined
    /// with no newline in between when text spanning them gets extracted
    /// (`motion::extract_text`/`whole_lines`), and treated as one line for
    /// `$`'s own end-of-line target. Default false: every implementor
    /// whose storage lines are real logical lines (a real newline really
    /// did end each one) never needs to override this. The one exception
    /// is `repl.rs`'s `ScreenBuffer`, the sole `Buffer` backed by a
    /// fixed-width terminal grid, where a long enough line can get cut by
    /// autowrap rather than ending on purpose -- without this, yanking
    /// across that cut spliced in a newline that was never actually in
    /// the source bytes, garbling anything long enough to wrap (a long
    /// URL, say) and making it impossible to select in one motion.
    fn line_wraps(&self, _line: usize) -> bool {
        false
    }
}
