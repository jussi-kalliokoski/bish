// Snippet expansion for `abbr` and language-server completions. An
// expansion with tabstops (`abbr -a gcm 'git commit -m "${1:message}"'`)
// goes in tentatively: tabstops shown, the caret in the first, Tab moving
// between them until the snippet is accepted.
//
// The syntax is LSP's, so a server's completions and `abbr` share one:
//
//   $1, $2, ...   a tabstop, visited in ascending order
//   ${1:default}  a tabstop with text to fall back on
//   $0            where the caret lands on accepting, visited last
//   \$, \}, \\     an escaped literal
//
// A number used twice is one tabstop in two places, and typing fills both:
// `${1:name}: ${2:T} = $1.into()`.
//
// A pure model: this module owns what a snippet is and what it renders
// to; editor.rs and fileeditor.rs own the keys and the drawing, which is
// why every rule here is unit-tested without `read_line`.

// The language an abbreviation targets without `--lang=`, and the
// language the shell prompt counts as.
pub const DEFAULT_LANG: &str = "bash";

// One stored abbreviation. Here rather than in exec.rs so the editors,
// which expand them, needn't depend on the shell; exec.rs owns the table
// (see `Shell::abbrs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Abbr {
    pub name: String,
    pub expansion: String,
    // `--lang=`: a glob matched against the language where expansion is
    // attempted: `bash` at the prompt, the file's language in the editor (see
    // `fileeditor::language_of`). A glob so one abbreviation can cover a
    // family (`*script`) or all but one (`!(bash)`).
    pub lang: String,
}

impl Abbr {
    #[cfg(test)]
    pub fn new(name: &str, expansion: &str) -> Abbr {
        Abbr { name: name.to_string(), expansion: expansion.to_string(), lang: DEFAULT_LANG.to_string() }
    }

    // Whether this abbreviation is live in `language`, using the shell's glob
    // engine, so `--lang=` takes what a `case` pattern does.
    pub fn applies_to(&self, language: &str) -> bool {
        crate::glob::matches(&self.lang, language)
    }
}

// The abbreviations live in `language`, in definition order. Both trigger
// sites take this snapshot, which holds for a whole prompt or Insert
// session, so `expand_abbr_at_cursor` needn't know about languages.
pub fn for_language(table: &[Abbr], language: &str) -> Vec<Abbr> {
    table.iter().filter(|a| a.applies_to(language)).cloned().collect()
}

// Pulls `--lang=GLOB` out of `abbr`'s arguments, returning the rest and
// the glob. Only among the leading options before NAME, in any order:
// `abbr -a foo echo --lang=x` stores four words of expansion.
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

// A live snippet: the expansion split around its tabstops, and what's
// been typed into each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    // The literal text around the slots, one more than there are slots:
    // `chunks[0]` before the first, `chunks[n]` after the last. Either can be
    // empty, and any can hold newlines.
    chunks: Vec<String>,
    // The tabstop each slot shows. Two slots with one tabstop are one hole in
    // two places.
    slots: Vec<usize>,
    // Per tabstop, in visit order: the number as written (`0` last), so an
    // unfilled tabstop with no default can show as `$1`.
    numbers: Vec<usize>,
    // Per tabstop: what's been typed. Empty means unfilled, so deleting a
    // fill back to nothing shows the default again.
    fills: Vec<String>,
    // Per tabstop: the `${1:here}` text, if it had one.
    defaults: Vec<Option<String>>,
    // The tabstop the caret is in, as an index into the vectors above, which
    // are in visit order.
    step: usize,
}

// One parsed piece of an expansion.
enum Piece {
    Literal(String),
    // (number, default)
    Tabstop(usize, Option<String>),
}

