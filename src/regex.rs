// Small backtracking regex engine for `[[ str =~ pattern ]]`. Supports the
// common ERE subset: literals, `.`, `*`/`+`/`?` (greedy), `^`/`$` anchors,
// `[...]`/`[^...]` character classes (with `-` ranges), `|` alternation,
// and `(...)` grouping, including capture extraction for BASH_REMATCH. No
// external crate -- hand-rolled recursive-descent parser plus
// continuation-passing backtracking matcher, same spirit as glob.rs.

use std::cell::RefCell;

#[derive(Debug, Clone)]
enum Re {
    Char(char),
    Any,
    Class(Vec<(char, char)>, bool),
    Star(Box<Re>),
    Plus(Box<Re>),
    Opt(Box<Re>),
    Concat(Vec<Re>),
    Alt(Vec<Re>),
    Start,
    End,
    // Numbered per ERE convention: by position of the opening paren, left
    // to right, regardless of nesting depth -- so `((a)(b))` is group 1 =
    // "(a)(b)" (the whole outer), group 2 = "(a)", group 3 = "(b)".
    Group(usize, Box<Re>),
}

struct ReParser<'a> {
    chars: Vec<char>,
    pos: usize,
    group_count: usize,
    _src: &'a str,
}

impl<'a> ReParser<'a> {
    fn new(src: &'a str) -> Self {
        ReParser { chars: src.chars().collect(), pos: 0, group_count: 0, _src: src }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn parse_alt(&mut self) -> Re {
        let mut branches = vec![self.parse_concat()];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_concat());
        }
        if branches.len() == 1 { branches.pop().unwrap() } else { Re::Alt(branches) }
    }

    fn parse_concat(&mut self) -> Re {
        let mut parts = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            parts.push(self.parse_repeat());
        }
        Re::Concat(parts)
    }

    fn parse_repeat(&mut self) -> Re {
        let atom = self.parse_atom();
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Re::Star(Box::new(atom))
            }
            Some('+') => {
                self.pos += 1;
                Re::Plus(Box::new(atom))
            }
            Some('?') => {
                self.pos += 1;
                Re::Opt(Box::new(atom))
            }
            _ => atom,
        }
    }

    fn parse_atom(&mut self) -> Re {
        match self.peek() {
            Some('^') => {
                self.pos += 1;
                Re::Start
            }
            Some('$') => {
                self.pos += 1;
                Re::End
            }
            Some('.') => {
                self.pos += 1;
                Re::Any
            }
            Some('(') => {
                self.pos += 1;
                self.group_count += 1;
                let idx = self.group_count;
                let inner = self.parse_alt();
                if self.peek() == Some(')') {
                    self.pos += 1;
                }
                Re::Group(idx, Box::new(inner))
            }
            Some('[') => {
                self.pos += 1;
                self.parse_class()
            }
            Some('\\') => {
                self.pos += 1;
                let c = self.peek().unwrap_or('\\');
                self.pos += 1;
                Re::Char(c)
            }
            Some(c) => {
                self.pos += 1;
                Re::Char(c)
            }
            None => Re::Concat(Vec::new()),
        }
    }

    fn parse_class(&mut self) -> Re {
        let negated = self.peek() == Some('^');
        if negated {
            self.pos += 1;
        }
        let mut ranges = Vec::new();
        let mut first = true;
        while let Some(c) = self.peek() {
            if c == ']' && !first {
                self.pos += 1;
                break;
            }
            first = false;
            self.pos += 1;
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|&c2| c2 != ']') {
                self.pos += 1;
                let hi = self.peek().unwrap_or(c);
                self.pos += 1;
                ranges.push((c, hi));
            } else {
                ranges.push((c, c));
            }
        }
        Re::Class(ranges, negated)
    }
}

fn parse(pattern: &str) -> (Re, usize) {
    let mut p = ReParser::new(pattern);
    let re = p.parse_alt();
    (re, p.group_count)
}

// One slot per capture group, indexed by group number (0 unused). A slot
// holds the (start, end) char-index range of that group's last successful
// match attempt on the path that ultimately succeeds -- backtracking
// requires each Group arm to restore its old value when a candidate range
// doesn't pan out, so a still-in-progress alternative doesn't see a stale
// capture from an abandoned attempt. Shared mutable state threaded through
// `&dyn Fn` continuations needs interior mutability; there's no way to
// thread a plain `&mut` through this CPS backtracking shape since multiple
// closures alias the same state at different points in the search tree.
// Where each capture group's `(start, end)` char range lands. `None` for
// a group the winning path never entered.
type CapSlots = Vec<Option<(usize, usize)>>;
type Captures = RefCell<CapSlots>;

