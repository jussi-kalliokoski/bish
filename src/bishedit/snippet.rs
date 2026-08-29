// Snippet expansion for `abbr`. An abbreviation whose expansion contains
// `%s` placeholders -- `abbr -a gcm 'git commit -m "%s"'` -- doesn't
// splice in as finished text: it splices in *tentatively*, with each
// placeholder shown as such and the caret parked in the first one, and
// the user tabs between them and types into them before accepting.
//
// A pure model, no terminal anywhere in it: this module owns what a
// snippet *is* (the literal chunks around its placeholders, each
// placeholder's current fill, which one is active, and the order they're
// visited in) and what it renders to; editor.rs owns the keystrokes and
// the drawing. Same split browser.rs and hexedit.rs use one tier up, and
// the reason every rule below is unit-tested with no `read_line` in
// sight.
//
// `%s` is the placeholder token and `%%` is a literal `%`, matching
// printf's own escaping -- a bare `%` before anything else is just a `%`,
// so an expansion that never meant to opt in (`awk '{print $1%2}'`)
// stays exactly what it says.

// What a placeholder looks like in the expansion, and what an unfilled
// one renders back as: seeing the token you wrote is what makes a
// tentative snippet legible as one.
pub const PLACEHOLDER: &str = "%s";

// The language an abbreviation targets unless `--lang=` says otherwise,
// and the language the shell prompt itself counts as -- so an
// abbreviation written without thinking about languages at all keeps
// working exactly where it always did.
pub const DEFAULT_LANG: &str = "bash";

// One stored abbreviation. Lives here rather than in exec.rs so that
// editor.rs and fileeditor.rs -- which do the expanding -- can name the
// type without depending on the shell: exec.rs owns the *table*, this is
// just its record. See `Shell::abbrs` for the storage/trigger split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abbr {
    pub name: String,
    pub expansion: String,
    // `--lang=`: a glob matched against the language of wherever
    // expansion is being attempted -- `bash` at the shell prompt, the
    // file's own language in the editor (see
    // fileeditor::language_of). A glob rather than a plain name so one
    // abbreviation can cover a family (`--lang='*script'`) or everything
    // but one (`--lang='!(bash)'`, which the shared glob engine already
    // understands as an extglob).
    pub lang: String,
    // The order the expansion's placeholders are visited in: 0-based
    // indices into them in text order, a permutation of `0..n`. Empty
    // means text order, which is also what every abbreviation without
    // placeholders has. `abbr -a foo 'bar -x %s -y %s' 2 1` stores
    // `[1, 0]`: the `-y` placeholder is filled first.
    pub order: Vec<usize>,
}

impl Abbr {
    pub fn new(name: &str, expansion: &str) -> Abbr {
        Abbr { name: name.to_string(), expansion: expansion.to_string(), lang: DEFAULT_LANG.to_string(), order: Vec::new() }
    }

    // Whether this abbreviation is live in `language`. Uses the shell's
    // own glob engine, so `--lang=` accepts exactly what a `case` pattern
    // does -- including `!(...)`, `@(a|b)` and character classes -- with
    // no second pattern dialect to learn or to maintain.
    pub fn applies_to(&self, language: &str) -> bool {
        crate::glob::matches(&self.lang, language)
    }
}

// The abbreviations from `table` that are live in `language`, in
// definition order. Both trigger sites take this snapshot rather than
// filtering at lookup time: the table is small, the filter result is
// stable for a whole prompt (or a whole Insert-mode session), and it
// keeps `expand_abbr_at_cursor` from needing to know what a language is.
pub fn for_language(table: &[Abbr], language: &str) -> Vec<Abbr> {
    table.iter().filter(|a| a.applies_to(language)).cloned().collect()
}

// A live snippet: the expansion split around its placeholders, plus what
// has been typed into each so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    // The literal text around the placeholders, always exactly one more
    // than there are placeholders -- `chunks[0]` precedes the first,
    // `chunks[n]` follows the last, and either can be empty.
    chunks: Vec<String>,
    // What has been typed into each placeholder, in *text* order (not
    // visit order). Empty means "still unfilled", which is why deleting
    // a fill back to nothing restores the `%s` on screen rather than
    // leaving a hole: there is no third state to track.
    fills: Vec<String>,
    // Visit order: `order[k]` is the placeholder filled k-th. Always a
    // permutation of `0..fills.len()`.
    order: Vec<usize>,
    // Where in `order` the caret currently is -- an index into `order`,
    // not into `fills`.
    step: usize,
}

