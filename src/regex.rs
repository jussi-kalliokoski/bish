// Small regex engine for `[[ str =~ pattern ]]`, and for the editor's
// `/`, `:s` and `:g`. Supports the common ERE subset: literals, `.`,
// `*`/`+`/`?` (greedy), `{n,m}` intervals, `^`/`$` anchors,
// `[...]`/`[^...]` character classes (with `-` ranges), `|` alternation,
// and `(...)` grouping, including capture extraction for BASH_REMATCH. No
// external crate -- hand-rolled recursive-descent parser plus a Pike VM,
// same spirit as glob.rs.

#[derive(Debug, Clone)]
enum Re {
    Char(char),
    Any,
    Class(Vec<(char, char)>, bool),
    Star(Box<Re>),
    Plus(Box<Re>),
    Opt(Box<Re>),
    // `{n}`, `{n,}` and `{n,m}` -- ERE's interval quantifier, with
    // `None` for an open upper bound. It was not parsed at all, and a
    // `{` is an ordinary character to this engine, so `a{2}` matched
    // the four-character text `a{2}` and did not match `aa`. Both
    // directions silently: no pattern was ever rejected for it.
    Repeat(Box<Re>, usize, Option<usize>),
    Concat(Vec<Re>),
    Alt(Vec<Re>),
    Start,
    End,
    /// `\b` and `\B`: a position where a word character is on exactly
    /// one side, or on both/neither. Consumes nothing -- which is what
    /// made it worth having a matcher that can express "a predicate on
    /// where you are" at all.
    WordBoundary(bool),
    /// `\<` and `\>`: the two halves of `\b`, told apart by which side
    /// the word is on. GNU's own extension, and bash has it.
    WordStart,
    WordEnd,
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
            // A `{` that does not open a well-formed interval is an
            // ordinary character, which is what POSIX leaves it as and
            // what every `awk '{print}'` pattern relies on.
            Some('{') => match self.parse_interval() {
                Some((min, max)) => Re::Repeat(Box::new(atom), min, max),
                None => atom,
            },
            _ => atom,
        }
    }

    /// `{n}`, `{n,}`, `{n,m}` -- consumed only if the whole thing is
    /// there and well formed, so the parser can fall back to treating
    /// the brace as text.
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
            // `[[:space:]]`: a class *name*, whose own `]` does not end
            // the bracket expression around it. Expanded into the same
            // ranges this class is already made of -- see
            // glob::posix_class_ranges.
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

/// What a backslash escape means.
///
/// GNU's set, because that is what bash has: `\b`/`\B` and `\<`/`\>`
/// for word boundaries, `\w`/`\W`/`\s`/`\S` for the two classes
/// people reach for, and `` \` ``/`\'` for the ends of the text. Every
/// other escape is the character itself, which is what makes `\.` a
/// dot and `\\` a backslash.
///
/// `\d` is deliberately *not* here. It is Perl's, not POSIX's and not
/// GNU's: bash matches `\d+` against `ab123` and finds nothing, because
/// to glibc that pattern is one or more letter `d`s. Adding it would
/// have been a silent disagreement with the shell this one is measured
/// against -- the probe is what said so, not the guess that preceded
/// it.
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
        // A word character is alphanumeric or an underscore -- glibc's
        // own `[_[:alnum:]]`, which is why `a_1` is one word to both.
        'w' => class(b"alnum", false, &[('_', '_')]),
        'W' => class(b"alnum", true, &[('_', '_')]),
        's' => class(b"space", false, &[]),
        'S' => class(b"space", true, &[]),
        // The ends of the *text*, which in this engine is what `^` and
        // `$` already mean: there is no multi-line mode for them to
        // differ from.
        '`' => Re::Start,
        '\'' => Re::End,
        _ => Re::Char(c),
    }
}

/// Alphanumeric or underscore -- what sits on either side of a `\b`.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn parse(pattern: &str) -> (Re, usize) {
    let mut p = ReParser::new(pattern);
    let re = p.parse_alt();
    (re, p.group_count)
}

