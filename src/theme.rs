// The colours of bish's own chrome -- the file browser's entry types,
// the editor's gutter, pane dividers, rendered markdown, diagnostics --
// and, at the bottom of this file, the themes bish ships.
//
// Deliberately the same shape `bishedit::highlight`'s own colour
// machinery already has, one axis over: a table naming each element's
// bishopt, a `default_style` giving what it looks like with nothing
// set, and a resolved map a caller with a live `Shell` builds once per
// redraw. Two parallel systems that worked differently would be two
// things to learn; this way `ui_col_directory` behaves exactly as
// `::bish hl` names do, and both land in a `::bish theme` declaration
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

// ---------------------------------------------------------------------
// The themes bish ships
// ---------------------------------------------------------------------

/// A theme bish ships, activated by name like any declared one:
/// `bishopt --set theme kaamos`.
///
/// The same two tables `::bish theme begin`/`end` would have captured --
/// `::bish hl` names and `ui_col_*` bishopts -- except that these are
/// `&'static str` CSS rather than parsed values, so they cost nothing
/// until something asks. A declared theme of the same name wins per
/// name, not wholesale (see `Shell::bishopt_value`), which is what makes
/// "I like kaamos but not its comments" one line of bishrc rather than a
/// theme of your own.
pub struct Builtin {
    pub name: &'static str,
    /// One line for `::bish theme list`.
    pub about: &'static str,
    /// `::bish hl` names -- what the highlighter paints.
    pub hl: &'static [(&'static str, &'static str)],
    /// bishopt names, every one of them a `Color` option -- bish's own
    /// chrome.
    pub opts: &'static [(&'static str, &'static str)],
}

/// Two skies, from the Talvitaivas code scheme.
///
/// The scheme is nine hue families at three volumes each, where the hue
/// says *what kind of thing* a word is and the lightness says how loud:
/// magenta is the language talking, violet a declaration, blue a name
/// you can jump to, cyan prose about the code, green text, yellow data,
/// orange a type, red a value that cannot change, pink something
/// fastened to the code. A variant may move a colour but never a
/// meaning, so the two skies below are the same scheme twice, read
/// against a night ground and a day one.
///
/// **Neither sets a background, because bish has no way to.** It draws
/// on whatever the terminal already is, so `revontulet` wants a terminal
/// at its own midnight blue (`#010719`) or anything near it, and
/// `kaamos` wants a light one (`#C7D6FE`) -- its ink is dark on purpose,
/// and on a black terminal it would be unreadable. That is a terminal
/// profile to pick, not something a shell can fix from inside.
///
/// Every colour carries an ANSI fallback after it, and those are not
/// guesses: the scheme declares its own mapping onto the sixteen slots,
/// where the loudest step of each family is the bright slot. A terminal
/// without truecolour gets the scheme's own reduction of itself --
/// against the scheme's own palette, which is the same assumption the
/// background makes. The sixteen slots are where the nine families came
/// from, so `kaamos` falling back to slot 15 for ink means "the
/// sixteenth colour of the day sky", which is dark; on a terminal still
/// carrying somebody else's palette it means white. Set the palette or
/// stay on truecolour.
///
/// Violet and orange have no slot of their own in that table -- sixteen
/// slots hold eight hues and the scheme has nine families, so one has to
/// borrow, which the scheme itself does by putting pink in bright
/// magenta. Violet borrows magenta the same way, and orange borrows red,
/// which is where the palette it was mixed from put it.
///
/// All nine families are used, but six of the scheme's twenty-seven
/// roles are not, and they are the six that need a distinction bish
/// cannot make: a hot keyword from an ordinary one, a declaration from a
/// modifier, an operator spelled out from one that is not, a builtin
/// type from one you wrote, a literal from a constant, and anything
/// deprecated. Those come from a parser or from a language server's
/// token *modifiers*, and bish reads neither -- its own highlighter is a
/// shell lexer, and the semantic tokens it does read it reads by type
/// name alone.
///
/// One name has to answer to two questions. `variable` is bish's own
/// kind for a shell expansion -- `$HOME`, `${x:-y}` -- and also the LSP
/// legend's name for a program variable, and bish already folds the
/// second into the first (`highlight::kind_for_semantic_type`), so they
/// cannot be coloured apart here either. It takes the shell reading: the
/// scheme paints an expansion as an escape, "the one thing in there that
/// is not text", and the prompt is where bish's own highlighting is seen
/// most. A `parameter` is set separately to plain ink, which is what
/// keeps the common case in code from inheriting it.
pub const BUILTIN_THEMES: &[Builtin] = &[
    Builtin { name: "revontulet", about: "Talvitaivas, night sky -- for a terminal at #010719 or near it", hl: REVONTULET_HL, opts: REVONTULET_OPTS },
    Builtin { name: "kaamos", about: "Talvitaivas, day sky -- dark ink, for a light terminal at #C7D6FE", hl: KAAMOS_HL, opts: KAAMOS_OPTS },
];

pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTIN_THEMES.iter().find(|t| t.name == name)
}