// Splits an expansion into literals and tabstops. Anything that isn't a
// well-formed tabstop is literal, so `echo $HOME` says what it says.
// `awk '{print $1}'` needs `\$1`.
fn parse_pieces(text: &str) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Only the three meaningful characters are escapable. Shell
            // abbreviations are full of other backslashes
            // (`find . -name '*.rs' \;`).
            '\\' if matches!(chars.get(i + 1), Some('$') | Some('}') | Some('\\')) => {
                literal.push(chars[i + 1]);
                i += 2;
            }
            '$' if i + 1 < chars.len() && chars[i + 1].is_ascii_digit() => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let number: String = chars[i + 1..j].iter().collect();
                match number.parse::<usize>() {
                    Ok(number) => {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                        pieces.push(Piece::Tabstop(number, None));
                        i = j;
                    }
                    // Digits too many to be a number are text.
                    Err(_) => {
                        literal.push('$');
                        i += 1;
                    }
                }
            }
            '$' if i + 1 < chars.len() && chars[i + 1] == '{' => match brace_tabstop(&chars, i) {
                Some((number, default, after)) => {
                    pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    pieces.push(Piece::Tabstop(number, default));
                    i = after;
                }
                None => {
                    literal.push('$');
                    i += 1;
                }
            },
            c => {
                literal.push(c);
                i += 1;
            }
        }
    }
    pieces.push(Piece::Literal(literal));
    pieces
}

