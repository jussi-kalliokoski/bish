// A small regex engine for `[[ str =~ pattern ]]` and the editor's `/`,
// `:s` and `:g`: the common ERE subset (literals, `.`, greedy `*` `+` `?`,
// `{n,m}`, `^` `$`, bracket classes with ranges, `|`, groups with captures
// for BASH_REMATCH). A recursive-descent parser and a Pike VM.

#[derive(Debug, Clone)]
enum Re {
    Char(char),
    Any,
    Class(Vec<(char, char)>, bool),
    Star(Box<Re>),
    Plus(Box<Re>),
    Opt(Box<Re>),
    // `{n}`, `{n,}`, `{n,m}`, ERE's interval quantifier; `None` is an open
    // upper bound.
    Repeat(Box<Re>, usize, Option<usize>),
    Concat(Vec<Re>),
    Alt(Vec<Re>),
    Start,
    End,
    /// `\b` and `\B`: a word character on exactly one side, or on both or
    /// neither. Consumes nothing.
    WordBoundary(bool),
    /// `\<` and `\>`: `\b`'s halves, by which side the word is on; GNU's
    /// extension, which bash has.
    WordStart,
    WordEnd,
    // Numbered by opening paren, left to right, whatever the nesting:
    // `((a)(b))` is group 1 `(a)(b)`, 2 `(a)`, 3 `(b)`.
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
            // A `{` not opening a well-formed interval is a character, as
            // POSIX leaves it and `awk '{print}'` patterns rely on.
            Some('{') => match self.parse_interval() {
                Some((min, max)) => Re::Repeat(Box::new(atom), min, max),
                None => atom,
            },
            _ => atom,
        }
    }

    /// `{n}`, `{n,}`, `{n,m}`, consumed only when whole and well formed, so
    /// the brace can fall back to text.
    fn parse_interval(&mut self) -> Option<(usize, Option<usize>)> {
        let start = self.pos;
        self.pos += 1;
        let min = self.parse_interval_number()?;
        let max = match self.peek() {
            Some('}') => Some(min),
            Some(',') => {
                self.pos += 1;
                match self.peek() {
                    Some('}') => None,
                    _ => Some(self.parse_interval_number()?),
                }
            }
            _ => {
                self.pos = start;
                return None;
            }
        };
        if self.peek() != Some('}') || max.is_some_and(|m| m < min) {
            self.pos = start;
            return None;
        }
        self.pos += 1;
        Some((min, max))
    }

    fn parse_interval_number(&mut self) -> Option<usize> {
        let from = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        match self.pos > from {
            true => self.chars[from..self.pos].iter().collect::<String>().parse().ok(),
            false => {
                self.pos = from;
                None
            }
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
                escaped_atom(c)
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
            // `[[:space:]]`: a class name, whose `]` doesn't end the
            // bracket around it, expanded to its ranges
            // (`glob::posix_class_ranges`).
            if c == '[' {
                let rest: String = self.chars[self.pos..].iter().collect();
                if let Some((name, len)) = crate::glob::posix_class_at(rest.as_bytes()) {
                    if let Some(rs) = crate::glob::posix_class_ranges(name) {
                        ranges.extend(rs.iter().map(|(lo, hi)| (*lo as char, *hi as char)));
                    }
                    self.pos += len;
                    first = false;
                    continue;
                }
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

/// What a backslash escape means, GNU's set as bash has: `\b`/`\B` and
/// `\<`/`\>` for word boundaries, `\w`/`\W`/`\s`/`\S`, and `` \` ``/`\'`
/// for the text's ends. Any other escape is the character itself (`\.`,
/// `\\`). No `\d`: it's Perl's, and bash finds nothing in `ab123` for
/// `\d+`, which to glibc is one or more `d`s.
fn escaped_atom(c: char) -> Re {
    let class = |name: &[u8], negated: bool, extra: &[(char, char)]| {
        let mut ranges: Vec<(char, char)> =
            crate::glob::posix_class_ranges(name).unwrap_or(&[]).iter().map(|(lo, hi)| (*lo as char, *hi as char)).collect();
        ranges.extend_from_slice(extra);
        Re::Class(ranges, negated)
    };
    match c {
        'b' => Re::WordBoundary(true),
        'B' => Re::WordBoundary(false),
        '<' => Re::WordStart,
        '>' => Re::WordEnd,
        // A word character is alphanumeric or `_`, glibc's `[_[:alnum:]]`.
        'w' => class(b"alnum", false, &[('_', '_')]),
        'W' => class(b"alnum", true, &[('_', '_')]),
        's' => class(b"space", false, &[]),
        'S' => class(b"space", true, &[]),
        // The text's ends, which `^` and `$` already are: there's no
        // multi-line mode.
        '`' => Re::Start,
        '\'' => Re::End,
        _ => Re::Char(c),
    }
}

/// Alphanumeric or `_`: what sits beside a `\b`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn parse(pattern: &str) -> (Re, usize) {
    let mut p = ReParser::new(pattern);
    let re = p.parse_alt();
    (re, p.group_count)
}

// One slot per capture group by number (0 is the whole match). `None`
// for a group the winning match never entered, which callers report as
// empty.
type CapSlots = Vec<Option<(usize, usize)>>;

// The other cases of `c`, for case-insensitive comparison. Simple
// folding, not full: a mapping to several characters is skipped, so `ß`
// doesn't match `SS`; this matcher consumes one input character per
// pattern character, as editor searches do. The tables are std's.
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

// Escapes every metacharacter, so the result matches only the literal
// text. For a quoted part of `=~`'s pattern, which bash matches
// literally.
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

// The matcher, a Pike VM: the pattern compiles to a small instruction
// list, and matching advances a set of threads (a program counter and
// captures) one input character at a time. Two threads at the same
// instruction are interchangeable, so only one is kept; the set never
// exceeds the program, and matching is one pass, O(text x pattern). A
// backtracker would explode on patterns like `^(a+)+$` against many
// `a`s and a `b`.
//
// The match found is POSIX's: the longest at the leftmost start. Every
// alternative is live at once, so every thread runs to the end and the
// longest is kept (`(a|ab)` against `abc` is `ab`, as in bash). Branch
// order only breaks ties between matches of the same start and length,
// deciding the captures; see `Threads::add` for the one divergence from
// bash.

#[derive(Debug, Clone)]
enum Inst {
    Char(char),
    Any,
    Class(Vec<(char, char)>, bool),
    AtStart,
    AtEnd,
    /// True for `\b`, false for `\B`.
    AtWordBoundary(bool),
    AtWordStart,
    AtWordEnd,
    /// Try `0` first; `1` is the lower-priority alternative.
    Split(usize, usize),
    Jmp(usize),
    /// Record the current position in capture slot `0` -- group `n`
    /// occupies slots `2n` and `2n + 1`.
    Save(usize),
    Match,
}

/// An interval's upper bound expands to that many copies of the body,
/// costing program size. POSIX's portable limit is 255; past it the bound
/// compiles as open, matching more rather than refusing.
const MAX_REPEAT_EXPANSION: usize = 255;

// Slots 0 and 1 are the whole match, so the program brackets the pattern
// with them, as a `Group` does its own.
fn compile(re: &Re) -> Vec<Inst> {
    let mut prog = vec![Inst::Save(0)];
    emit(re, &mut prog);
    prog.push(Inst::Save(1));
    prog.push(Inst::Match);
    prog
}

fn emit(re: &Re, prog: &mut Vec<Inst>) {
    match re {
        Re::Char(c) => prog.push(Inst::Char(*c)),
        Re::Any => prog.push(Inst::Any),
        Re::Class(ranges, negated) => prog.push(Inst::Class(ranges.clone(), *negated)),
        Re::Start => prog.push(Inst::AtStart),
        Re::End => prog.push(Inst::AtEnd),
        Re::WordBoundary(want) => prog.push(Inst::AtWordBoundary(*want)),
        Re::WordStart => prog.push(Inst::AtWordStart),
        Re::WordEnd => prog.push(Inst::AtWordEnd),
        Re::Concat(parts) => {
            for p in parts {
                emit(p, prog);
            }
        }
        // Each branch jumps past the rest when done; the last needs none, and
        // every jump is patched to the end.
        Re::Alt(branches) => {
            let mut jumps = Vec::new();
            for (i, b) in branches.iter().enumerate() {
                if i + 1 == branches.len() {
                    emit(b, prog);
                    break;
                }
                let split = prog.len();
                prog.push(Inst::Jmp(0));
                emit(b, prog);
                jumps.push(prog.len());
                prog.push(Inst::Jmp(0));
                let next = prog.len();
                prog[split] = Inst::Split(split + 1, next);
            }
            let end = prog.len();
            for j in jumps {
                prog[j] = Inst::Jmp(end);
            }
        }
        Re::Star(inner) => {
            let split = prog.len();
            prog.push(Inst::Jmp(0));
            emit(inner, prog);
            prog.push(Inst::Jmp(split));
            let end = prog.len();
            prog[split] = Inst::Split(split + 1, end);
        }
        Re::Plus(inner) => {
            let body = prog.len();
            emit(inner, prog);
            let split = prog.len();
            prog.push(Inst::Jmp(0));
            let end = prog.len();
            prog[split] = Inst::Split(body, end);
        }
        Re::Opt(inner) => {
            let split = prog.len();
            prog.push(Inst::Jmp(0));
            emit(inner, prog);
            let end = prog.len();
            prog[split] = Inst::Split(split + 1, end);
        }
        // `{n,m}` is n copies then m - n optional ones; `{n,}` n copies and
        // a star. A counting instruction would need a counter per thread,
        // and a thread that's just a pc and captures is what makes the set
        // dedupable.
        Re::Repeat(inner, min, max) => {
            for _ in 0..*min {
                emit(inner, prog);
            }
            match max {
                None => emit(&Re::Star(inner.clone()), prog),
                Some(m) if m - min > MAX_REPEAT_EXPANSION => emit(&Re::Star(inner.clone()), prog),
                Some(m) => {
                    let mut splits = Vec::new();
                    for _ in *min..*m {
                        splits.push(prog.len());
                        prog.push(Inst::Jmp(0));
                        emit(inner, prog);
                    }
                    let end = prog.len();
                    for s in splits {
                        prog[s] = Inst::Split(s + 1, end);
                    }
                }
            }
        }
        Re::Group(idx, inner) => {
            prog.push(Inst::Save(idx * 2));
            emit(inner, prog);
            prog.push(Inst::Save(idx * 2 + 1));
        }
    }
}

/// A thread's captures, cloned when a thread forks: twice the group count
/// plus two slots, and patterns are small.
type Slots = Vec<Option<usize>>;

/// Two threads at one instruction are interchangeable, so the first to
/// arrive survives; the epsilon walk is depth-first in branch order, so
/// first means written first. That's only a tie-break after
/// leftmost-longest decided the extent, settling which spelling the
/// captures report. bash's answer there is glibc's, not POSIX's rule:
/// `(|a)(a|)` against `a` fills the first group in bash and the second
/// here. See `bashdiff::DIVERGENCES`.
struct Threads {
    /// The threads waiting on an input character, in priority order.
    dense: Vec<(usize, Slots)>,
    /// Which instructions this walk reached, zero-width ones included, which
    /// also keeps the walk finite over `(a*)*`.
    on_list: Vec<bool>,
}

impl Threads {
    fn new(len: usize) -> Threads {
        Threads { dense: Vec::new(), on_list: vec![false; len] }
    }

    fn clear(&mut self) {
        self.dense.clear();
        self.on_list.iter_mut().for_each(|f| *f = false);
    }

    /// Follows every instruction that consumes nothing (jumps, splits,
    /// saves, assertions) and adds those that do, depth-first in priority
    /// order. It takes the whole text, since an assertion like `\b` asks
    /// about the characters around `pos`.
    fn add(&mut self, prog: &[Inst], pc: usize, chars: &[char], pos: usize, slots: &Slots) {
        if self.on_list[pc] {
            return;
        }
        self.on_list[pc] = true;
        let before = pos.checked_sub(1).and_then(|i| chars.get(i)).copied().is_some_and(is_word_char);
        let after = chars.get(pos).copied().is_some_and(is_word_char);
        match &prog[pc] {
            Inst::Jmp(to) => self.add(prog, *to, chars, pos, slots),
            Inst::Split(a, b) => {
                self.add(prog, *a, chars, pos, slots);
                self.add(prog, *b, chars, pos, slots);
            }
            Inst::Save(slot) => {
                let mut next = slots.clone();
                next[*slot] = Some(pos);
                self.add(prog, pc + 1, chars, pos, &next);
            }
            Inst::AtStart => {
                if pos == 0 {
                    self.add(prog, pc + 1, chars, pos, slots);
                }
            }
            Inst::AtEnd => {
                if pos == chars.len() {
                    self.add(prog, pc + 1, chars, pos, slots);
                }
            }
            Inst::AtWordBoundary(want) => {
                if (before != after) == *want {
                    self.add(prog, pc + 1, chars, pos, slots);
                }
            }
            Inst::AtWordStart => {
                if !before && after {
                    self.add(prog, pc + 1, chars, pos, slots);
                }
            }
            Inst::AtWordEnd => {
                if before && !after {
                    self.add(prog, pc + 1, chars, pos, slots);
                }
            }
            _ => self.dense.push((pc, slots.clone())),
        }
    }
}

/// A compiled pattern: instructions, and how many groups their `Save`s
/// refer to.
struct Program {
    insts: Vec<Inst>,
    group_count: usize,
}

impl Program {
    fn new(re: &Re, group_count: usize) -> Program {
        Program { insts: compile(re), group_count }
    }
}

/// Runs the program over `chars` from `from`, returning
/// `(start, end, slots)`. `anchored` requires starting at `from`.
/// Unanchored, a new thread starts at each position until something
/// matches, so all candidate starts advance together in one pass, not
/// O(text^2 x pattern). Leftmost-longest: an earlier start beats a later
/// one, and the longest wins among equal starts, so reaching `Match`
/// records a candidate and the rest run on; equal ones keep the earlier
/// thread, which settles the captures.
fn run(prog: &Program, chars: &[char], from: usize, anchored: bool, ignore_case: bool) -> Option<(usize, usize, CapSlots)> {
    let insts = &prog.insts;
    let empty: Slots = vec![None; (prog.group_count + 1) * 2];
    let mut clist = Threads::new(insts.len());
    let mut nlist = Threads::new(insts.len());
    let mut best: Option<(usize, usize, Slots)> = None;

    let mut at = from;
    loop {
        // A new start goes behind the running threads, so an earlier start
        // outranks it; once anything matched, no later start could win.
        if best.is_none() && (!anchored || at == from) {
            clist.add(insts, 0, chars, at, &empty);
        }
        for (pc, slots) in &clist.dense {
            if matches!(insts[*pc], Inst::Match) {
                let (start, end) = (slots[0].unwrap_or(from), slots[1].unwrap_or(at));
                if best.as_ref().is_none_or(|(bs, be, _)| start < *bs || (start == *bs && end > *be)) {
                    best = Some((start, end, slots.clone()));
                }
            }
        }
        if at >= chars.len() {
            break;
        }
        let c = chars[at];
        nlist.clear();
        for (pc, slots) in &clist.dense {
            // A thread begun after the best match can only lose.
            if best.as_ref().is_some_and(|(bs, _, _)| slots[0].is_some_and(|s| s > *bs)) {
                continue;
            }
            let consumes = match &insts[*pc] {
                Inst::Char(x) => chars_equal(c, *x, ignore_case),
                Inst::Any => true,
                // The input character is folded, not the class: folding
                // ranges would flip endpoints, right only within one
                // alphabet. Testing the input's cases against the ranges as
                // written is exact: `[a-z]` matches `A` because `a` is in
                // it.
                Inst::Class(ranges, negated) => {
                    let hit = in_ranges(ranges, c) || (ignore_case && case_variants(c).any(|v| in_ranges(ranges, v)));
                    hit != *negated
                }
                _ => false,
            };
            if consumes {
                nlist.add(insts, pc + 1, chars, at + 1, slots);
            }
        }
        std::mem::swap(&mut clist, &mut nlist);
        at += 1;
    }

    let (start, end, slots) = best?;
    let caps = (0..=prog.group_count)
        .map(|g| match (slots[g * 2], slots[g * 2 + 1]) {
            (Some(s), Some(e)) => Some((s, e)),
            _ => None,
        })
        .collect();
    Some((start, end, caps))
}

// A group the match never entered is the empty string, BASH_REMATCH's
// shape, which `:s` backreferences follow.
fn capture_texts(chars: &[char], start: usize, end: usize, caps: CapSlots) -> Vec<String> {
    let mut out = Vec::with_capacity(caps.len());
    out.push(chars[start..end].iter().collect());
    for slot in caps.into_iter().skip(1) {
        out.push(match slot {
            Some((s, e)) => chars[s..e].iter().collect(),
            None => String::new(),
        });
    }
    out
}

pub struct Regex {
    prog: Program,
    ignore_case: bool,
}

impl Regex {
    /// `ignore_case` is fixed at compile time, where callers know it (once
    /// per search). No shorthand without it, so a new search site can't
    /// silently default to case-sensitive.
    pub fn compile(pattern: &str, ignore_case: bool) -> Regex {
        let (re, group_count) = parse(pattern);
        Regex { prog: Program::new(&re, group_count), ignore_case }
    }

    /// Whether this pattern matches starting exactly at `pos`, returning the
    /// end. Doesn't scan forward, unlike `find_at`; for a caller that knows
    /// the start (scanning backward a position at a time).
    pub fn match_at(&self, chars: &[char], pos: usize) -> Option<usize> {
        run(&self.prog, chars, pos, true, self.ignore_case).map(|(_, end, _)| end)
    }

    /// The leftmost match at or after `from`, ERE-style, or `None`.
    pub fn find_at(&self, chars: &[char], from: usize) -> Option<(usize, usize)> {
        run(&self.prog, chars, from, false, self.ignore_case).map(|(start, end, _)| (start, end))
    }

    /// `find_at` with captures in `match_captures`' shape (0 the whole match,
    /// then each group, empty if never entered), for backreferences and `&`
    /// in `:s`.
    pub fn find_at_with_captures(&self, chars: &[char], from: usize) -> Option<(usize, usize, Vec<String>)> {
        let (start, end, caps) = run(&self.prog, chars, from, false, self.ignore_case)?;
        Some((start, end, capture_texts(chars, start, end, caps)))
    }
}

/// `[[ str =~ pattern ]]`: on a match anywhere in `str` (unanchored, like
/// ERE regexec, honouring `^` and `$`), BASH_REMATCH-style captures: 0
/// the whole match, then each group, empty if never entered, as in bash.
/// `ignore_case` is `shopt -s nocasematch`.
pub fn match_captures(text: &str, pattern: &str, ignore_case: bool) -> Option<Vec<String>> {
    let (re, group_count) = parse(pattern);
    let prog = Program::new(&re, group_count);
    let chars: Vec<char> = text.chars().collect();
    let (start, end, caps) = run(&prog, &chars, 0, false, ignore_case)?;
    Some(capture_texts(&chars, start, end, caps))
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

    // The input is folded, not the class.
    #[test]
    fn a_class_is_tested_against_the_inputs_other_cases() {
        assert!(m("^[a-z]", "Hello", true));
        assert!(m("^[A-Z]", "hello", true));
        assert!(!m("^[a-z]", "Hello", false));
        // A negated class agrees with itself: `H` is in `[a-z]` ignoring
        // case, so not in `[^a-z]`.
        assert!(!m("^[^a-z]", "Hello", true));
        assert!(m("^[^a-z]", "Hello", false));
        // A range not about letters is unaffected.
        assert!(m("^[0-9]", "5", true));
        assert!(!m("^[0-9]", "x", true));
    }

    #[test]
    fn folding_reaches_past_ascii_but_stays_simple() {
        assert!(m("äpfel", "ÄPFEL", true));
        assert!(m("ÄPFEL", "äpfel", true));
        // Simple folding only: `ß` isn't `SS`, which would take two input
        // positions for one pattern character. bash's `nocasematch` agrees.
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
        // Expands to more than one character, so declined (`case_variants`).
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
    // The pattern that kills a backtracker: every way to split thirty a's,
    // with a `b` so none works. A thread set has one state per instruction.
    #[test]
    fn a_pattern_that_used_to_take_forever_finishes() {
        let text: String = std::iter::repeat_n('a', 2000).collect();
        let re = Regex::compile("^(a+)+$", false);
        assert!(re.find_at(&chars(&text), 0).is_some());
        assert!(re.find_at(&chars(&(text + "b")), 0).is_none());
    }

    // Leftmost-longest: `(a|ab)` against `abc` is `ab`, as in bash, not the
    // first branch's `a`.
    #[test]
    fn the_longest_match_wins_at_the_leftmost_start() {
        fn whole(pattern: &str, text: &str) -> String {
            match_captures(text, pattern, false).unwrap().remove(0)
        }
        assert_eq!(whole("(a|ab)", "abc"), "ab");
        assert_eq!(whole("x|xy", "xy"), "xy");
        assert_eq!(whole("(|a)", "abc"), "a");
        assert_eq!(whole("ab|abc", "abc"), "abc");
        // Leftmost still beats longest.
        assert_eq!(match_captures("xabc", "b|abc", false), Some(vec!["abc".to_string()]));
        assert_eq!(whole("a|bcd", "abcd"), "a");
    }

    // Equal lengths keep the earlier branch, which decides where captures
    // land.
    #[test]
    fn branch_order_settles_the_captures_of_equally_long_matches() {
        assert_eq!(match_captures("ab", "(ab)|(ab)", false), Some(vec!["ab".into(), "ab".into(), String::new()]));
        assert_eq!(match_captures("aaa", "(a*)(a*)", false), Some(vec!["aaa".into(), "aaa".into(), String::new()]));
    }

    #[test]
    fn an_interval_repeats_its_body_that_many_times() {
        assert!(!m("a{2}", "a", false));
        assert!(m("a{2}", "aa", false));
        assert!(m("a{2,}", "aaaa", false));
        assert_eq!(match_captures("aaaa", "a{2,3}", false), Some(vec!["aaa".to_string()]));
        // Past the expansion limit the bound is open, matching more.
        assert!(m(&format!("a{{1,{}}}", MAX_REPEAT_EXPANSION + 10), "aaa", false));
    }

    // Unanchored search advances every start together: one pass per line.
    #[test]
    fn a_search_that_fails_over_a_long_line_stays_linear() {
        let text: String = std::iter::repeat_n('a', 20_000).collect();
        let re = Regex::compile("(a|aa)*b", false);
        assert!(re.find_at(&chars(&text), 0).is_none());
    }
    // Assertions consume nothing and ask about their position. GNU's set,
    // as bash has.
    #[test]
    fn word_boundaries_are_positions_not_characters() {
        assert!(m("\\bbar", "foo bar", false));
        assert!(!m("\\bbar", "foobar", false));
        assert!(m("foo\\b", "foo bar", false));
        assert!(m("\\Bar", "foo bar", false), "inside a word is where \\B holds");
        assert!(!m("\\Bfoo", "foo", false));
        assert!(m("\\<bar", "foo bar", false));
        assert!(!m("\\<ar", "foo bar", false));
        assert!(m("bar\\>", "foo bar", false));
        assert!(!m("ba\\>", "foo bar", false));
    }

    #[test]
    fn the_two_shorthand_classes_are_the_ones_glibc_has() {
        assert_eq!(match_captures("a_1 %", "\\w+", false), Some(vec!["a_1".to_string()]), "a word character is alphanumeric or an underscore");
        assert_eq!(match_captures("ab %", "\\W", false), Some(vec![" ".to_string()]));
        assert_eq!(match_captures("ab cd", "\\s", false), Some(vec![" ".to_string()]));
        assert_eq!(match_captures("  xy", "\\S+", false), Some(vec!["xy".to_string()]));
    }

    // `\d` is Perl's; bash finds nothing in `ab123` for `\d+`.
    #[test]
    fn a_perl_shorthand_this_engine_does_not_have_is_a_literal() {
        assert!(!m("\\d+", "ab123", false));
        assert!(m("\\d+", "add", false), "which is what a literal `d` matches");
    }

    #[test]
    fn the_ends_of_the_text_have_their_own_spelling_too() {
        assert!(m("\\`abc", "abc", false));
        assert!(!m("\\`bc", "abc", false));
        assert!(m("abc\\'", "abc", false));
        assert!(!m("ab\\'", "abc", false));
    }

    // Other escapes are the character itself: `\.` is a dot.
    #[test]
    fn an_escape_with_no_meaning_is_the_character() {
        assert!(m("a\\.b", "a.b", false));
        assert!(!m("a\\.b", "axb", false));
        assert!(m("a\\nb", "anb", false), "\\n is the letter n here, as it is in bash");
    }

    // An assertion inside a repetition must hold every time round.
    #[test]
    fn an_assertion_inside_a_repetition_holds_each_time() {
        assert_eq!(match_captures("one two", "(\\w+\\s*)+", false), Some(vec!["one two".to_string(), "two".to_string()]));
        assert!(!m("^(\\bx)+$", "xx", false), "only the first x begins a word");
    }
}
