// Snippet expansion for `abbr` and for a language server's own
// completions. An expansion containing tabstops -- `abbr -a gcm 'git
// commit -m "${1:message}"'` -- doesn't splice in as finished text: it
// splices in *tentatively*, with each tabstop shown as such and the
// caret parked in the first, and the user tabs between them and types
// into them before accepting.
//
// The syntax is LSP's, exactly:
//
//   $1, $2, ...   a tabstop, visited in ascending order
//   ${1:default}  a tabstop with text to fall back on
//   $0            where the caret lands once the snippet is accepted,
//                 visited last
//   \$, \}, \\     an escaped literal
//
// A number repeated is one tabstop appearing in several places, and
// typing fills all of them at once -- which is how `${1:name}: ${2:T} =
// $1.into()` reads the way it looks.
//
// It is LSP's syntax because bish now speaks LSP: a server's completion
// arrives in exactly this notation, and having `abbr` use a second one
// would mean two parsers, two sets of rules to remember, and snippets
// that cannot be pasted from a server's own docs. This replaced an
// earlier `%s`-and-a-trailing-integer-run spelling, which is why `%` is
// an ordinary character again.
//
// A pure model, no terminal anywhere in it: this module owns what a
// snippet *is* (the literal chunks around its tabstops, each tabstop's
// current fill and default, and which one is active) and what it
// renders to; editor.rs and fileeditor.rs own the keystrokes and the
// drawing. Same split browser.rs and hexedit.rs use one tier up, and
// the reason every rule below is unit-tested with no `read_line` in
// sight.

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
}

impl Abbr {
    pub fn new(name: &str, expansion: &str) -> Abbr {
        Abbr { name: name.to_string(), expansion: expansion.to_string(), lang: DEFAULT_LANG.to_string() }
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

// A live snippet: the expansion split around its tabstops, plus what
// has been typed into each so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snippet {
    // The literal text around the tabstop slots, always exactly one more
    // than there are slots -- `chunks[0]` precedes the first, `chunks[n]`
    // follows the last, and either can be empty. May contain newlines.
    chunks: Vec<String>,
    // Which tabstop each slot shows. Two slots naming the same tabstop
    // are the same hole in two places, which is what makes `$1` written
    // twice fill twice.
    slots: Vec<usize>,
    // Per tabstop, in visit order. `numbers` is what the user wrote
    // (`1`, `2`, ... and `0` last), kept only so an unfilled tabstop
    // with no default can render as the `$1` it came from.
    numbers: Vec<usize>,
    // Per tabstop: what has been typed. Empty means "still unfilled",
    // which is why deleting a fill back to nothing restores the default
    // on screen rather than leaving a hole -- there is no third state.
    fills: Vec<String>,
    // Per tabstop: the `${1:here}` text, if it had one.
    defaults: Vec<Option<String>>,
    // Which tabstop the caret is in, as an index into the three vectors
    // above -- which are already in visit order, so this is the step
    // number too.
    step: usize,
}

// One parsed piece of an expansion.
enum Piece {
    Literal(String),
    // (number, default)
    Tabstop(usize, Option<String>),
}

// Splits an expansion into literals and tabstops.
//
// Deliberately forgiving: anything that is not a well-formed tabstop is
// literal text, so an expansion that never meant to opt in (`echo $HOME`,
// `awk '{print $1}'` -- wait, that one *is* a tabstop, which is exactly
// why `\$1` exists) still says what it says rather than being rejected.
fn parse_pieces(text: &str) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            // Only the three characters that mean something here are
            // escapable. A shell abbreviation is full of ordinary
            // backslashes (`find . -name '*.rs' \;`), and having every
            // one of them silently eaten would be a worse surprise than
            // `\n` staying `\n`.
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
                    // A run of digits too long to be a number is text.
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

// Reads `${N}` or `${N:default}` starting at `i`. `None` when what
// follows isn't one, which makes it ordinary text.
//
// A default may itself contain braces and further tabstops -- LSP allows
// `${1:${2:inner}}` -- and the matching brace is found by depth. The
// default's own contents are then taken as *literal text*: a nested
// tabstop becomes the text it would have shown, which is the useful
// half of nesting without a tree in the model.
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
                // Unterminated: not a tabstop at all, so the `${` reads
                // as the two characters it is.
                return None;
            }
            let inner: String = chars[j + 1..k].iter().collect();
            Some((number, Some(flatten(&inner)), k + 1))
        }
        // `${1` followed by anything else is not a tabstop. Notably
        // `${1|a,b|}` (a choice) is left as text rather than silently
        // becoming a plain tabstop that lost its options.
        _ => None,
    }
}

