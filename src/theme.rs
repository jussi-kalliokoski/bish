// The colours of bish's own chrome (the file browser's entry types, the
// editor's gutter, pane dividers, rendered markdown, diagnostics), and
// at the bottom, the themes bish ships.
//
// Shaped like `bishedit::highlight`'s colours: a table naming each
// element's bishopt, a `default_style` for when nothing is set, and a
// resolved map a caller builds once per redraw. So `ui_col_directory`
// behaves as `::bish hl` names do, and both fit in a `::bish theme`
// declaration.
//
// Only the foreground is themeable, as in `resolve_style`: bishopt's
// `Color` can't express weight, so a directory's bold and a link's
// underline stay.
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
    /// Rendered markdown: `:help` and `:preview`.
    Heading,
    Code,
    Link,
    Quote,
    /// Diagnostics, in the gutter and under the text. One per
    /// `lint::Severity`, matched exhaustively by `diagnostic_style`
    /// (fileeditor.rs), so a new severity has to be added here too.
    Error,
    Warning,
    Info,
    Hint,
}

/// Which bishopt drives each. Here rather than in `exec.rs`, as with
/// `SYN_COL_OPTIONS`: `Ui` is this module's type.
///
/// Not every `Ui`: `LineNumber` and `Divider` use the terminal's own
/// foreground (dimmed, and plain), and a `Color` option can only hold a
/// concrete colour, so registering them would change a fresh install's
/// look. As `SYN_COL_OPTIONS` does for `Flag`, `Subcommand` and `Link`.
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

/// What each element looks like with nothing set.
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
        // Only dimmed: a gutter shouldn't compete with the text.
        Ui::LineNumber => (vt100::Color::Default, dim),
        Ui::Divider => (vt100::Color::Default, plain),
        Ui::Heading => (vt100::Color::Indexed(3), bold),
        Ui::Code => (vt100::Color::Indexed(2), plain),
        Ui::Link => (vt100::Color::Indexed(6), underline),
        Ui::Quote => (vt100::Color::Indexed(4), dim),
        Ui::Error => (vt100::Color::Indexed(1), underline),
        Ui::Warning => (vt100::Color::Indexed(3), underline),
        // Underlined like the two above; the colour says how much it matters.
        // Blue and cyan read as informational beside red and yellow.
        Ui::Info => (vt100::Color::Indexed(4), underline),
        Ui::Hint => (vt100::Color::Indexed(6), underline),
    }
}

/// A session's resolved UI colours, built once per redraw by a caller
/// with a `Shell`. Empty or `None` behaves as `default_style`.
pub type UiColors = HashMap<Ui, vt100::Color>;

pub fn resolve(element: Ui, colors: Option<&UiColors>) -> (vt100::Color, vt100::CellAttrs) {
    let (default_fg, attrs) = default_style(element);
    let fg = colors.and_then(|c| c.get(&element)).copied().unwrap_or(default_fg);
    (fg, attrs)
}

/// The SGR sequence one element is drawn with, for call sites writing
/// escapes into a string rather than building `vt100::Cell`s.
pub fn sgr(element: Ui, colors: Option<&UiColors>) -> String {
    let (fg, attrs) = resolve(element, colors);
    vt100::sgr_codes(fg, vt100::Color::Default, attrs)
}

// ---------------------------------------------------------------------
// The themes bish ships
// ---------------------------------------------------------------------

/// A theme bish ships, activated by name like a declared one: `bishopt
/// --set theme kaamos`.
///
/// The two tables `::bish theme begin`/`end` would capture (`::bish hl`
/// names and `ui_col_*` bishopts), as static strings parsed only when
/// asked. A declared theme of the same name wins per name, not wholesale
/// (see `Shell::bishopt_value`), so overriding one colour is one line.
pub struct Builtin {
    pub name: &'static str,
    /// One line for `::bish theme list`.
    pub about: &'static str,
    /// `::bish hl` names: what the highlighter paints.
    pub hl: &'static [(&'static str, &'static str)],
    /// bishopt names, all `Color` options: bish's own chrome.
    pub opts: &'static [(&'static str, &'static str)],
}

