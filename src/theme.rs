// The colours of bish's own chrome -- the file browser's entry types,
// the editor's gutter, pane dividers, rendered markdown, diagnostics.
//
// Deliberately the same shape `bishedit::highlight`'s `syn_col_*`
// machinery already has, one axis over: a table naming each element's
// bishopt, a `default_style` giving what it looks like with nothing
// set, and a resolved map a caller with a live `Shell` builds once per
// redraw. Two parallel systems that worked differently would be two
// things to learn; this way `ui_col_directory` behaves exactly as
// `syn_col_keyword` does, and both land in a `::bish theme` declaration
// without either knowing about themes at all.
//
// **Only the foreground is themeable**, again matching `resolve_style`:
// bishopt's `Color` type has no way to express weight, so the bold on a
// directory and the underline on a link stay what they are. That is
// also the right call on its own merits -- a link that stopped being
// underlined because someone picked a colour would be a worse link.
#![allow(dead_code)]

use crate::vt100;
use std::collections::HashMap;

/// One piece of bish's own interface whose colour a theme can set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ui {
    /// The file browser's entry types.
    Directory,
    Symlink,
    Archive,
    Executable,
    /// The editor's line-number gutter.
    LineNumber,
    /// The lines between panes.
    Divider,
    /// Rendered markdown -- `:help` and `:preview`.
    Heading,
    Code,
    Link,
    Quote,
    /// Diagnostics, in the gutter and under the text. One per
    /// `lint::Severity` variant, and `diagnostic_style` (fileeditor.rs)
    /// matches exhaustively between the two, so the day a severity is
    /// added is the day this list has to grow with it.
    Error,
    Warning,
    Info,
    Hint,
}

/// Which bishopt drives each one. Lives here rather than in `exec.rs`
/// for the same reason `SYN_COL_OPTIONS` does: `Ui` is this module's own
/// type, and the options table has no reason to depend on it.
///
/// Deliberately not every `Ui`. `LineNumber` and `Divider` are drawn in
/// the terminal's *own* foreground -- dimmed, and plain, respectively --
/// and bishopt's `Color` type can only ever produce a concrete colour:
/// there is no "inherit whatever the terminal uses" value to register as
/// their default, so registering one at all would change how a fresh
/// install looks. The exact call `SYN_COL_OPTIONS` already makes for
/// `Flag`/`Subcommand`/`Link`, and they become one line each the day
/// that type grows a way to say it.
pub const UI_COL_OPTIONS: &[(Ui, &str)] = &[
    (Ui::Directory, "ui_col_directory"),
    (Ui::Symlink, "ui_col_symlink"),
    (Ui::Archive, "ui_col_archive"),
    (Ui::Executable, "ui_col_executable"),
    (Ui::Heading, "ui_col_heading"),
    (Ui::Code, "ui_col_code"),
    (Ui::Link, "ui_col_link"),
    (Ui::Quote, "ui_col_quote"),
    (Ui::Error, "ui_col_error"),
    (Ui::Warning, "ui_col_warning"),
    (Ui::Info, "ui_col_info"),
    (Ui::Hint, "ui_col_hint"),
];

/// What each element looks like with nothing set -- exactly what the
/// hardcoded escape sequences these replaced already produced, so a
/// fresh install renders identically to before this existed.
pub fn default_style(element: Ui) -> (vt100::Color, vt100::CellAttrs) {
    let plain = vt100::CellAttrs::default();
    let bold = vt100::CellAttrs { bold: true, ..plain };
    let dim = vt100::CellAttrs { dim: true, ..plain };
    let underline = vt100::CellAttrs { underline: true, ..plain };
    match element {
        Ui::Directory => (vt100::Color::Indexed(4), bold),
        Ui::Symlink => (vt100::Color::Indexed(6), plain),
        Ui::Archive => (vt100::Color::Indexed(5), plain),
        Ui::Executable => (vt100::Color::Indexed(2), plain),
        // No colour of its own, just dimmed: a gutter that competed with
        // the text for attention would be the wrong way round.
        Ui::LineNumber => (vt100::Color::Default, dim),
        Ui::Divider => (vt100::Color::Default, plain),
        Ui::Heading => (vt100::Color::Indexed(3), bold),
        Ui::Code => (vt100::Color::Indexed(2), plain),
        Ui::Link => (vt100::Color::Indexed(6), underline),
        Ui::Quote => (vt100::Color::Indexed(4), dim),
        Ui::Error => (vt100::Color::Indexed(1), underline),
        Ui::Warning => (vt100::Color::Indexed(3), underline),
        // Underlined like the two above -- a finding is a finding, and
        // the colour is what says how much it matters. Blue and cyan
        // read as "informational" against red/yellow without competing
        // with them for attention, which is the whole point of a
        // severity below Warning.
        Ui::Info => (vt100::Color::Indexed(4), underline),
        Ui::Hint => (vt100::Color::Indexed(6), underline),
    }
}