// What each of bish's own highlight names was given, and why -- the
// mapping is the same for both skies, which is the scheme's own rule
// that a variant moves colours and not meanings.
//
// Five are the scheme's roles verbatim: `keyword` is its keyword,
// `operator` its operator, `string` its string, `comment` its comment,
// `number` its number. `key` is its "field", which it defines as a named
// slot -- object field, property, JSON key, markup attribute, "four
// syntaxes, one idea, one colour", and bish's `Key` is that idea.
//
// The rest needed a reading:
//
// * `redirect` takes the third magenta step. A redirect is the language
//   talking, like a keyword, and magenta's spare step was the markup tag
//   -- which bish has no highlighter for. It also keeps the magenta a
//   redirect already had (`default_style`), so nothing moves hue.
// * `variable`, `substitution` and `format_specifier` are all pink,
//   the family for what is fastened to the code. The scheme gives `${}`
//   and `\n` one colour on purpose -- "the one colour allowed inside a
//   string, because it is the one thing in there that is not text" --
//   and that is exactly what an expansion and a `%s` are, so both take
//   it. A command substitution takes the step below: the same kind of
//   hole, but a span rather than a name.
// * `invalid_command` takes the red the scheme spends on errors, which
//   is the one place its red means a problem rather than a constant.
//
// And the four browser entry types, where the scheme's meanings and
// `ls`'s traditional colours disagree. The scheme wins, since that is
// what a theme is for:
//
// * a directory is a name you can jump to, so it is blue -- and the
//   loudest blue, the one the scheme spends on a definition, because in
//   a listing it is the thing you are looking for.
// * an executable is blue too, at the step the scheme calls a *call*.
//   Its own shell specimen paints every command name there, which makes
//   this the scheme's answer rather than an analogy.
// * a symlink is the quietest blue, which the scheme calls a module and
//   defines as "a path to somewhere else". A symlink is nothing else.
//   `ui_col_link` is the same colour for the same reason.
// * an archive is data in a box, so it leaves blue for the quiet step of
//   yellow. It is the one of the four that gives up a hue it had.
const REVONTULET_HL: &[(&str, &str)] = &[
    ("comment", "#117172, -bish-cyan"),
    // A flag is the scheme's "unit" -- the suffix on a number, the
    // quiet step of the data family -- which is what its own shell
    // specimen paints `-euo` and `--seed` with. A subcommand is a call,
    // and a path that resolves is the module step, "a path to somewhere
    // else", which is what `ui_col_link` already took.
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
    // The LSP 3.16 semantic-token legend, by the names a server
    // legends them under -- `::bish hl`'s namespace is open, so a
    // token type is themeable without bish knowing it exists. These
    // are the scheme's own code roles, and most of them have no other
    // way into bish: nothing in a shell is a class.
    //
    // Only the names with no bash-syntax kind of their own are listed,
    // plus the five that bish *does* fold but folds somewhere the
    // scheme disagrees with: a `property` is the scheme's field rather
    // than a variable, an `enumMember` is a constant, a `regexp` is a
    // string at the loud step, a `modifier` is its own violet, and a
    // `parameter` is a plain identifier rather than an expansion.
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

// The chrome. The four severities are the scheme's own diagnostic
// colours, which it states outright: red for an error, yellow for a
// warning, blue for info, cyan for a hint. The three markdown roles are
// its prose family -- a heading is the "doc tag" it defines as structure
// inside the prose, a quote is a doc comment, and inline code is the raw
// string it marks code spans with inside its own doc comments.
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

// Kaamos is the same table read against a day sky: every family keeps
// its hue and every role keeps its step, and the steps run dark instead
// of bright, because on a light ground darker is louder.
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

    // A shipped theme is data, and the one thing data can be is wrong.
    // Every colour here is parsed the way `::bish hl --set` would parse
    // it, so a typo is a failing test rather than a colour that silently
    // does nothing on the one terminal nobody tested.
    #[test]
    fn every_shipped_colour_parses_with_a_fallback_for_a_terminal_that_cannot_show_it() {
        for theme in BUILTIN_THEMES {
            for (name, css) in theme.hl.iter().chain(theme.opts) {
                let parsed = crate::csscolor::parse_terminal_list(css).unwrap_or_else(|e| panic!("{}: {name}: {css:?}: {e}", theme.name));
                // Truecolour first and an ANSI slot behind it. Without
                // the second, the theme simply stops existing on a
                // terminal that answers `TERM=xterm-256color`, which is
                // most of them over ssh.
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

    // The two skies are one scheme twice, so a role that exists in one
    // and not the other is a role somebody forgot -- and the day it is
    // added to `HL_NAMES` or to `UI_COL_OPTIONS` is the day both have to
    // grow with it, which is what this says out loud.
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

        // Every option in this module's own table, and every highlight
        // name bish produces.
        for (_, option) in UI_COL_OPTIONS {
            assert!(skies[0].2.iter().any(|n| n == option), "no shipped theme sets {option}");
        }
        for (_, name) in crate::bishedit::highlight::HL_NAMES {
            assert!(skies[0].1.iter().any(|n| n == name), "no shipped theme sets {name}");
        }
        // And nothing is set twice, which would make which one wins
        // depend on the order of a table nobody reads in order.
        for (label, mut rows) in [("hl", skies[0].1.clone()), ("opts", skies[0].2.clone())] {
            let count = rows.len();
            rows.dedup();
            assert_eq!(rows.len(), count, "a name appears twice in {label}");
        }
    }

    // What the scheme is for. Both skies are the same mapping, so the
    // family a role belongs to has to survive the sky changing -- and
    // the ANSI fallback is what says which family that is, since it is
    // the scheme's own sixteen-slot reduction of itself.
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
        // The one thing a reader should be able to check by eye: the
        // night sky writes light on dark and the day sky dark on light.
        assert_eq!(rgb(night, "keyword"), "#CB5EFB");
        assert_eq!(rgb(day, "keyword"), "#6E0D91");
    }

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
