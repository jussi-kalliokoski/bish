// bish-edit's headless core: buffers, motions, highlighting, linting and
// the rest of what the editor and the line editor share.
#![allow(dead_code)]

pub mod codehighlight;
pub mod completion;
pub mod fold;
pub mod format;
pub mod fuzzy;
pub mod grapheme;
pub mod highlight;
pub mod lint;
pub mod manpages;
pub mod motion;
pub mod registers;
pub mod snippet;
pub mod suggestion;
pub mod tabular;
pub mod textbuffer;
pub mod undo;
pub mod unicode_tables;
pub mod unicode_width;
pub mod vimkeys;
pub mod wrap;

/// vim's `ignorecase` and `smartcase`, resolved for one pattern.
/// `smartcase` only narrows: with `ignorecase` on, an uppercase letter in
/// the pattern makes that search case-sensitive.
pub fn ignore_case_for(pattern: &str, ignorecase: bool, smartcase: bool) -> bool {
    ignorecase && !(smartcase && pattern.chars().any(char::is_uppercase))
}

/// Read-only line and cursor access, which motions work over. Each
/// concrete buffer type does its own editing.
pub trait Buffer {
    fn line_count(&self) -> usize;
    fn line_len(&self, line: usize) -> usize;
    fn char_at(&self, line: usize, col: usize) -> Option<char>;

    fn line_chars(&self, line: usize) -> Vec<char> {
        (0..self.line_len(line)).filter_map(|c| self.char_at(line, c)).collect()
    }

    fn cursor(&self) -> (usize, usize);
    fn set_cursor(&mut self, line: usize, col: usize);

    /// Whether a search for `pattern` in this buffer folds case. Asked of
    /// the buffer because a motion is handed only a pattern, and the
    /// renderer's highlight pass even less. Default false, vim's default,
    /// for implementors with no settings (a test double, the hex view).
    fn search_ignore_case(&self, _pattern: &str) -> bool {
        false
    }

    /// Which characters besides letters and digits are part of a word for
    /// `w`/`b`/`e`/`ge`, `iw`/`aw`, `*`/`#` and `Ctrl-W`: vim's `iskeyword`.
    /// A plain set rather than vim's ranges-and-classes spec, since
    /// `char::is_alphanumeric` already covers every script's letters: `-` for
    /// kebab-case, `$` for shell variables. Default `_`.
    fn word_chars(&self) -> &str {
        "_"
    }

    fn viewport_top(&self) -> usize;
    fn set_viewport_top(&mut self, line: usize);
    fn viewport_height(&self) -> usize;
    /// Resyncs the height from what the caller is about to render into.
    /// Every real view overrides it, since a pane's height changes with the
    /// terminal. The default does nothing, for a test double.
    fn set_viewport_height(&mut self, _rows: usize) {}

    /// How many columns of the current line are scrolled off to the left.
    /// Default 0, right for `ScreenBuffer` (repl.rs), whose content is
    /// pre-wrapped terminal output. `TextBuffer` needs it, since one line
    /// can be far wider than the pane. Kept in sync by
    /// `fileeditor::scroll_to_show_cursor` and repl.rs's copy for
    /// `NavBuffer`, which pass the width in on each call.
    fn viewport_left(&self) -> usize {
        0
    }
    fn set_viewport_left(&mut self, _col: usize) {}

    /// `m{a-z}`, `` `{mark} ``, `'{mark}`. Marks are per buffer, like the
    /// cursor.
    fn set_mark(&mut self, name: char, pos: (usize, usize));
    fn get_mark(&self, name: char) -> Option<(usize, usize)>;

    /// Whether `line` soft-wraps into `line + 1`: separate storage lines,
    /// joined without a newline when text is extracted
    /// (`motion::extract_text`, `whole_lines`) and one line for `$`. Only
    /// `ScreenBuffer` (repl.rs) overrides it: a terminal grid cuts long lines
    /// by autowrap, and yanking across the cut mustn't add a newline that was
    /// never in the output.
    fn line_wraps(&self, _line: usize) -> bool {
        false
    }

    /// This buffer's folds, if it has any (the file editor's), so `j` can
    /// step over a closed fold as one line.
    fn folds(&self) -> Option<&fold::Folds> {
        None
    }
    fn folds_mut(&mut self) -> Option<&mut fold::Folds> {
        None
    }

    /// The lines drawn as `line`'s row: just `line`, unless a closed fold
    /// hides it.
    fn fold_span(&self, line: usize) -> (usize, usize) {
        self.folds().map_or((line, line), |f| f.span(line))
    }
}

#[cfg(test)]
mod tests {
    use super::ignore_case_for;

    // `smartcase` only narrows, and only with `ignorecase` on.
    #[test]
    fn smartcase_narrows_ignorecase_and_means_nothing_without_it() {
        assert!(!ignore_case_for("beta", false, false));
        assert!(ignore_case_for("beta", true, false));
        assert!(ignore_case_for("BETA", true, false), "without smartcase, case in the pattern is irrelevant");

        assert!(ignore_case_for("beta", true, true), "all lowercase, so still folded");
        assert!(!ignore_case_for("Beta", true, true), "one uppercase letter is enough to narrow it");
        assert!(!ignore_case_for("BETA", true, true));

        assert!(!ignore_case_for("beta", false, true), "smartcase alone does nothing at all");
        assert!(!ignore_case_for("BETA", false, true));

        assert!(ignore_case_for("2 + 2", true, true));
        assert!(ignore_case_for("", true, true));
    }
}