/// A session's resolved UI colours, built once per redraw by a caller
/// that has a live `Shell` to read the options from. Empty (or `None`)
/// behaves exactly like calling `default_style` directly.
pub type UiColors = HashMap<Ui, vt100::Color>;

pub fn resolve(element: Ui, colors: Option<&UiColors>) -> (vt100::Color, vt100::CellAttrs) {
    let (default_fg, attrs) = default_style(element);
    let fg = colors.and_then(|c| c.get(&element)).copied().unwrap_or(default_fg);
    (fg, attrs)
}

/// The SGR sequence one element is drawn with -- what the call sites
/// that write escapes into a string directly need, as against the ones
/// that build `vt100::Cell`s.
pub fn sgr(element: Ui, colors: Option<&UiColors>) -> String {
    let (fg, attrs) = resolve(element, colors);
    vt100::sgr_codes(fg, vt100::Color::Default, attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every element has an option, and every option is a real one -- the
    // list is the interface, and a name that only exists on one side is
    // a colour nobody can set or an option that sets nothing.
    #[test]
    fn every_element_has_exactly_one_option() {
        let mut names: Vec<&str> = UI_COL_OPTIONS.iter().map(|(_, name)| *name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "two elements share an option name");
        for (element, name) in UI_COL_OPTIONS {
            assert!(name.starts_with("ui_col_"), "{name} doesn't look like one of these");
            assert_eq!(UI_COL_OPTIONS.iter().filter(|(e, _)| e == element).count(), 1, "{element:?} listed twice");
        }
    }

    #[test]
    fn an_override_replaces_the_colour_and_leaves_the_weight_alone() {
        let (default_fg, default_attrs) = default_style(Ui::Directory);
        assert!(default_attrs.bold, "a directory is bold to begin with");
        let mut colors = UiColors::new();
        colors.insert(Ui::Directory, vt100::Color::Indexed(5));
        let (fg, attrs) = resolve(Ui::Directory, Some(&colors));
        assert_eq!(fg, vt100::Color::Indexed(5));
        assert_eq!(attrs, default_attrs, "still bold: a colour can't express weight");
        assert_ne!(fg, default_fg);
    }

    // The two that are drawn in the terminal's own foreground, and why.
    #[test]
    fn the_elements_with_no_colour_of_their_own_have_no_option() {
        for element in [Ui::LineNumber, Ui::Divider] {
            assert_eq!(default_style(element).0, vt100::Color::Default);
            assert!(!UI_COL_OPTIONS.iter().any(|(e, _)| *e == element), "{element:?} has no colour to register a default for");
        }
    }

    #[test]
    fn no_overrides_is_the_default_style() {
        for (element, _) in UI_COL_OPTIONS {
            assert_eq!(resolve(*element, None), default_style(*element));
            assert_eq!(resolve(*element, Some(&UiColors::new())), default_style(*element));
        }
    }

    // What the hardcoded sequences these replaced already produced.
    #[test]
    fn the_defaults_are_what_was_drawn_before() {
        assert_eq!(sgr(Ui::Directory, None), "\x1b[0;1;34m");
        assert_eq!(sgr(Ui::Symlink, None), "\x1b[0;36m");
        assert_eq!(sgr(Ui::Archive, None), "\x1b[0;35m");
        assert_eq!(sgr(Ui::Executable, None), "\x1b[0;32m");
    }
}