// `${N}` or `${N:default}` at `i`, or `None` for ordinary text. A default
// may contain braces and tabstops (`${1:${2:inner}}`); the closing brace
// is found by depth, and the default is then taken as literal text, a
// nested tabstop becoming what it would show.
fn brace_tabstop(chars: &[char], i: usize) -> Option<(usize, Option<String>, usize)> {
    let mut j = i + 2;
    let digits_from = j;
    while j < chars.len() && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j == digits_from {
        return None;
    }
    let number: usize = chars[digits_from..j].iter().collect::<String>().parse().ok()?;
    match chars.get(j) {
        Some('}') => Some((number, None, j + 1)),
        Some(':') => {
            let mut depth = 1;
            let mut k = j + 1;
            while k < chars.len() {
                match chars[k] {
                    '\\' if k + 1 < chars.len() => k += 1,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                k += 1;
            }
            if k >= chars.len() {
                // Unterminated, so `${` is just text.
                return None;
            }
            let inner: String = chars[j + 1..k].iter().collect();
            Some((number, Some(flatten(&inner)), k + 1))
        }
        // `${1` followed by anything else isn't a tabstop. A choice
        // (`${1|a,b|}`) stays text rather than losing its options.
        _ => None,
    }
}

/// An expansion with every tabstop reduced to its default: for a caller
/// with nowhere to put a caret (a one-line preview, a nested default).
pub fn flatten(text: &str) -> String {
    let mut out = String::new();
    for piece in parse_pieces(text) {
        match piece {
            Piece::Literal(text) => out.push_str(&text),
            Piece::Tabstop(_, default) => out.push_str(&default.unwrap_or_default()),
        }
    }
    out
}

/// Whether `text` has a tabstop, deciding whether `abbr` and the completion
/// menu splice tentatively.
#[cfg(test)]
pub fn has_tabstops(text: &str) -> bool {
    parse_pieces(text).iter().any(|p| matches!(p, Piece::Tabstop(..)))
}

impl Snippet {
    /// `None` with no tabstops: that's ordinary text.
    pub fn parse(expansion: &str) -> Option<Snippet> {
        let pieces = parse_pieces(expansion);
        // Ascending by number with `$0` last, the protocol's rule, which is
        // what makes `$0` the caret's final place.
        let mut numbers: Vec<usize> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Tabstop(n, _) => Some(*n),
                _ => None,
            })
            .collect();
        crate::sort::sort(&mut numbers);
        numbers.dedup();
        if numbers.is_empty() {
            return None;
        }
        if numbers[0] == 0 {
            numbers.rotate_left(1);
        }
        let index_of = |n: usize| numbers.iter().position(|m| *m == n).expect("collected from the same pieces");

        let mut chunks = vec![String::new()];
        let mut slots = Vec::new();
        let mut defaults = vec![None; numbers.len()];
        for piece in pieces {
            match piece {
                Piece::Literal(text) => chunks.last_mut().expect("never empty").push_str(&text),
                Piece::Tabstop(number, default) => {
                    let tabstop = index_of(number);
                    // The first default given wins, so `${1:name} ... $1`
                    // keeps it.
                    if defaults[tabstop].is_none() {
                        defaults[tabstop] = default;
                    }
                    slots.push(tabstop);
                    chunks.push(String::new());
                }
            }
        }
        let fills = vec![String::new(); numbers.len()];
        Some(Snippet { chunks, slots, numbers, fills, defaults, step: 0 })
    }

    /// How many distinct tabstops: holes, not places.
    #[cfg(test)]
    pub fn tabstop_count(&self) -> usize {
        self.fills.len()
    }

    /// The tabstop the caret is in.
    pub fn active(&self) -> usize {
        self.step
    }

    /// On the last tabstop, where Enter accepts instead of advancing.
    pub fn at_last(&self) -> bool {
        self.step + 1 == self.fills.len()
    }

    // What an unfilled tabstop shows: its default, or the `$1` it was written
    // as, so an empty hole reads as one. `$0` shows nothing: it's where the
    // caret ends up, not a hole to fill.
    fn shown(&self, tabstop: usize) -> String {
        match &self.defaults[tabstop] {
            Some(default) => default.clone(),
            None if self.numbers[tabstop] == 0 => String::new(),
            None => format!("${}", self.numbers[tabstop]),
        }
    }

    fn text_of(&self, tabstop: usize) -> String {
        if self.fills[tabstop].is_empty() { self.shown(tabstop) } else { self.fills[tabstop].clone() }
    }

    /// The snippet as it looks in the buffer: fills, or defaults, or tokens.
    /// May contain newlines.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, chunk) in self.chunks.iter().enumerate() {
            out.push_str(chunk);
            if let Some(&tabstop) = self.slots.get(i) {
                out.push_str(&self.text_of(tabstop));
            }
        }
        out
    }

    /// Each slot's `[start, end)` in `render()`'s output and its tabstop, in
    /// text order, in chars.
    pub fn spans(&self) -> Vec<(usize, usize, usize)> {
        let mut spans = Vec::with_capacity(self.slots.len());
        let mut at = 0;
        for (i, chunk) in self.chunks.iter().enumerate() {
            at += chunk.chars().count();
            if let Some(&tabstop) = self.slots.get(i) {
                let width = self.text_of(tabstop).chars().count();
                spans.push((at, at + width, tabstop));
                at += width;
            }
        }
        spans
    }

    /// Where the caret belongs in `render()`'s output: at the end of the
    /// active tabstop's fill, which for an unfilled one is the start of
    /// what it shows, so the token reads as about to be replaced. A
    /// repeated tabstop puts it at the first place.
    pub fn caret(&self) -> usize {
        let active = self.active();
        match self.spans().into_iter().find(|(_, _, tabstop)| *tabstop == active) {
            Some((start, end, _)) => {
                if self.fills[active].is_empty() {
                    start
                } else {
                    end
                }
            }
            None => 0,
        }
    }

    pub fn type_char(&mut self, c: char) {
        let active = self.active();
        self.fills[active].push(c);
    }

    /// Deletes the last char of the active fill. `false` when there was
    /// nothing, so Backspace doesn't eat the text around the snippet.
    pub fn backspace(&mut self) -> bool {
        let active = self.active();
        self.fills[active].pop().is_some()
    }

    /// Wraps both ways, as the completion menu's Tab does.
    pub fn advance(&mut self, backward: bool) {
        let n = self.fills.len();
        self.step = if backward { (self.step + n - 1) % n } else { (self.step + 1) % n };
    }

    /// The finished text, and the caret's place in it. An unfilled tabstop
    /// gives its default or nothing, so no literal `$1` is left in a
    /// command about to run. The caret lands on `$0` if there is one, else
    /// at the end.
    pub fn accept(&self) -> (String, usize) {
        let mut out = String::new();
        let mut caret = None;
        for (i, chunk) in self.chunks.iter().enumerate() {
            out.push_str(chunk);
            let Some(&tabstop) = self.slots.get(i) else { continue };
            let fill = &self.fills[tabstop];
            out.push_str(if fill.is_empty() { self.defaults[tabstop].as_deref().unwrap_or("") } else { fill });
            // After what `$0` holds, as if typed by hand.
            if self.numbers[tabstop] == 0 && caret.is_none() {
                caret = Some(out.chars().count());
            }
        }
        let end = out.chars().count();
        (out, caret.unwrap_or(end))
    }
}

// A snippet spliced into a real buffer