/// An expansion with its tabstops reduced to their defaults and nothing
/// else -- what a snippet looks like once every hole is left alone.
///
/// This is what a caller with nowhere to put a caret wants: a
/// single-line preview, a `$0` that has to go somewhere, or the nested
/// default inside `${1:${2:x}}`.
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

/// Whether `text` has any tabstop at all -- the question `abbr` and the
/// completion menu both ask before deciding to splice tentatively
/// rather than as finished text.
pub fn has_tabstops(text: &str) -> bool {
    parse_pieces(text).iter().any(|p| matches!(p, Piece::Tabstop(..)))
}

impl Snippet {
    /// `None` when the expansion has no tabstops at all -- that's
    /// ordinary text, and the caller splices it as such.
    pub fn parse(expansion: &str) -> Option<Snippet> {
        let pieces = parse_pieces(expansion);
        // Visit order is ascending by number with `$0` last, which is
        // the protocol's own rule and also the only order that makes
        // `$0` mean "where the caret ends up".
        let mut numbers: Vec<usize> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Tabstop(n, _) => Some(*n),
                _ => None,
            })
            .collect();
        numbers.sort_unstable();
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
                    // The first spelling that gave a default wins, so
                    // `${1:name} ... $1` keeps the one that said what it
                    // meant.
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

    /// How many distinct tabstops there are -- holes to visit, not
    /// places they appear.
    pub fn tabstop_count(&self) -> usize {
        self.fills.len()
    }

    /// The tabstop the caret is in.
    pub fn active(&self) -> usize {
        self.step
    }

    /// True when the caret is on the last tabstop in visit order --
    /// where Enter accepts the snippet instead of advancing.
    pub fn at_last(&self) -> bool {
        self.step + 1 == self.fills.len()
    }

    // What an unfilled tabstop shows: its default, or the `$1` it was
    // written as. Seeing the token you typed is what makes an empty hole
    // legible as one.
    //
    // Except `$0`, which shows nothing: it is not a hole to type into,
    // it is where the caret ends up, and every editor with snippets
    // draws it as the empty position it is rather than as two
    // characters to delete.
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

    /// What the snippet currently looks like in the buffer: filled
    /// tabstops show what was typed, unfilled ones show their default or
    /// the token they came from. May contain newlines.
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

    /// Every slot's own `[start, end)` span within `render()`'s output
    /// plus which tabstop it belongs to, in text order and measured in
    /// chars.
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

    /// Where the real caret belongs within `render()`'s output: at the
    /// end of what has been typed into the active tabstop, which for an
    /// unfilled one is the *start* of what it shows -- so the token
    /// reads as something about to be replaced rather than as something
    /// already typed.
    ///
    /// A tabstop appearing more than once takes the caret to the first
    /// of its places.
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

    /// Deletes the last char of the active tabstop's fill. `false` when
    /// there was nothing to delete -- the caller's cue that Backspace
    /// had no meaning here, rather than silently eating the literal text
    /// around the snippet.
    pub fn backspace(&mut self) -> bool {
        let active = self.active();
        self.fills[active].pop().is_some()
    }

    /// Wraps in both directions, the same way the completion menu's own
    /// Tab/Shift-Tab cycling already does: with two tabstops, tabbing
    /// past the last one lands back on the first rather than dead-ending.
    pub fn advance(&mut self, backward: bool) {
        let n = self.fills.len();
        self.step = if backward { (self.step + n - 1) % n } else { (self.step + 1) % n };
    }

    /// The finished text, and where the caret goes in it.
    ///
    /// An unfilled tabstop contributes its default if it had one and
    /// nothing at all if it didn't, so a snippet accepted without
    /// touching every hole reads as if those arguments were simply never
    /// typed rather than leaving a literal `$1` in a command about to
    /// run. The caret lands on `$0` when the expansion named one, and
    /// after everything otherwise.
    pub fn accept(&self) -> (String, usize) {
        let mut out = String::new();
        let mut caret = None;
        for (i, chunk) in self.chunks.iter().enumerate() {
            out.push_str(chunk);
            let Some(&tabstop) = self.slots.get(i) else { continue };
            let fill = &self.fills[tabstop];
            out.push_str(if fill.is_empty() { self.defaults[tabstop].as_deref().unwrap_or("") } else { fill });
            // After whatever `$0` ended up holding, not before it: a
            // hole typed into leaves the caret past what was typed,
            // exactly as typing it by hand would.
            if self.numbers[tabstop] == 0 && caret.is_none() {
                caret = Some(out.chars().count());
            }
        }
        let end = out.chars().count();
        (out, caret.unwrap_or(end))
    }
}

