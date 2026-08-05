// Small backtracking regex engine for `[[ str =~ pattern ]]`. Supports the
// common ERE subset: literals, `.`, `*`/`+`/`?` (greedy), `^`/`$` anchors,
// `[...]`/`[^...]` character classes (with `-` ranges), `|` alternation,
// and `(...)` grouping (structural only -- no captured backreferences,
// since bash's `[[ ]]` only exposes BASH_REMATCH[0] here, which is just
// "did it match" for our purposes; full capture-group extraction isn't
// implemented). No external crate -- hand-rolled recursive-descent parser
// plus continuation-passing backtracking matcher, same spirit as glob.rs.

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
}

struct ReParser<'a> {
    chars: Vec<char>,
    pos: usize,
    _src: &'a str,
}

impl<'a> ReParser<'a> {
    fn new(src: &'a str) -> Self {
        ReParser { chars: src.chars().collect(), pos: 0, _src: src }
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
        if branches.len() == 1 {
            branches.pop().unwrap()
        } else {
            Re::Alt(branches)
        }
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
                let inner = self.parse_alt();
                if self.peek() == Some(')') {
                    self.pos += 1;
                }
                inner
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

fn parse(pattern: &str) -> Re {
    ReParser::new(pattern).parse_alt()
}

// Continuation-passing backtracking match: `k` says whether the rest of
// the overall pattern accepts starting at the given position. Needed so
// `a*a` (etc) can backtrack the star's greedy match when what follows
// doesn't fit.
fn match_re(re: &Re, s: &[char], pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
    match re {
        Re::Char(c) => s.get(pos) == Some(c) && k(pos + 1),
        Re::Any => pos < s.len() && k(pos + 1),
        Re::Class(ranges, negated) => match s.get(pos) {
            Some(&c) => {
                let hit = ranges.iter().any(|&(lo, hi)| c >= lo && c <= hi);
                (hit != *negated) && k(pos + 1)
            }
            None => false,
        },
        Re::Start => pos == 0 && k(pos),
        Re::End => pos == s.len() && k(pos),
        Re::Concat(parts) => match_concat(parts, s, pos, k),
        Re::Alt(branches) => branches.iter().any(|b| match_re(b, s, pos, k)),
        Re::Star(inner) => match_star(inner, s, pos, 0, k),
        Re::Plus(inner) => match_re(inner, s, pos, &|p| match_star(inner, s, p, 0, k)),
        Re::Opt(inner) => match_re(inner, s, pos, k) || k(pos),
    }
}

fn match_concat(parts: &[Re], s: &[char], pos: usize, k: &dyn Fn(usize) -> bool) -> bool {
    match parts.split_first() {
        None => k(pos),
        Some((head, rest)) => match_re(head, s, pos, &|p| match_concat(rest, s, p, k)),
    }
}

// Greedy star: try consuming as many as possible first, backtracking down
// to zero. `depth` is just a runaway-recursion guard for pathological
// patterns on long inputs.
fn match_star(inner: &Re, s: &[char], pos: usize, depth: usize, k: &dyn Fn(usize) -> bool) -> bool {
    if depth < s.len() + 1 && match_re(inner, s, pos, &|p| p > pos && match_star(inner, s, p, depth + 1, k)) {
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

// `[[ str =~ pattern ]]`: true if `pattern` matches anywhere in `str`
// (unanchored, like ERE regexec), honoring explicit `^`/`$` when present.
pub fn is_match(text: &str, pattern: &str) -> bool {
    let re = parse(pattern);
    let chars: Vec<char> = text.chars().collect();
    for start in 0..=chars.len() {
        if match_re(&re, &chars, start, &|_| true) {
            return true;
        }
    }
    false
}