// What a live snippet needs from its buffer. A snippet can span lines
// (a server's `fn ${1:name}() {\n    $0\n}`), so positions are
// `(line, col)`. The shell prompt has one line and folds newlines to
// spaces (see `is_multiline`).
pub trait SnippetHost {
    // Replace from `from` up to `to` with `text`, which may hold newlines
    // when `is_multiline`.
    fn replace_span(&mut self, from: (usize, usize), to: (usize, usize), text: &str);
    fn place_cursor(&mut self, line: usize, col: usize);
    // False for the one-line shell prompt. A multi-line snippet there has its
    // newlines folded to spaces rather than being refused, since `abbr` is
    // shared with the editor.
    fn is_multiline(&self) -> bool {
        true
    }
}

// Where `text` ends, given that it starts at `from`.
fn end_of(from: (usize, usize), text: &str) -> (usize, usize) {
    match text.rsplit_once('\n') {
        Some((before, last)) => (from.0 + before.matches('\n').count() + 1, last.chars().count()),
        None => (from.0, from.1 + text.chars().count()),
    }
}

// A char offset into `text` as a `(line, col)` delta applied to `from`.
fn offset_to_position(from: (usize, usize), text: &str, offset: usize) -> (usize, usize) {
    let prefix: String = text.chars().take(offset).collect();
    end_of(from, &prefix)
}

/// One place a tabstop appears, in the host buffer's own coordinates:
/// where it starts, where it ends, and whether it is the hole being
/// typed into.
pub type Hole = ((usize, usize), (usize, usize), bool);

// A snippet spliced into a buffer tentatively: the model, where its text
// sits, and what was there before. Invariant: the buffer from `start` to
// `end` is `rendered()`, and the cursor is where `snip.caret()` says.
// Nothing else touches that span meanwhile.
pub struct LiveSnippet {
    pub snip: Snippet,
    start: (usize, usize),
    end: (usize, usize),
    multiline: bool,
    // The abbreviation name this replaced, which `cancel` puts back.
    original: String,
}

impl LiveSnippet {
    // Replaces the abbreviation name at `[col, col + original.len())` with
    // the snippet's first rendering, caret in the first tabstop.
    pub fn start(snip: Snippet, line: usize, col: usize, original: String, host: &mut impl SnippetHost) -> LiveSnippet {
        let start = (line, col);
        let mut live = LiveSnippet { snip, start, end: end_of(start, &original), multiline: host.is_multiline(), original };
        live.sync(host);
        live
    }

    // The model's rendering, newlines folded to spaces for a one-line host.
    // Folding rather than dropping keeps the model's char offsets valid.
    fn shaped(&self, text: String) -> String {
        if self.multiline { text } else { text.replace('\n', " ") }
    }

    // Rewrites the span from the model and puts the cursor in the active
    // tabstop, after every edit, so the two can't drift.
    pub fn sync(&mut self, host: &mut impl SnippetHost) {
        let rendered = self.shaped(self.snip.render());
        host.replace_span(self.start, self.end, &rendered);
        self.end = end_of(self.start, &rendered);
        let (line, col) = offset_to_position(self.start, &rendered, self.snip.caret());
        host.place_cursor(line, col);
    }

    // Makes the snippet ordinary text, as `Snippet::accept` describes.
    pub fn accept(self, host: &mut impl SnippetHost) {
        let (text, caret) = self.snip.accept();
        let text = self.shaped(text);
        host.replace_span(self.start, self.end, &text);
        let (line, col) = offset_to_position(self.start, &text, caret);
        host.place_cursor(line, col);
    }

    pub fn cancel(self, host: &mut impl SnippetHost) {
        host.replace_span(self.start, self.end, &self.original);
        let (line, col) = end_of(self.start, &self.original);
        host.place_cursor(line, col);
    }