/// Two skies, from the Talvitaivas code scheme.
///
/// The scheme is nine hue families at three volumes: the hue says what
/// kind of thing a word is, the lightness how loud. Magenta is the
/// language talking, violet a declaration, blue a name you can jump to,
/// cyan prose about the code, green text, yellow data, orange a type, red
/// a value that can't change, pink something fastened to the code. A
/// variant may move a colour but never a meaning, so the two skies are
/// the same scheme against night and day.
///
/// Neither sets a background, as bish can't: `revontulet` wants a
/// terminal at its midnight blue (`#010719`) or near it, `kaamos` a light
/// one (`#C7D6FE`); its ink is dark, unreadable on black. That's a
/// terminal profile to pick.
///
/// Each colour has an ANSI fallback from the scheme's own mapping onto
/// the sixteen slots, the loudest step of each family being the bright
/// slot. It assumes the scheme's palette, as the background does: for
/// `kaamos`, slot 15 is dark ink, but white on another palette. Set the
/// palette or use truecolour.
///
/// Violet and orange have no slot: sixteen slots hold eight hues, and the
/// scheme puts pink in bright magenta. Violet borrows magenta too, and
/// orange red.
///
/// Six of the scheme's twenty-seven roles go unused, needing distinctions
/// bish can't make: a hot keyword from an ordinary one, a declaration
/// from a modifier, a spelled-out operator from a symbolic one, a builtin
/// type from a user's, a literal from a constant, and deprecation. Those
/// need a parser or LSP token modifiers, and bish reads semantic tokens
/// by type name only.
///
/// `variable` is both bish's kind for a shell expansion (`$HOME`) and the
/// LSP name for a program variable, folded together by
/// `highlight::kind_for_semantic_type`. It takes the shell reading, the
/// scheme's escape colour for "the one thing in there that is not text";
/// `parameter` is set to plain ink so code doesn't inherit it.
pub const BUILTIN_THEMES: &[Builtin] = &[
    Builtin { name: "revontulet", about: "Talvitaivas, night sky -- for a terminal at #010719 or near it", hl: REVONTULET_HL, opts: REVONTULET_OPTS },
    Builtin { name: "kaamos", about: "Talvitaivas, day sky -- dark ink, for a light terminal at #C7D6FE", hl: KAAMOS_HL, opts: KAAMOS_OPTS },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTIN_THEMES.iter().find(|t| t.name == name)
}

// What each bish highlight name was given; the same for both skies.
//
// Five are the scheme's roles verbatim: `keyword`, `operator`, `string`,
// `comment`, `number`. `key` is its "field", a named slot (object field,
// property, JSON key, markup attribute), which is bish's `Key`.
//
// The rest needed a reading:
//
// * `redirect` takes the third magenta step: the language talking, like a
//   keyword, on magenta's spare step (the markup tag, which bish doesn't
//   highlight). It keeps the magenta `default_style` gives it.
// * `variable`, `substitution` and `format_specifier` are pink: the
//   scheme gives `${}` and `\n` one colour, "the one colour allowed
//   inside a string", and an expansion and a `%s` are exactly that. A
//   command substitution takes the step below: a span, not a name.
// * `invalid_command` takes the red the scheme spends on errors.
//
// The browser's entry types, where the scheme and `ls`'s colours
// disagree, follow the scheme:
//
// * a directory is a name you can jump to, so blue, at the loudest step
//   (a definition), as it's what a listing is searched for.
// * an executable is blue at the step the scheme calls a call; its shell
//   specimen paints command names there.
// * a symlink is the quietest blue, the scheme's module, "a path to
//   somewhere else"; `ui_col_link` matches.
// * an archive is data in a box: the quiet step of yellow.
const REVONTULET_HL: &[(&str, &str)] = &[
    ("comment", "#117172, -bish-cyan"),
    // A flag is the scheme's "unit", the quiet step of data, as its shell
    // specimen paints `-euo` and `--seed`. A subcommand is a call; a path
    // that resolves is the module step, like `ui_col_link`.
    ("flag", "#A68017, -bish-yellow"),
    ("format_specifier", "#FC80C7, -bish-bright-magenta"),
    ("invalid_command", "#FB4264, -bish-red"),
    ("key", "#FDC943, -bish-bright-yellow"),
    ("keyword", "#CB5EFB, -bish-magenta"),
    ("link", "#3462F9, -bish-blue"),
    ("number", "#DAA922, -bish-yellow"),
    ("operator", "#B3B9CC, -bish-white"),
    ("redirect", "#D88DFC, -bish-magenta"),
    ("string", "#61C721, -bish-green"),
    ("subcommand", "#6993FB, -bish-blue"),
    ("substitution", "#FB3BB7, -bish-bright-magenta"),
    ("variable", "#FC80C7, -bish-bright-magenta"),
    // LSP 3.16 semantic-token types, by their legend names: `::bish hl`'s
    // namespace is open, so any token type is themeable. These are the
    // scheme's code roles, most with no other way into bish.
    //
    // Only names with no bash kind of their own, plus five bish folds where
    // the scheme disagrees: `property` is a field, not a variable;
    // `enumMember` a constant; `regexp` a loud string; `modifier` its own
    // violet; `parameter` plain ink, not an expansion.
    ("class", "#FC9C65, -bish-bright-red"),
    ("decorator", "#FB3BB7, -bish-bright-magenta"),
    ("enum", "#FC9C65, -bish-bright-red"),
    ("enumMember", "#FB4264, -bish-red"),
    ("event", "#C8188E, -bish-bright-magenta"),
    ("function", "#6993FB, -bish-blue"),
    ("interface", "#FC9C65, -bish-bright-red"),
    ("macro", "#FB3BB7, -bish-bright-magenta"),
    ("method", "#6993FB, -bish-blue"),
    ("modifier", "#824EF9, -bish-magenta"),
    ("namespace", "#3462F9, -bish-blue"),
    ("parameter", "#D7DDF0, -bish-bright-white"),
    ("property", "#FDC943, -bish-bright-yellow"),
    ("regexp", "#78F32B, -bish-bright-green"),
    ("struct", "#FC9C65, -bish-bright-red"),
    ("type", "#FC9C65, -bish-bright-red"),
    ("typeParameter", "#BD5A13, -bish-red"),
];