// Splits an expansion into its literal chunks, resolving `%%` to `%`.
// Returns `(chunks, placeholder_count)`.
fn split_placeholders(expansion: &str) -> (Vec<String>, usize) {
    let mut chunks = vec![String::new()];
    let mut chars = expansion.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            chunks.last_mut().unwrap().push(c);
            continue;
        }
        match chars.peek() {
            Some('s') => {
                chars.next();
                chunks.push(String::new());
            }
            Some('%') => {
                chars.next();
                chunks.last_mut().unwrap().push('%');
            }
            // A `%` before anything else (including end of string) is
            // just a `%`.
            _ => chunks.last_mut().unwrap().push('%'),
        }
    }
    let count = chunks.len() - 1;
    (chunks, count)
}

// How many `%s` placeholders an expansion has. Used by `abbr` itself to
// decide whether a trailing run of integers is an order specification or
// just more words of the expansion.
pub fn placeholder_count(expansion: &str) -> usize {
    split_placeholders(expansion).1
}

// True when `order` is a permutation of `0..count` -- the only shape
// `Snippet` will accept, so a malformed one degrades to text order
// rather than panicking or silently dropping a placeholder.
pub fn is_valid_order(order: &[usize], count: usize) -> bool {
    if order.len() != count {
        return false;
    }
    let mut seen = vec![false; count];
    for &i in order {
        match seen.get_mut(i) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    true
}

// Tells `abbr -a foo 'bar -x %s -y %s' 2 1` (a placeholder order) apart
// from `abbr -a foo echo 1 2` (four words of expansion). The rule is
// deliberately narrow, because the two really are the same shape once
// the words are joined:
//
//   * the trailing run must be *separate argv words*, so the order is
//     only ever found where the user actually split it off -- quoting the
//     whole thing (`abbr -a foo 'echo %s 1'`) never triggers it;
//   * every word in the run must be a plain positive integer;
//   * there must be exactly as many of them as the rest has `%s`
//     placeholders, and they must form a permutation of `1..=n`.
//
// Anything short of all three is an ordinary expansion word, so the
// pre-existing behavior (join everything) is what every abbreviation
// without placeholders still gets. Returns the words that are really the
// expansion, plus the 0-based order (empty for text order).
pub fn parse_order(words: &[String]) -> (&[String], Vec<usize>) {
    let split = words.iter().rposition(|w| w.parse::<usize>().is_err()).map_or(0, |i| i + 1);
    let (head, tail) = words.split_at(split);
    if tail.is_empty() || head.is_empty() {
        return (words, Vec::new());
    }
    let count = placeholder_count(&head.join(" "));
    let order: Vec<usize> = tail.iter().filter_map(|w| w.parse::<usize>().ok()).map(|n| n.wrapping_sub(1)).collect();
    if is_valid_order(&order, count) { (head, order) } else { (words, Vec::new()) }
}

// Pulls `--lang=GLOB` out of `abbr`'s own argument list, returning the
// rest and the glob. Only recognized among the *leading* options -- the
// run of `-a`/`--erase`/... flags before the NAME -- so `abbr -a foo echo
// --lang=x` still stores four ordinary words of expansion, the same way
// `parse_order` only reads a trailing integer run where the user
// actually split it off. Order within that run doesn't matter: both
// `abbr --lang=rust -a foo ...` and `abbr -a --lang=rust foo ...` work.
pub fn take_lang_flag(args: &[String]) -> (Vec<String>, Option<String>) {
    const MODE_FLAGS: [&str; 10] = ["-a", "--add", "-e", "--erase", "-l", "--list", "-s", "--show", "-q", "--query"];
    let mut lang = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut leading = true;
    for arg in args {
        if leading && let Some(value) = arg.strip_prefix("--lang=") {
            lang = Some(value.to_string());
            continue;
        }
        if !MODE_FLAGS.contains(&arg.as_str()) {
            leading = false;
        }
        rest.push(arg.clone());
    }
    (rest, lang)
}

impl Snippet {
    // `None` when the expansion has no placeholders at all -- that's an
    // ordinary abbreviation, and the caller splices it as plain text
    // exactly as it always did. An `order` that isn't a permutation of
    // the placeholders is ignored rather than rejected: text order is
    // always a correct answer, and a stored abbreviation is not worth
    // refusing to expand over.
    pub fn parse(expansion: &str, order: &[usize]) -> Option<Snippet> {
        let (chunks, count) = split_placeholders(expansion);
        if count == 0 {
            return None;
        }
        let order = if is_valid_order(order, count) { order.to_vec() } else { (0..count).collect() };
        Some(Snippet { chunks, fills: vec![String::new(); count], order, step: 0 })
    }

    pub fn placeholder_count(&self) -> usize {
        self.fills.len()
    }

    // The placeholder the caret is in, as an index in *text* order.
    pub fn active(&self) -> usize {
        self.order[self.step]
    }

    // True when the caret is on the last placeholder in visit order --
    // where Enter accepts the snippet instead of advancing.
    pub fn at_last(&self) -> bool {
        self.step + 1 == self.order.len()
    }

    // What the snippet currently looks like on the line: filled
    // placeholders show what was typed, unfilled ones show the `%s` they
    // came from.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, chunk) in self.chunks.iter().enumerate() {
            out.push_str(chunk);
            if let Some(fill) = self.fills.get(i) {
                out.push_str(if fill.is_empty() { PLACEHOLDER } else { fill });
            }
        }
        out
    }

    // Every placeholder's own `[start, end)` span within `render()`'s
    // output, in text order and measured in chars (which is what the
    // line editor's own buffer is indexed by).
    pub fn spans(&self) -> Vec<(usize, usize)> {
        let mut spans = Vec::with_capacity(self.fills.len());
        let mut at = 0;
        for (i, chunk) in self.chunks.iter().enumerate() {
            at += chunk.chars().count();
            if let Some(fill) = self.fills.get(i) {
                let width = if fill.is_empty() { PLACEHOLDER.chars().count() } else { fill.chars().count() };
                spans.push((at, at + width));
                at += width;
            }
        }
        spans
    }

    // Where the real caret belongs within `render()`'s output: at the end
    // of what has been typed into the active placeholder, which for an
    // unfilled one is the *start* of its `%s` -- so the token reads as
    // something about to be replaced rather than as something already
    // typed.
    pub fn caret(&self) -> usize {
        let active = self.active();
        let (start, end) = self.spans()[active];
        if self.fills[active].is_empty() { start } else { end }
    }

    pub fn type_char(&mut self, c: char) {
        let active = self.active();
        self.fills[active].push(c);
    }

    // Deletes the last char of the active placeholder's fill. `false`
    // when there was nothing to delete -- the caller's cue that
    // Backspace had no meaning here, rather than silently eating the
    // literal text around the snippet.
    pub fn backspace(&mut self) -> bool {
        let active = self.active();
        self.fills[active].pop().is_some()
    }

    // Wraps in both directions, the same way the completion menu's own
    // Tab/Shift-Tab cycling already does: with two placeholders, tabbing
    // past the last one lands back on the first rather than dead-ending.
    pub fn advance(&mut self, backward: bool) {
        let n = self.order.len();
        self.step = if backward { (self.step + n - 1) % n } else { (self.step + 1) % n };
    }

    // The finished text: unfilled placeholders contribute nothing at all,
    // so a snippet accepted without touching every hole reads as if those
    // arguments were simply never typed, rather than leaving a literal
    // `%s` in a command about to run.
    pub fn accept(&self) -> String {
        let mut out = String::new();
        for (i, chunk) in self.chunks.iter().enumerate() {
            out.push_str(chunk);
            if let Some(fill) = self.fills.get(i) {
                out.push_str(fill);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------
// A snippet spliced into a real buffer
// ---------------------------------------------------------------------

// The one thing a live snippet needs from whatever buffer it's spliced
// into. An `abbr` expansion is a single line by construction (`run_abbr`
// joins its words with spaces), so a snippet never spans one either --
// which is what lets the shell prompt's single-line `LineEditor` and the
// file editor's multi-line `TextBuffer` share every operation below
// instead of each growing its own copy of them.
pub trait SnippetHost {
    // Replace chars `[start, end)` of `line` with `text`.
    fn replace_in_line(&mut self, line: usize, start: usize, end: usize, text: &str);
    fn place_cursor(&mut self, line: usize, col: usize);
}

// A snippet that is currently spliced into a buffer, tentatively: the
// model plus where its rendered text sits and what was there before it.
//
// The invariant every method preserves: chars `[col, col + len)` of
// `line` are exactly `snip.render()`, and the cursor is wherever
// `snip.caret()` says. Nothing else should touch that span while this
// is alive.
pub struct LiveSnippet {
    pub snip: Snippet,
    line: usize,
    col: usize,
    len: usize,
    // The abbreviation name this replaced -- what `cancel` puts back.
    original: String,
}

impl LiveSnippet {
    // Replaces the abbreviation name at `[col, col + original.len())` of
    // `line` with the snippet's first rendering, caret in the first
    // placeholder.
    pub fn start(snip: Snippet, line: usize, col: usize, original: String, host: &mut impl SnippetHost) -> LiveSnippet {
        let len = original.chars().count();
        let mut live = LiveSnippet { snip, line, col, len, original };
        live.sync(host);
        live
    }

    // Rewrites the span from the model and parks the cursor in the
    // active placeholder. Called after every edit to the model, so the
    // two can't drift.
    pub fn sync(&mut self, host: &mut impl SnippetHost) {
        let rendered = self.snip.render();
        host.replace_in_line(self.line, self.col, self.col + self.len, &rendered);
        self.len = rendered.chars().count();
        host.place_cursor(self.line, self.col + self.snip.caret());
    }

    // Turns the tentative snippet into ordinary text: unfilled
    // placeholders contribute nothing, and the cursor lands right after
    // the result -- exactly where it would be had the whole thing just
    // been typed out by hand.
    pub fn accept(self, host: &mut impl SnippetHost) {
        let text = self.snip.accept();
        host.replace_in_line(self.line, self.col, self.col + self.len, &text);
        host.place_cursor(self.line, self.col + text.chars().count());
    }

    pub fn cancel(self, host: &mut impl SnippetHost) {
        host.replace_in_line(self.line, self.col, self.col + self.len, &self.original);
        host.place_cursor(self.line, self.col + self.original.chars().count());
    }

    pub fn line(&self) -> usize {
        self.line
    }

    // Every placeholder as `(start_col, end_col, is_active)` on `line` --
    // what each of the two renderers turns into its own highlighting.
    pub fn holes(&self) -> Vec<(usize, usize, bool)> {
        let active = self.snip.active();
        self.snip
            .spans()
            .into_iter()
            .enumerate()
            .map(|(i, (start, end))| (self.col + start, self.col + end, i == active))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snip(expansion: &str) -> Snippet {
        Snippet::parse(expansion, &[]).expect("expansion has placeholders")
    }

    #[test]
    fn an_expansion_without_placeholders_is_not_a_snippet() {
        assert_eq!(Snippet::parse("git checkout", &[]), None);
    }

    #[test]
    fn a_fresh_snippet_shows_its_placeholders_verbatim() {
        let s = snip("bar -x %s -y %s | qoo");
        assert_eq!(s.render(), "bar -x %s -y %s | qoo");
        assert_eq!(s.placeholder_count(), 2);
    }

    #[test]
    fn typing_replaces_the_active_placeholder_only() {
        let mut s = snip("bar -x %s -y %s | qoo");
        for c in "one".chars() {
            s.type_char(c);
        }
        assert_eq!(s.render(), "bar -x one -y %s | qoo");
        s.advance(false);
        for c in "two".chars() {
            s.type_char(c);
        }
        assert_eq!(s.render(), "bar -x one -y two | qoo");
    }

    #[test]
    fn deleting_a_fill_back_to_nothing_restores_the_placeholder() {
        let mut s = snip("echo %s");
        s.type_char('h');
        assert_eq!(s.render(), "echo h");
        assert!(s.backspace());
        assert_eq!(s.render(), "echo %s", "an emptied placeholder is unfilled again, not a hole");
        assert!(!s.backspace(), "there is nothing left to delete inside the placeholder");
    }

    #[test]
    fn accepting_drops_unfilled_placeholders_entirely() {
        let mut s = snip("bar -x %s -y %s | qoo");
        for c in "one".chars() {
            s.type_char(c);
        }
        assert_eq!(s.accept(), "bar -x one -y  | qoo", "the untouched `%s` leaves nothing behind");
    }

    #[test]
    fn spans_and_caret_track_what_is_actually_rendered() {
        let mut s = snip("ab%scd%s");
        assert_eq!(s.spans(), vec![(2, 4), (6, 8)]);
        // Unfilled: the caret sits *before* the token it will replace.
        assert_eq!(s.caret(), 2);
        s.type_char('X');
        assert_eq!(s.render(), "abXcd%s");
        assert_eq!(s.spans(), vec![(2, 3), (5, 7)]);
        assert_eq!(s.caret(), 3, "filled: the caret follows what was typed");
    }

    #[test]
    fn tab_order_wraps_in_both_directions() {
        let mut s = snip("%s %s %s");
        assert_eq!(s.active(), 0);
        s.advance(true);
        assert_eq!(s.active(), 2, "back from the first wraps to the last");
        s.advance(false);
        assert_eq!(s.active(), 0, "and forward from the last wraps to the first");
    }

    #[test]
    fn at_last_is_about_visit_order_not_text_order() {
        let mut s = Snippet::parse("%s %s", &[1, 0]).unwrap();
        assert_eq!(s.active(), 1, "the reversed order fills the second placeholder first");
        assert!(!s.at_last());
        s.advance(false);
        assert_eq!(s.active(), 0);
        assert!(s.at_last(), "the *first* placeholder is the last one visited here");
    }

    #[test]
    fn a_reversed_order_puts_typed_text_where_the_caret_actually_was() {
        let mut s = Snippet::parse("bar -x %s -y %s", &[1, 0]).unwrap();
        for c in "why".chars() {
            s.type_char(c);
        }
        s.advance(false);
        for c in "ex".chars() {
            s.type_char(c);
        }
        assert_eq!(s.accept(), "bar -x ex -y why");
    }

    #[test]
    fn a_nonsense_order_falls_back_to_text_order_rather_than_refusing() {
        // Not a permutation (repeats 0, never mentions 1) -- an
        // abbreviation is not worth refusing to expand over.
        let s = Snippet::parse("%s %s", &[0, 0]).unwrap();
        assert_eq!(s.order, vec![0, 1]);
        assert!(!is_valid_order(&[0, 0], 2));
        assert!(!is_valid_order(&[0], 2), "too short");
        assert!(!is_valid_order(&[0, 2], 2), "out of range");
        assert!(is_valid_order(&[1, 0], 2));
    }

    #[test]
    fn double_percent_is_a_literal_percent_and_a_lone_one_is_left_alone() {
        assert_eq!(placeholder_count("100%% sure"), 0);
        assert_eq!(Snippet::parse("100%% sure", &[]), None);
        // A `%` that isn't opting in stays exactly what it says.
        assert_eq!(placeholder_count("awk '{print $1%2}'"), 0);
        let s = snip("%%s is %s");
        assert_eq!(s.render(), "%s is %s", "the escaped one is literal text, only the second is a hole");
        assert_eq!(s.spans(), vec![(6, 8)]);
    }

    fn strs(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_language_glob_is_the_shells_own_glob() {
        let mut a = Abbr::new("foo", "bar");
        assert_eq!(a.lang, DEFAULT_LANG);
        assert!(a.applies_to("bash"));
        assert!(!a.applies_to("rust"));

        a.lang = "*script".to_string();
        assert!(a.applies_to("javascript") && a.applies_to("typescript"));
        assert!(!a.applies_to("rust"));

        // Inverse globbing, via the extglob the shared engine already
        // understands -- everything *except* bash.
        a.lang = "!(bash)".to_string();
        assert!(a.applies_to("rust") && a.applies_to("text"));
        assert!(!a.applies_to("bash"));

        a.lang = "@(rust|go)".to_string();
        assert!(a.applies_to("rust") && a.applies_to("go") && !a.applies_to("bash"));

        a.lang = "*".to_string();
        assert!(a.applies_to("anything at all"));
    }

    #[test]
    fn for_language_keeps_definition_order() {
        let table = vec![
            Abbr::new("a", "one"),
            Abbr { lang: "rust".into(), ..Abbr::new("b", "two") },
            Abbr { lang: "!(bash)".into(), ..Abbr::new("c", "three") },
        ];
        let names = |lang: &str| for_language(&table, lang).into_iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(names("bash"), vec!["a"]);
        assert_eq!(names("rust"), vec!["b", "c"]);
        assert_eq!(names("toml"), vec!["c"]);
    }

    #[test]
    fn the_lang_flag_is_only_read_among_the_leading_options() {
        // Either side of the mode flag.
        assert_eq!(take_lang_flag(&strs(&["--lang=rust", "-a", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), Some("rust".into())));
        assert_eq!(take_lang_flag(&strs(&["-a", "--lang=rust", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), Some("rust".into())));
        // With no mode flag at all (`abbr NAME EXPANSION` means add).
        assert_eq!(take_lang_flag(&strs(&["--lang=rust", "foo", "bar"])), (strs(&["foo", "bar"]), Some("rust".into())));
        // After the NAME it is ordinary expansion text, the same way
        // `parse_order` only reads a trailing integer run.
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "echo", "--lang=x"])), (strs(&["-a", "foo", "echo", "--lang=x"]), None));
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), None));
    }

    #[test]
    fn a_placeholder_can_sit_at_either_end_of_the_expansion() {
        let mut s = snip("%s");
        assert_eq!(s.render(), "%s");
        assert_eq!(s.spans(), vec![(0, 2)]);
        s.type_char('z');
        assert_eq!(s.accept(), "z");
    }
}