// Everything the matcher carries besides the pattern and the input: the
// capture slots, and whether comparisons fold case.
struct MatchCtx {
    caps: Captures,
    ignore_case: bool,
}

// The other case(s) of `c`, for a case-insensitive comparison.
//
// **Simple** case folding, not full: a mapping that expands to more than
// one character is skipped, so `ß` does not match `SS` and `ﬁ` does not
// match `FI`. Full folding would have to change how many input
// characters a single pattern character consumes, which this matcher --
// one character, one position -- has no shape for, and every editor
// search this exists for treats it the same way.
//
// std's own `to_lowercase`/`to_uppercase` carry the Unicode tables. This
// module hand-rolls its engine because no crate may be taken; it does
// not hand-roll data the standard library already has.
fn case_variants(c: char) -> impl Iterator<Item = char> {
    let lower = one_char(c.to_lowercase().collect::<Vec<char>>());
    let upper = one_char(c.to_uppercase().collect::<Vec<char>>());
    [lower, upper].into_iter().flatten().filter(move |&v| v != c)
}

fn one_char(mapped: Vec<char>) -> Option<char> {
    match mapped.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

fn chars_equal(a: char, b: char, ignore_case: bool) -> bool {
    a == b || (ignore_case && case_variants(a).any(|v| v == b))
}

fn in_ranges(ranges: &[(char, char)], c: char) -> bool {
    ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

// Continuation-passing backtracking match: `k` says whether the rest of
// the overall pattern accepts starting at the given position. Needed so
// `a*a` (etc) can backtrack the star's greedy match when what follows
// doesn't fit.
fn match_re(re: &Re, s: &[char], pos: usize, ctx: &MatchCtx, k: &dyn Fn(usize) -> bool) -> bool {
    match re {
        Re::Char(c) => matches!(s.get(pos), Some(&x) if chars_equal(x, *c, ctx.ignore_case)) && k(pos + 1),
        Re::Any => pos < s.len() && k(pos + 1),
        // The input character is folded, not the class: folding the
        // *ranges* would mean flipping their endpoints, which is only
        // right for a range that stays inside one script's own
        // alphabet. Testing the input's other cases against the ranges
        // as written is exact -- `[a-z]` matches `A` because `a` is in
        // it, and `[A-Z]` matches `a` for the mirror reason.
        Re::Class(ranges, negated) => match s.get(pos) {
            Some(&c) => {
                let hit = in_ranges(ranges, c) || (ctx.ignore_case && case_variants(c).any(|v| in_ranges(ranges, v)));
                (hit != *negated) && k(pos + 1)
            }
            None => false,
        },
        Re::Start => pos == 0 && k(pos),
        Re::End => pos == s.len() && k(pos),
        Re::Concat(parts) => match_concat(parts, s, pos, ctx, k),
        Re::Alt(branches) => branches.iter().any(|b| match_re(b, s, pos, ctx, k)),
        Re::Star(inner) => match_star(inner, s, pos, 0, ctx, k),
        Re::Plus(inner) => match_re(inner, s, pos, ctx, &|p| match_star(inner, s, p, 0, ctx, k)),
        Re::Opt(inner) => match_re(inner, s, pos, ctx, k) || k(pos),
        Re::Group(idx, inner) => {
            let old = ctx.caps.borrow()[*idx];
            let result = match_re(inner, s, pos, ctx, &|p| {
                ctx.caps.borrow_mut()[*idx] = Some((pos, p));
                if k(p) {
                    true
                } else {
                    ctx.caps.borrow_mut()[*idx] = old;
                    false
                }
            });
            if !result {
                ctx.caps.borrow_mut()[*idx] = old;
            }
            result
        }
    }
}

fn match_concat(parts: &[Re], s: &[char], pos: usize, ctx: &MatchCtx, k: &dyn Fn(usize) -> bool) -> bool {
    match parts.split_first() {
        None => k(pos),
        Some((head, rest)) => match_re(head, s, pos, ctx, &|p| match_concat(rest, s, p, ctx, k)),
    }
}

// Greedy star: try consuming as many as possible first, backtracking down
// to zero. `depth` is just a runaway-recursion guard for pathological
// patterns on long inputs.
fn match_star(inner: &Re, s: &[char], pos: usize, depth: usize, ctx: &MatchCtx, k: &dyn Fn(usize) -> bool) -> bool {
    if depth < s.len() + 1 && match_re(inner, s, pos, ctx, &|p| p > pos && match_star(inner, s, p, depth + 1, ctx, k)) {
        return true;
    }
    k(pos)
}

// Escapes every character `parse` treats as a metacharacter, so the result
// -- fed back through `parse` -- matches only the literal input text. Used
// for `[[ ]]`'s `=~` when the pattern operand was quoted/escaped in the
// source (bash: quoting any part of a `=~` pattern forces that part to
// match literally instead of as regex).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// Shared by `Regex::match_at` and `match_captures` below: does `re` match
// starting at exactly `pos` (not scanning forward for the next viable
// start)? Returns the match's own end position plus its capture slots.
fn match_at_with_caps(re: &Re, group_count: usize, chars: &[char], pos: usize, ignore_case: bool) -> Option<(usize, CapSlots)> {
    let ctx = MatchCtx { caps: RefCell::new(vec![None; group_count + 1]), ignore_case };
    let end: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
    let matched = match_re(re, chars, pos, &ctx, &|p| {
        end.set(Some(p));
        true
    });
    matched.then(|| (end.get().unwrap(), ctx.caps.into_inner()))
}

/// A compiled pattern, reusable across many searches without reparsing --
/// for callers (line-editor `/`/`?` search, in particular) that run the
/// same pattern against many lines rather than matching it once against a
/// single string the way `match_captures` below does.
pub struct Regex {
    re: Re,
    group_count: usize,
    ignore_case: bool,
}

impl Regex {
    /// `ignore_case` rides on the compiled pattern rather than on each
    /// search because that is where every caller has the answer: an
    /// editor knows its `ignorecase` setting once per search, not once
    /// per line.
    ///
    /// There is deliberately no one-argument shorthand. Every caller
    /// answering the question explicitly is what stops a new search
    /// site from quietly defaulting to case-sensitive -- which is
    /// exactly how `/` stayed case-sensitive while `:s`, reaching the
    /// same buffer by a different route, did not.
    pub fn compile(pattern: &str, ignore_case: bool) -> Regex {
        let (re, group_count) = parse(pattern);
        Regex { re, group_count, ignore_case }
    }

    /// Does this pattern match starting at exactly `pos`? Returns the
    /// match's end position (char index) if so. Unlike `find_at`, doesn't
    /// scan forward -- a caller wanting "the next match at or after some
    /// position" wants `find_at`; a caller that already knows the exact
    /// start it cares about (e.g. scanning backward one position at a
    /// time) wants this instead.
    pub fn match_at(&self, chars: &[char], pos: usize) -> Option<usize> {
        match_at_with_caps(&self.re, self.group_count, chars, pos, self.ignore_case).map(|(end, _)| end)
    }

    /// Leftmost match starting at or after `from` (char index into
    /// `chars`), ERE-style. `None` if nothing matches anywhere in
    /// `chars[from..]`.
    pub fn find_at(&self, chars: &[char], from: usize) -> Option<(usize, usize)> {
        (from..=chars.len()).find_map(|start| self.match_at(chars, start).map(|end| (start, end)))
    }

    /// Same search as `find_at`, but also returns captures in
    /// `match_captures`'s own shape: index 0 is the whole match, indices
    /// 1..=N are each group (empty string, not absent, for one the
    /// winning path never entered). For a caller (`:s`'s own
    /// substitution loop, repl.rs) that needs backreferences/`&` in a
    /// replacement rather than just knowing a match happened.
    pub fn find_at_with_captures(&self, chars: &[char], from: usize) -> Option<(usize, usize, Vec<String>)> {
        for start in from..=chars.len() {
            if let Some((end, caps)) = match_at_with_caps(&self.re, self.group_count, chars, start, self.ignore_case) {
                let mut out = Vec::with_capacity(self.group_count + 1);
                out.push(chars[start..end].iter().collect());
                for slot in caps.into_iter().skip(1) {
                    out.push(match slot {
                        Some((s0, e0)) => chars[s0..e0].iter().collect(),
                        None => String::new(),
                    });
                }
                return Some((start, end, out));
            }
        }
        None
    }
}

// `[[ str =~ pattern ]]`: true (with Some(...)) if `pattern` matches
// anywhere in `str` (unanchored, like ERE regexec), honoring explicit
// `^`/`$` when present. On success also returns BASH_REMATCH-style
// captures: index 0 is the whole matched substring, indices 1..=N are each
// group's substring (empty string, not absent, for a group the winning
// match path never entered -- matches real bash's own behavior).
/// `ignore_case` is `shopt -s nocasematch`'s own question, answered by
/// the caller -- that option was registered with nothing to act on until
/// this engine could fold case at all.
pub fn match_captures(text: &str, pattern: &str, ignore_case: bool) -> Option<Vec<String>> {
    let (re, group_count) = parse(pattern);
    let chars: Vec<char> = text.chars().collect();
    for start in 0..=chars.len() {
        if let Some((end, caps)) = match_at_with_caps(&re, group_count, &chars, start, ignore_case) {
            let mut out = Vec::with_capacity(group_count + 1);
            out.push(chars[start..end].iter().collect());
            for slot in caps.into_iter().skip(1) {
                out.push(match slot {
                    Some((s0, e0)) => chars[s0..e0].iter().collect(),
                    None => String::new(),
                });
            }
            return Some(out);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pattern: &str, text: &str, ignore_case: bool) -> bool {
        let re = Regex::compile(pattern, ignore_case);
        re.find_at(&text.chars().collect::<Vec<char>>(), 0).is_some()
    }

    #[test]
    fn a_case_insensitive_pattern_matches_either_case() {
        assert!(!m("hello", "HELLO", false), "the default is unchanged");
        assert!(m("hello", "HELLO", true));
        assert!(m("HeLLo", "hello", true));
        assert!(m("h.llo", "HELLO", true), "and the metacharacters still mean what they meant");
        assert!(m("(ab)+c", "ABABC", true));
        assert!(m("^foo$", "FOO", true));
        assert!(!m("^foo$", "FOOD", true));
    }

    // The input is folded, not the class: folding the ranges would mean
    // flipping their endpoints, which is only right for a range that
    // stays inside one alphabet.
    #[test]
    fn a_class_is_tested_against_the_inputs_other_cases() {
        assert!(m("^[a-z]", "Hello", true));
        assert!(m("^[A-Z]", "hello", true));
        assert!(!m("^[a-z]", "Hello", false));
        // A negated class has to agree with itself: `H` is in `[a-z]`
        // case-insensitively, so it is not in `[^a-z]`.
        assert!(!m("^[^a-z]", "Hello", true));
        assert!(m("^[^a-z]", "Hello", false));
        // A range that is not about letters is untouched either way.
        assert!(m("^[0-9]", "5", true));
        assert!(!m("^[0-9]", "x", true));
    }

    #[test]
    fn folding_reaches_past_ascii_but_stays_simple() {
        assert!(m("äpfel", "ÄPFEL", true));
        assert!(m("ÄPFEL", "äpfel", true));
        // Simple folding only: `ß` does not become `SS`, because a
        // mapping that expands to two characters would have to consume
        // two input positions for one pattern character. Real bash's own
        // `nocasematch` says no here too.
        assert!(!m("STRASSE", "straße", true));
        assert!(m("stra\u{df}e", "STRA\u{df}E", true), "but it still matches itself");
    }

    #[test]
    fn captures_survive_folding() {
        let re = Regex::compile("(a+)(B+)", true);
        let chars: Vec<char> = "xxAAbbyy".chars().collect();
        let (start, end, caps) = re.find_at_with_captures(&chars, 0).expect("matches");
        assert_eq!((start, end), (2, 6));
        assert_eq!(caps, vec!["AAbb".to_string(), "AA".to_string(), "bb".to_string()]);
    }

    #[test]
    fn case_variants_skips_a_character_that_has_none() {
        assert_eq!(case_variants('5').collect::<Vec<char>>(), Vec::<char>::new());
        assert_eq!(case_variants('a').collect::<Vec<char>>(), vec!['A']);
        assert_eq!(case_variants('A').collect::<Vec<char>>(), vec!['a']);
        // Expands to more than one character, so simple folding declines
        // it -- see `case_variants`' own doc comment.
        assert_eq!(case_variants('\u{df}').collect::<Vec<char>>(), Vec::<char>::new());
    }

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn find_at_with_captures_finds_the_leftmost_match_and_its_groups() {
        let re = Regex::compile("(a+)(b)", false);
        let cs = chars("xx aaab yy");
        let (start, end, caps) = re.find_at_with_captures(&cs, 0).unwrap();
        assert_eq!((start, end), (3, 7));
        assert_eq!(caps, vec!["aaab".to_string(), "aaa".to_string(), "b".to_string()]);
    }

    #[test]
    fn find_at_with_captures_respects_from() {
        let re = Regex::compile("a", false);
        let cs = chars("a.a.a");
        let (start, _, _) = re.find_at_with_captures(&cs, 2).unwrap();
        assert_eq!(start, 2);
        assert!(re.find_at_with_captures(&cs, 5).is_none());
    }

    #[test]
    fn find_at_with_captures_none_when_nothing_matches() {
        let re = Regex::compile("z+", false);
        assert!(re.find_at_with_captures(&chars("abc"), 0).is_none());
    }

    #[test]
    fn find_at_with_captures_empty_string_for_a_group_that_never_matched() {
        let re = Regex::compile("(a)|(b)", false);
        let (_, _, caps) = re.find_at_with_captures(&chars("b"), 0).unwrap();
        assert_eq!(caps, vec!["b".to_string(), String::new(), "b".to_string()]);
    }
}