// The chrome. Severities are the scheme's diagnostic colours: red error,
// yellow warning, blue info, cyan hint. The markdown roles are its prose
// family: a heading is its "doc tag", a quote a doc comment, inline code
// the raw string it uses for code spans.
const REVONTULET_OPTS: &[(&str, &str)] = &[
    ("ui_col_archive", "#A68017, -bish-yellow"),
    ("ui_col_code", "#479616, -bish-green"),
    ("ui_col_directory", "#97B6FC, -bish-bright-blue"),
    ("ui_col_error", "#FB4264, -bish-red"),
    ("ui_col_executable", "#6993FB, -bish-blue"),
    ("ui_col_heading", "#2EE5E7, -bish-bright-cyan"),
    ("ui_col_hint", "#24BBBC, -bish-cyan"),
    ("ui_col_info", "#6993FB, -bish-blue"),
    ("ui_col_link", "#3462F9, -bish-blue"),
    ("ui_col_quote", "#24BBBC, -bish-cyan"),
    ("ui_col_symlink", "#3462F9, -bish-blue"),
    ("ui_col_warning", "#DAA922, -bish-yellow"),
];

// Kaamos is the same table on a day sky: each family keeps its hue and
// each role its step, running dark instead of bright, as darker is
// louder on a light ground.
const KAAMOS_HL: &[(&str, &str)] = &[
    ("comment", "#147B7C, -bish-cyan"),
    ("flag", "#866710, -bish-yellow"),
    ("format_specifier", "#600643, -bish-bright-magenta"),
    ("invalid_command", "#8B0C2D, -bish-red"),
    ("key", "#503D05, -bish-bright-yellow"),
    ("keyword", "#6E0D91, -bish-magenta"),
    ("link", "#1E41F8, -bish-blue"),
    ("number", "#6A510A, -bish-yellow"),
    ("operator", "#424756, -bish-white"),
    ("redirect", "#51076B, -bish-magenta"),
    ("string", "#275808, -bish-green"),
    ("subcommand", "#1215D6, -bish-blue"),
    ("substitution", "#830C5C, -bish-bright-magenta"),
    ("variable", "#600643, -bish-bright-magenta"),
    ("class", "#562504, -bish-bright-red"),
    ("decorator", "#830C5C, -bish-bright-magenta"),
    ("enum", "#562504, -bish-bright-red"),
    ("enumMember", "#8B0C2D, -bish-red"),
    ("event", "#AC137A, -bish-bright-magenta"),
    ("function", "#1215D6, -bish-blue"),
    ("interface", "#562504, -bish-bright-red"),
    ("macro", "#830C5C, -bish-bright-magenta"),
    ("method", "#1215D6, -bish-blue"),
    ("modifier", "#6F19EB, -bish-magenta"),
    ("namespace", "#1E41F8, -bish-blue"),
    ("parameter", "#212534, -bish-bright-white"),
    ("property", "#503D05, -bish-bright-yellow"),
    ("regexp", "#1A4004, -bish-bright-green"),
    ("struct", "#562504, -bish-bright-red"),
    ("type", "#562504, -bish-bright-red"),
    ("typeParameter", "#9B480D, -bish-red"),
];

const KAAMOS_OPTS: &[(&str, &str)] = &[
    ("ui_col_archive", "#866710, -bish-yellow"),
    ("ui_col_code", "#36750E, -bish-green"),
    ("ui_col_directory", "#0B0DA0, -bish-bright-blue"),
    ("ui_col_error", "#8B0C2D, -bish-red"),
    ("ui_col_executable", "#1215D6, -bish-blue"),
    ("ui_col_heading", "#07494A, -bish-bright-cyan"),
    ("ui_col_hint", "#0D6262, -bish-cyan"),
    ("ui_col_info", "#1215D6, -bish-blue"),
    ("ui_col_link", "#1E41F8, -bish-blue"),
    ("ui_col_quote", "#0D6262, -bish-cyan"),
    ("ui_col_symlink", "#1E41F8, -bish-blue"),
    ("ui_col_warning", "#6A510A, -bish-yellow"),
];