// One slot per capture group, indexed by group number (index 0 is the
// whole match). `None` for a group the winning match never entered --
// distinct from a group that matched emptily, which every caller that
// reports captures flattens back to the empty string.
type CapSlots = Vec<Option<(usize, usize)>>;

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

// ---------------------------------------------------------------------
// The matcher
// ---------------------------------------------------------------------
//
// A Pike VM: the pattern is compiled to a small instruction list, and
// matching advances a *set* of threads through it one input character
// at a time. A thread is a program counter and its capture slots;
// two threads at the same instruction are indistinguishable from that
// point on, so only one is kept. The live set is therefore never larger
// than the program, and the whole match is one pass -- O(text x
// pattern), with no path explored twice.
//
// It replaced a backtracker, which is the same shape as the parser and
// is much the easier thing to write: try a branch, and on failure come
// back and try the next. The trouble is that failure can be discovered
// arbitrarily far away, so the branches multiply -- `^(a+)+$` against
// thirty a's and a `b` explored every one of the 2^30 ways to split
// them and never came back. In the editor, where this same engine backs
// `/`, `:s` and `:g`, that is the process gone.
//
// The other thing that changed with it is *which* match is found.
// POSIX wants the longest match at the leftmost position it can start,
// and a backtracker naturally gives the first one its branch order
// reaches instead: `(a|ab)` against `abc` matched `a` here and matches
// `ab` in bash. A thread set has no branch order to prefer -- every
// alternative is live at once -- so the rule falls out of letting every
// thread run to the end and keeping the longest. Branch order survives
// only as a tie-break between matches of the same start and the same
// length, where all it decides is what the capture groups say -- see
// `Threads::add` and the one divergence from bash that leaves.

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

/// An interval's upper bound is expanded into that many copies of the
/// body, so a large one costs program size. POSIX puts the portable
/// limit at 255 and this is the same number: past it the pattern is
/// compiled as though the bound were open, which matches more rather
/// than refusing outright.
const MAX_REPEAT_EXPANSION: usize = 255;

// Slots 0 and 1 are the whole match, so the program brackets the
// pattern with them exactly as a `Group` brackets its own.
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
        // Each branch jumps past the rest once it is done; the last
        // needs no jump, and every jump is patched to the end.
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
        // `{n,m}` is n copies, then m - n optional ones; `{n,}` is n
        // copies and a star. There is no counting instruction, and
        // adding one would need a counter per thread -- expansion keeps
        // a thread a program counter and its captures, which is what
        // makes the set dedupable at all.
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

/// A thread's captures. Cloned when a thread forks, which is why they
/// are a plain `Rc`-free vector: the slot count is twice the group
/// count plus two, and patterns are small.
type Slots = Vec<Option<usize>>;

/// Two threads at the same instruction are indistinguishable from
/// there on, so only one survives, and the one that got there first
/// wins: the epsilon walk below is depth-first in the pattern's own
/// branch order, so "first" means the branch written first.
///
/// That is a tie-break, and it only ever runs after leftmost-longest
/// has already decided the extent of the match -- it settles nothing
/// but which spelling of an equally long match the capture groups
/// report. Bash's own answer there is glibc's, which is not the POSIX
/// rule (POSIX would hand each subexpression, left to right, the
/// longest it can take): `(|a)(a|)` against `a` fills the first group
/// in bash and the second here. See `bashdiff::DIVERGENCES`.
struct Threads {
    /// The threads waiting on an input character, in priority order.
    dense: Vec<(usize, Slots)>,
    /// Which instructions this walk has already reached, counting the
    /// ones that consume nothing. That is also what keeps the epsilon
    /// walk finite over `(a*)*` and friends.
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