// ---------------------------------------------------------------------
// A snippet spliced into a real buffer
// ---------------------------------------------------------------------

// The one thing a live snippet needs from whatever buffer it's spliced
// into.
//
// A snippet may be several lines: a server's own completion routinely is
// (`fn ${1:name}() {\n    $0\n}`), and an `abbr` written with a quoted
// newline can be. So the span is `(line, col)` at both ends, which the
// file editor's `TextBuffer` already splices natively -- and which the
// shell prompt, being one line, folds to spaces (see `is_multiline`).
pub trait SnippetHost {
    // Replace everything from `from` up to (not including) `to` with
    // `text`, which may contain newlines when `is_multiline`.
    fn replace_span(&mut self, from: (usize, usize), to: (usize, usize), text: &str);
    fn place_cursor(&mut self, line: usize, col: usize);
    // False for a buffer with exactly one line -- the shell prompt.
    // A multi-line snippet expanded there has its newlines folded to
    // spaces rather than being refused: `abbr` is shared between the
    // prompt and the editor, and one that only worked in one of them
    // would be a worse answer than one that reads a little flatter at
    // the prompt.
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

// A snippet that is currently spliced into a buffer, tentatively: the
// model plus where its rendered text sits and what was there before it.
//
// The invariant every method preserves: the buffer from `start` to `end`
// is exactly `rendered()`, and the cursor is wherever `snip.caret()`
// says. Nothing else should touch that span while this is alive.
pub struct LiveSnippet {
    pub snip: Snippet,
    start: (usize, usize),
    end: (usize, usize),
    multiline: bool,
    // The abbreviation name this replaced -- what `cancel` puts back.
    original: String,
}

impl LiveSnippet {
    // Replaces the abbreviation name at `[col, col + original.len())` of
    // `line` with the snippet's first rendering, caret in the first
    // tabstop.
    pub fn start(snip: Snippet, line: usize, col: usize, original: String, host: &mut impl SnippetHost) -> LiveSnippet {
        let start = (line, col);
        let mut live = LiveSnippet { snip, start, end: end_of(start, &original), multiline: host.is_multiline(), original };
        live.sync(host);
        live
    }

    // What actually goes in the buffer: the model's rendering, with
    // newlines folded to spaces for a host that has only one line.
    // Folding rather than dropping keeps every char offset the model
    // computed valid, which is what lets `spans`/`caret` stay
    // one-dimensional.
    fn shaped(&self, text: String) -> String {
        if self.multiline { text } else { text.replace('\n', " ") }
    }

    // Rewrites the span from the model and parks the cursor in the
    // active tabstop. Called after every edit to the model, so the two
    // can't drift.
    pub fn sync(&mut self, host: &mut impl SnippetHost) {
        let rendered = self.shaped(self.snip.render());
        host.replace_span(self.start, self.end, &rendered);
        self.end = end_of(self.start, &rendered);
        let (line, col) = offset_to_position(self.start, &rendered, self.snip.caret());
        host.place_cursor(line, col);
    }