#[cfg(test)]
mod tests {
    use super::*;

    // Every shipped colour parses as `::bish hl --set` would parse it, so a
    // typo fails a test.
    #[test]
    fn every_shipped_colour_parses_with_a_fallback_for_a_terminal_that_cannot_show_it() {
        for theme in BUILTIN_THEMES {
            for (name, css) in theme.hl.iter().chain(theme.opts) {
                let parsed = crate::csscolor::parse_terminal_list(css).unwrap_or_else(|e| panic!("{}: {name}: {css:?}: {e}", theme.name));
                // Truecolour first, then an ANSI slot; without it the theme
                // vanishes on `TERM=xterm-256color`, common over ssh.
                assert!(
                    matches!(parsed.first(), Some(crate::csscolor::TermColor::Rgba(_))),
                    "{}: {name}: the exact colour has to come first",
                    theme.name
                );
                assert!(
                    parsed.iter().any(|c| matches!(c, crate::csscolor::TermColor::Ansi(n) if *n < 16)),
                    "{}: {name}: {css:?} has no fallback for a terminal without truecolour",
                    theme.name
                );
            }
        }
    }

    // The two skies are one scheme, so a role in one but not the other was
    // forgotten; a new `HL_NAMES` or `UI_COL_OPTIONS` entry needs both.
    #[test]
    fn both_skies_cover_the_same_roles_and_every_role_bish_can_colour() {
        let names = |rows: &[(&str, &str)]| {
            let mut v: Vec<String> = rows.iter().map(|(n, _)| (*n).to_string()).collect();
            v.sort();
            v
        };
        let skies: Vec<(&str, Vec<String>, Vec<String>)> = BUILTIN_THEMES.iter().map(|t| (t.name, names(t.hl), names(t.opts))).collect();
        for sky in &skies[1..] {
            assert_eq!((&sky.1, &sky.2), (&skies[0].1, &skies[0].2), "{} and {} must name the same roles", skies[0].0, sky.0);
        }

        // Every option in this module's table, and every highlight name.
        for (_, option) in UI_COL_OPTIONS {
            assert!(skies[0].2.iter().any(|n| n == option), "no shipped theme sets {option}");
        }
        for (_, name) in crate::bishedit::highlight::HL_NAMES {
            assert!(skies[0].1.iter().any(|n| n == name), "no shipped theme sets {name}");
        }
        // Nothing set twice, which would make the winner depend on table
        // order.
        for (label, mut rows) in [("hl", skies[0].1.clone()), ("opts", skies[0].2.clone())] {
            let count = rows.len();
            rows.dedup();
            assert_eq!(rows.len(), count, "a name appears twice in {label}");
        }
    }

    // A role's family survives the sky changing; the ANSI fallback, the
    // scheme's own reduction, says which family.
    #[test]
    fn a_sky_moves_a_colour_and_never_a_meaning() {
        let slot = |theme: &Builtin, name: &str| {
            let css = theme.hl.iter().chain(theme.opts).find(|(n, _)| *n == name).map(|(_, css)| *css).expect(name);
            css.split(", ").nth(1).expect(name).to_string()
        };
        let rgb = |theme: &Builtin, name: &str| {
            let css = theme.hl.iter().chain(theme.opts).find(|(n, _)| *n == name).map(|(_, css)| *css).expect(name);
            css.split(',').next().expect(name).to_string()
        };
        let (night, day) = (&BUILTIN_THEMES[0], &BUILTIN_THEMES[1]);
        assert_eq!((night.name, day.name), ("revontulet", "kaamos"));
        for (_, name) in crate::bishedit::highlight::HL_NAMES {
            assert_eq!(slot(night, name), slot(day, name), "{name} changed family between the two skies");
            assert_ne!(rgb(night, name), rgb(day, name), "{name} is the same colour in both skies");
        }
        // Night writes light on dark, day dark on light.
        assert_eq!(rgb(night, "keyword"), "#CB5EFB");
        assert_eq!(rgb(day, "keyword"), "#6E0D91");
    }

    // Every element has an option and every option is real: a name on one
    // side only is a colour nobody can set, or an option setting nothing.
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

    // The two drawn in the terminal's own foreground.
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

    // The defaults.
    #[test]
    fn the_defaults_are_what_was_drawn_before() {
        assert_eq!(sgr(Ui::Directory, None), "\x1b[0;1;34m");
        assert_eq!(sgr(Ui::Symlink, None), "\x1b[0;36m");
        assert_eq!(sgr(Ui::Archive, None), "\x1b[0;35m");
        assert_eq!(sgr(Ui::Executable, None), "\x1b[0;32m");
    }
}