    /// Follows every instruction that consumes no input -- jumps,
    /// splits, saves and the assertions -- and adds the pcs that do.
    /// Depth-first and in priority order, so the list stays ordered by
    /// the branch order the pattern was written in.
    ///
    /// The whole text is here, not just the position, because an
    /// assertion is a question about where `pos` *is*: `\b` needs the
    /// character on either side of it. Consuming nothing and looking at
    /// its neighbours is the entire shape of a zero-width assertion,
    /// and it is why they belong in this walk rather than in the step
    /// that eats a character.
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

/// A compiled pattern: the instructions, and how many capture groups
/// their `Save`s refer to.
struct Program {
    insts: Vec<Inst>,
    group_count: usize,
}

impl Program {
    fn new(re: &Re, group_count: usize) -> Program {
        Program { insts: compile(re), group_count }
    }
}

/// Runs the program over `chars` from `from`, returning the match as
/// `(start, end, slots)`.
///
/// `anchored` says whether the match must begin at `from` exactly.
/// Unanchored, the search is still one pass: a fresh thread is started
/// at every position until something matches, so the whole set of
/// candidate starts advances together rather than the pattern being
/// re-run from each one in turn. That is the difference between
/// O(text x pattern) and O(text^2 x pattern), and on a 2000-character
/// line the editor feels it.
///
/// Which match wins is POSIX's rule, leftmost-longest: an earlier start
/// always beats a later one, and among matches sharing a start the
/// longest wins. So a thread reaching `Match` does not end the search;
/// it records a candidate, and the rest keep running. Equal starts and
/// equal lengths keep the earlier thread -- the one the pattern's own
/// branch order reached first -- and that is what settles the capture
/// slots.
fn run(prog: &Program, chars: &[char], from: usize, anchored: bool, ignore_case: bool) -> Option<(usize, usize, CapSlots)> {
    let insts = &prog.insts;
    let empty: Slots = vec![None; (prog.group_count + 1) * 2];
    let mut clist = Threads::new(insts.len());
    let mut nlist = Threads::new(insts.len());
    let mut best: Option<(usize, usize, Slots)> = None;

    let mut at = from;
    loop {
        // A new start goes in behind the threads already running, which
        // is what makes an earlier start outrank it. Once anything has
        // matched, no later start could win, so none is added.
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
            // A thread that began after the best match so far can only
            // lose; carrying it costs the rest of the line.
            if best.as_ref().is_some_and(|(bs, _, _)| slots[0].is_some_and(|s| s > *bs)) {
                continue;
            }
            let consumes = match &insts[*pc] {
                Inst::Char(x) => chars_equal(c, *x, ignore_case),
                Inst::Any => true,
                // The input character is folded, not the class: folding
                // the *ranges* would mean flipping their endpoints,
                // which is only right for a range that stays inside one
                // script's own alphabet. Testing the input's other
                // cases against the ranges as written is exact --
                // `[a-z]` matches `A` because `a` is in it, and `[A-Z]`
                // matches `a` for the mirror reason.
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

// A group the winning match never entered is reported as the empty
// string rather than as absent -- BASH_REMATCH's own shape, which the
// editor's `:s` backreferences follow.
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
        Regex { prog: Program::new(&re, group_count), ignore_case }
    }

    /// Does this pattern match starting at exactly `pos`? Returns the
    /// match's end position (char index) if so. Unlike `find_at`, doesn't
    /// scan forward -- a caller wanting "the next match at or after some
    /// position" wants `find_at`; a caller that already knows the exact
    /// start it cares about (e.g. scanning backward one position at a
    /// time) wants this instead.
    pub fn match_at(&self, chars: &[char], pos: usize) -> Option<usize> {
        run(&self.prog, chars, pos, true, self.ignore_case).map(|(_, end, _)| end)
    }

    /// Leftmost match starting at or after `from` (char index into
    /// `chars`), ERE-style. `None` if nothing matches anywhere in
    /// `chars[from..]`.
    pub fn find_at(&self, chars: &[char], from: usize) -> Option<(usize, usize)> {
        run(&self.prog, chars, from, false, self.ignore_case).map(|(start, end, _)| (start, end))
    }

    /// Same search as `find_at`, but also returns captures in
    /// `match_captures`'s own shape: index 0 is the whole match, indices
    /// 1..=N are each group (empty string, not absent, for one the
    /// winning path never entered). For a caller (`:s`'s own
    /// substitution loop, repl.rs) that needs backreferences/`&` in a
    /// replacement rather than just knowing a match happened.
    pub fn find_at_with_captures(&self, chars: &[char], from: usize) -> Option<(usize, usize, Vec<String>)> {
        let (start, end, caps) = run(&self.prog, chars, from, false, self.ignore_case)?;
        Some((start, end, capture_texts(chars, start, end, caps)))
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
    // The pattern every backtracker dies on: thirty ways to split each
    // run of a's, and a `b` at the end so none of them works. The old
    // engine explored 2^30 of them; a thread set has one state per
    // instruction and cannot.
    #[test]
    fn a_pattern_that_used_to_take_forever_finishes() {
        let text: String = std::iter::repeat_n('a', 2000).collect();
        let re = Regex::compile("^(a+)+$", false);
        assert!(re.find_at(&chars(&text), 0).is_some());
        assert!(re.find_at(&chars(&(text + "b")), 0).is_none());
    }

    // POSIX picks the longest match at the leftmost start, not the
    // first one the pattern's branch order reaches. `(a|ab)` against
    // `abc` is the discriminator: a backtracker says `a`, bash and this
    // say `ab`.
    #[test]
    fn the_longest_match_wins_at_the_leftmost_start() {
        fn whole(pattern: &str, text: &str) -> String {
            match_captures(text, pattern, false).unwrap().remove(0)
        }
        assert_eq!(whole("(a|ab)", "abc"), "ab");
        assert_eq!(whole("x|xy", "xy"), "xy");
        assert_eq!(whole("(|a)", "abc"), "a");
        assert_eq!(whole("ab|abc", "abc"), "abc");
        // Leftmost still outranks longest: the shorter match starts first.
        assert_eq!(match_captures("xabc", "b|abc", false), Some(vec!["abc".to_string()]));
        assert_eq!(whole("a|bcd", "abcd"), "a");
    }

    // Ties in length keep the earlier branch, which is what decides
    // where the capture groups land when both spellings match the same
    // text.
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
        // Past the expansion limit the bound is treated as open, which
        // matches more rather than refusing.
        assert!(m(&format!("a{{1,{}}}", MAX_REPEAT_EXPANSION + 10), "aaa", false));
    }

    // An unanchored search advances every candidate start together
    // rather than restarting the pattern at each one, so a long line
    // costs one pass and not one per character.
    #[test]
    fn a_search_that_fails_over_a_long_line_stays_linear() {
        let text: String = std::iter::repeat_n('a', 20_000).collect();
        let re = Regex::compile("(a|aa)*b", false);
        assert!(re.find_at(&chars(&text), 0).is_none());
    }
    // Assertions consume nothing and ask about where they are, which is
    // the one thing a matcher that only ever eats characters cannot
    // express. The set is GNU's, because that is what bash has.
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

    // `\d` is Perl's, not GNU's. bash finds nothing in `ab123` for
    // `\d+`, because to glibc that pattern is one or more letter `d`s,
    // and this agrees with the shell it is measured against rather than
    // with the habit.
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

    // Every other escape is still the character itself, which is what
    // makes `\.` a dot rather than any character.
    #[test]
    fn an_escape_with_no_meaning_is_the_character() {
        assert!(m("a\\.b", "a.b", false));
        assert!(!m("a\\.b", "axb", false));
        assert!(m("a\\nb", "anb", false), "\\n is the letter n here, as it is in bash");
    }

    // An assertion inside a repetition still has to hold every time
    // round, which is the case a matcher that treated it as a character
    // would get wrong in the other direction.
    #[test]
    fn an_assertion_inside_a_repetition_holds_each_time() {
        assert_eq!(match_captures("one two", "(\\w+\\s*)+", false), Some(vec!["one two".to_string(), "two".to_string()]));
        assert!(!m("^(\\bx)+$", "xx", false), "only the first x begins a word");
    }
}