    // Turns the tentative snippet into ordinary text: an unfilled
    // tabstop contributes its default or nothing at all, and the cursor
    // lands on `$0` if the expansion named one and after everything
    // otherwise -- exactly where it would be had the whole thing just
    // been typed out by hand.
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

    pub fn line(&self) -> usize {
        self.start.0
    }

    // Every slot as `((line, col), (line, col), is_active)` -- what each
    // of the two renderers turns into its own highlighting. A tabstop
    // written more than once yields one entry per place, all of them
    // active together, which is what makes a mirrored hole read as one
    // thing in two spots.
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

    // The whole point of `${1:default}`: an untouched hole is not
    // necessarily an empty one.
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
        // Unfilled: the caret sits *before* the token it will replace.
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

    // Visit order is the numbers ascending, which is what replaced the
    // old trailing-integer-run spelling: the order is written into the
    // expansion itself now.
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

    // `$0` is where the caret ends up, and it is visited after every
    // numbered stop however it was written.
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
        // And typing into it leaves the caret past what was typed.
        type_in(&mut s, "x");
        let (text, caret) = s.accept();
        assert_eq!(text, "if ; then\n\tx\nfi");
        assert_eq!(caret, "if ; then\n\tx".chars().count());
    }

    // The same number twice is one hole in two places -- the shape that
    // makes `${1:name}: ${2:T} = $1.into()` read the way it looks.
    #[test]
    fn a_repeated_number_is_one_tabstop_filled_in_every_place() {
        let mut s = snip("${1:name}: $2 = $1.into()");
        assert_eq!(s.tabstop_count(), 2);
        assert_eq!(s.render(), "${1:name}: $2 = ${1:name}.into()".replace("${1:name}", "name"));
        type_in(&mut s, "x");
        assert_eq!(s.render(), "x: $2 = x.into()");
        assert_eq!(s.spans(), vec![(0, 1, 0), (3, 5, 1), (8, 9, 0)]);
        assert_eq!(s.caret(), 1, "the caret goes to the first of the places");
        // Both places are the active hole, so both get marked.
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
        // `\$` is a literal dollar, so an expansion about money or shell
        // variables can say so.
        assert_eq!(Snippet::parse("echo \\$1"), None);
        assert_eq!(flatten("echo \\$1"), "echo $1");
        assert_eq!(flatten("cost: \\${1}"), "cost: ${1}");
        assert_eq!(flatten("a \\\\ b"), "a \\ b");
        // Everything else keeps its backslash: a shell abbreviation is
        // full of ordinary ones.
        assert_eq!(flatten(r"find . -name '*.rs' \;"), r"find . -name '*.rs' \;");
        assert_eq!(flatten(r"printf 'a\nb'"), r"printf 'a\nb'");
    }

    #[test]
    fn what_is_not_a_tabstop_is_text() {
        // A `$` before a non-digit is a shell variable, not a hole.
        assert_eq!(Snippet::parse("echo $HOME"), None);
        assert_eq!(Snippet::parse("cost is 100% $"), None);
        // `%` carries no meaning at all any more.
        assert_eq!(Snippet::parse("100%% sure"), None);
        assert_eq!(flatten("100%% sure"), "100%% sure");
        // Unterminated, and a choice (`${1|a,b|}`) bish does not
        // implement: both stay exactly what they say rather than
        // silently becoming a plain hole that lost its options.
        assert_eq!(Snippet::parse("${1:oops"), None);
        assert_eq!(Snippet::parse("${1|a,b|}"), None);
        // But `${1}` on its own is one.
        assert_eq!(snip("${1}").render(), "$1");
    }

    // LSP allows `${1:${2:x}}`; bish keeps the useful half -- the text
    // it would have shown -- without a tree in the model.
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
        // After the NAME it is ordinary expansion text.
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "echo", "--lang=x"])), (strs(&["-a", "foo", "echo", "--lang=x"]), None));
        assert_eq!(take_lang_flag(&strs(&["-a", "foo", "bar"])), (strs(&["-a", "foo", "bar"]), None));
    }

}