    // Every slot as `((line, col), (line, col), is_active)`, for the
    // renderers. A repeated tabstop gives an entry per place, all active
    // together.
    pub fn holes(&self) -> Vec<Hole> {
        let active = self.snip.active();
        let rendered = self.shaped(self.snip.render());
        self.snip
            .spans()
            .into_iter()
            .map(|(start, end, tabstop)| {
                (offset_to_position(self.start, &rendered, start), offset_to_position(self.start, &rendered, end), tabstop == active)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snip(expansion: &str) -> Snippet {
        Snippet::parse(expansion).expect("expansion has tabstops")
    }

    fn type_in(s: &mut Snippet, text: &str) {
        for c in text.chars() {
            s.type_char(c);
        }
    }

    #[test]
    fn an_expansion_without_tabstops_is_not_a_snippet() {
        assert_eq!(Snippet::parse("git checkout"), None);
        assert!(!has_tabstops("git checkout"));
        assert!(has_tabstops("git checkout $1"));
    }

    #[test]
    fn a_fresh_snippet_shows_the_tokens_it_was_written_with() {
        let s = snip("bar -x $1 -y $2 | qoo");
        assert_eq!(s.render(), "bar -x $1 -y $2 | qoo");
        assert_eq!(s.tabstop_count(), 2);
    }

    #[test]
    fn typing_replaces_the_active_tabstop_only() {
        let mut s = snip("bar -x $1 -y $2 | qoo");
        type_in(&mut s, "one");
        assert_eq!(s.render(), "bar -x one -y $2 | qoo");
        s.advance(false);
        type_in(&mut s, "two");
        assert_eq!(s.render(), "bar -x one -y two | qoo");
    }

    #[test]
    fn deleting_a_fill_back_to_nothing_restores_the_token() {
        let mut s = snip("echo $1");
        s.type_char('h');
        assert_eq!(s.render(), "echo h");
        assert!(s.backspace());
        assert_eq!(s.render(), "echo $1", "an emptied tabstop is unfilled again, not a hole");
        assert!(!s.backspace(), "there is nothing left to delete inside the tabstop");
    }

    // An untouched hole isn't necessarily empty.
    #[test]
    fn a_default_is_what_an_untouched_tabstop_shows_and_leaves_behind() {
        let mut s = snip("git commit -m \"${1:message}\"");
        assert_eq!(s.render(), "git commit -m \"message\"");
        assert_eq!(s.accept().0, "git commit -m \"message\"");
        type_in(&mut s, "fix");
        assert_eq!(s.render(), "git commit -m \"fix\"", "typing replaces the default rather than appending to it");
        assert_eq!(s.accept().0, "git commit -m \"fix\"");
        assert!(s.backspace() && s.backspace() && s.backspace());
        assert_eq!(s.render(), "git commit -m \"message\"", "and deleting it all brings the default back");
    }

    #[test]
    fn accepting_drops_untouched_tabstops_that_had_no_default() {
        let mut s = snip("bar -x $1 -y $2 | qoo");
        type_in(&mut s, "one");
        assert_eq!(s.accept().0, "bar -x one -y  | qoo", "the untouched `$2` leaves nothing behind");
    }

    #[test]
    fn spans_and_caret_track_what_is_actually_rendered() {
        let mut s = snip("ab$1cd$2");
        assert_eq!(s.spans(), vec![(2, 4, 0), (6, 8, 1)]);
        // Unfilled, the caret sits before the token it will replace.
        assert_eq!(s.caret(), 2);
        s.type_char('X');
        assert_eq!(s.render(), "abXcd$2");
        assert_eq!(s.spans(), vec![(2, 3, 0), (5, 7, 1)]);
        assert_eq!(s.caret(), 3, "filled: the caret follows what was typed");
    }

    #[test]
    fn tab_order_wraps_in_both_directions() {
        let mut s = snip("$1 $2 $3");
        assert_eq!(s.active(), 0);
        s.advance(true);
        assert_eq!(s.active(), 2, "back from the first wraps to the last");
        s.advance(false);
        assert_eq!(s.active(), 0, "and forward from the last wraps to the first");
    }

    // Visit order is ascending by number.
    #[test]
    fn tabstops_are_visited_in_number_order_not_text_order() {
        let mut s = snip("bar -x $2 -y $1");
        assert_eq!(s.render(), "bar -x $2 -y $1");
        assert!(!s.at_last());
        type_in(&mut s, "why");
        assert_eq!(s.render(), "bar -x $2 -y why", "`$1` is filled first even though it comes second");
        s.advance(false);
        assert!(s.at_last());
        type_in(&mut s, "ex");
        assert_eq!(s.accept().0, "bar -x ex -y why");
    }

    // `$0` is visited after every numbered stop, however it was written.
    #[test]
    fn the_final_tabstop_is_visited_last_and_takes_the_caret() {
        let mut s = snip("if $1; then\n\t$0\nfi");
        assert_eq!(s.render(), "if $1; then\n\t\nfi", "`$0` draws as the empty position it is");
        assert_eq!(s.active(), 0);
        s.advance(false);
        assert!(s.at_last(), "`$0` is last however early it appears");
        let (text, caret) = s.accept();
        assert_eq!(text, "if ; then\n\t\nfi");
        assert_eq!(caret, "if ; then\n\t".chars().count(), "the caret lands where `$0` was");
        // Typing into it leaves the caret past the text.
        type_in(&mut s, "x");
        let (text, caret) = s.accept();
        assert_eq!(text, "if ; then\n\tx\nfi");
        assert_eq!(caret, "if ; then\n\tx".chars().count());
    }

    #[test]
    fn a_repeated_number_is_one_tabstop_filled_in_every_place() {
        let mut s = snip("${1:name}: $2 = $1.into()");
        assert_eq!(s.tabstop_count(), 2);
        assert_eq!(s.render(), "${1:name}: $2 = ${1:name}.into()".replace("${1:name}", "name"));
        type_in(&mut s, "x");
        assert_eq!(s.render(), "x: $2 = x.into()");
        assert_eq!(s.spans(), vec![(0, 1, 0), (3, 5, 1), (8, 9, 0)]);
        assert_eq!(s.caret(), 1, "the caret goes to the first of the places");
        // Both places are the active hole, so both are marked.
        let active: Vec<bool> = s.spans().iter().map(|(_, _, tab)| *tab == s.active()).collect();
        assert_eq!(active, vec![true, false, true]);
    }

    #[test]
    fn a_default_wins_wherever_it_was_written() {
        let s = snip("$1 and ${1:this}");
        assert_eq!(s.render(), "this and this", "the spelling that said what it meant is the one that shows");
    }

    #[test]
    fn escapes_cover_the_three_characters_that_mean_something() {
        // `\$` is a literal dollar.
        assert_eq!(Snippet::parse("echo \\$1"), None);
        assert_eq!(flatten("echo \\$1"), "echo $1");
        assert_eq!(flatten("cost: \\${1}"), "cost: ${1}");
        assert_eq!(flatten("a \\\\ b"), "a \\ b");
        // Other backslashes stay.
        assert_eq!(flatten(r"find . -name '*.rs' \;"), r"find . -name '*.rs' \;");
        assert_eq!(flatten(r"printf 'a\nb'"), r"printf 'a\nb'");
    }

    #[test]
    fn what_is_not_a_tabstop_is_text() {
        // `$` before a non-digit is a shell variable.
        assert_eq!(Snippet::parse("echo $HOME"), None);
        assert_eq!(Snippet::parse("cost is 100% $"), None);
        // `%` is ordinary.
        assert_eq!(Snippet::parse("100%% sure"), None);
        assert_eq!(flatten("100%% sure"), "100%% sure");
        // Unterminated, and a choice (`${1|a,b|}`), which isn't
        // implemented: both stay text.
        assert_eq!(Snippet::parse("${1:oops"), None);
        assert_eq!(Snippet::parse("${1|a,b|}"), None);
        // `${1}` alone is a tabstop.
        assert_eq!(snip("${1}").render(), "$1");
    }

    // `${1:${2:x}}` keeps the text it would show.
    #[test]
    fn a_nested_default_becomes_the_text_it_would_have_shown() {
        let s = snip("fn ${1:${2:name}}()");
        assert_eq!(s.tabstop_count(), 1, "the inner one is part of the outer's default, not its own hole");
        assert_eq!(s.render(), "fn name()");
    }

    #[test]
    fn a_tabstop_can_sit_at_either_end_of_the_expansion() {
        let mut s = snip("$1");
        assert_eq!(s.render(), "$1");
        assert_eq!(s.spans(), vec![(0, 2, 0)]);
        s.type_char('z');
        assert_eq!(s.accept().0, "z");
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

        // Everything but bash, via extglob.
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
        // On either side of the mode flag.
        assert_eq!(take_lang_flag(&strs(&["--lang=rust", "-a", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), Some("rust".into())));
        assert_eq!(take_lang_flag(&strs(&["-a", "--lang=rust", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), Some("rust".into())));
        // With no mode flag (`abbr NAME EXPANSION` adds).
        assert_eq!(take_lang_flag(&strs(&["--lang=rust", "foo", "bar"])), (strs(&["foo", "bar"]), Some("rust".into())));
        // After NAME it's expansion text.
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "echo", "--lang=x"])), (strs(&["-a", "foo", "echo", "--lang=x"]), None));
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), None));
    }
}
