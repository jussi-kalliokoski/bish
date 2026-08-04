use crate::lexer::{Chunk, Lexer, Tok};

#[derive(Debug, Clone)]
pub struct Word {
    pub chunks: Vec<Chunk>,
    pub globbable: bool,
}

#[derive(Debug, Clone)]
pub enum Redirect {
    In(Word),
    Out { word: Word, append: bool },
    Err { word: Word, append: bool },
    Both { word: Word, append: bool },
    DupErrToOut,
    HereString(Word),
    HereDoc(Word),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignMode {
    Set,
    Append,
}

#[derive(Debug, Clone)]
pub struct SimpleCommand {
    pub assigns: Vec<(String, AssignMode, Word)>,
    // `name=(word word ...)` / `name+=(word word ...)` array literals --
    // kept separate from `assigns` since array values don't fit the scalar
    // Word model.
    pub array_assigns: Vec<(String, AssignMode, Vec<Word>)>,
    // `name[index]=value` -- index is raw text (an arithmetic expression,
    // evaluated at assignment time), kept separate since it targets one
    // array element rather than the whole variable.
    pub index_assigns: Vec<(String, String, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

// Compound commands carry their own trailing redirects (e.g. `done < file`,
// `{ ...; } > file`), parsed right after the closing keyword/brace. Not yet
// applied at exec time (see exec.rs) -- parsed now so the grammar is right
// and wiring it up later doesn't require another parser pass.
#[derive(Debug, Clone)]
pub enum Command {
    Simple(SimpleCommand),
    If {
        branches: Vec<(Program, Program)>,
        else_branch: Option<Program>,
        redirects: Vec<Redirect>,
    },
    While {
        cond: Program,
        body: Program,
        until: bool,
        redirects: Vec<Redirect>,
    },
    For {
        var: String,
        // `None` means the `in ...` clause was omitted entirely (bash
        // iterates "$@" in that case); `Some(vec)` is an explicit list,
        // which may itself be empty (`for x in; do ...` -- zero iterations).
        words: Option<Vec<Word>>,
        body: Program,
        redirects: Vec<Redirect>,
    },
    CFor {
        init: String,
        cond: String,
        step: String,
        body: Program,
        redirects: Vec<Redirect>,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, Program)>,
        redirects: Vec<Redirect>,
    },
    Group(Program, Vec<Redirect>),
    FuncDef { name: String, body: Box<Command> },
    // Raw, not-yet-parsed source text of a (...) subshell -- tokenized and
    // parsed lazily, recursively, when it actually runs (see exec.rs).
    Subshell(String, Vec<Redirect>),
    // Standalone ((expr)) arithmetic command.
    Arith(String, Vec<Redirect>),
    // `[[ expr ]]`. Parsed as a flat token stream rather than folded into
    // an expression tree here -- exec.rs's evaluator does the actual
    // precedence handling (NOT > binary tests > AND > OR), same
    // deferred-evaluation spirit as Subshell/Arith.
    Test(Vec<TestAtom>, Vec<Redirect>),
}

#[derive(Debug, Clone)]
pub enum TestAtom {
    Word(Word),
    And,
    Or,
    Not,
    // A parenthesized sub-expression, e.g. `( a == b )` inside `[[ ]]`.
    Group(Vec<TestAtom>),
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<Command>,
    pub negate: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Combinator {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Combinator, Pipeline)>,
}

#[derive(Debug, Clone, Copy)]
pub enum Sep {
    Seq,
    Background,
}

#[derive(Debug, Clone)]
pub struct ListItem {
    pub and_or: AndOr,
    pub sep: Sep,
}

pub type Program = Vec<ListItem>;

pub struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    pub fn new(toks: Vec<Tok>) -> Self {
        Parser { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: Tok) -> Result<(), String> {
        match self.advance() {
            Some(t) if t == want => Ok(()),
            other => Err(format!("expected {:?}, got {:?}", want, other)),
        }
    }

    fn at_any(&self, stops: &[Tok]) -> bool {
        match self.peek() {
            Some(t) => stops.contains(t),
            None => false,
        }
    }

    pub fn parse_program(&mut self) -> Result<Program, String> {
        let prog = self.parse_list_until(&[])?;
        if let Some(other) = self.peek() {
            return Err(format!("unexpected token: {:?}", other));
        }
        Ok(prog)
    }

    // Parses a list of ListItems until the next token (after skipping
    // separators) matches one of `stops`, or (if `stops` is empty) until
    // end of input. Does not consume the stop token.
    fn parse_list_until(&mut self, stops: &[Tok]) -> Result<Program, String> {
        let mut items = Vec::new();
        self.skip_terminators();
        while !self.at_any(stops) {
            if self.peek().is_none() {
                if stops.is_empty() {
                    break;
                }
                return Err(format!("unexpected end of input, expected one of {:?}", stops));
            }
            let and_or = self.parse_and_or()?;
            let sep = match self.peek() {
                Some(Tok::Amp) => {
                    self.advance();
                    Sep::Background
                }
                Some(Tok::Semi) | Some(Tok::Newline) => {
                    self.advance();
                    Sep::Seq
                }
                _ => Sep::Seq,
            };
            items.push(ListItem { and_or, sep });
            self.skip_terminators();
        }
        Ok(items)
    }

    fn skip_terminators(&mut self) {
        while matches!(self.peek(), Some(Tok::Semi) | Some(Tok::Newline)) {
            self.advance();
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Tok::Newline)) {
            self.advance();
        }
    }

    fn parse_and_or(&mut self) -> Result<AndOr, String> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.advance();
                    self.skip_newlines();
                    rest.push((Combinator::And, self.parse_pipeline()?));
                }
                Some(Tok::Or) => {
                    self.advance();
                    self.skip_newlines();
                    rest.push((Combinator::Or, self.parse_pipeline()?));
                }
                _ => break,
            }
        }
        Ok(AndOr { first, rest })
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, String> {
        let negate = if let Some(Tok::Word(chunks, _)) = self.peek() {
            if matches!(chunks.as_slice(), [Chunk::Str(s)] if s == "!") {
                self.advance();
                true
            } else {
                false
            }
        } else {
            false
        };
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, negate })
    }

    fn parse_command(&mut self) -> Result<Command, String> {
        match self.peek() {
            Some(Tok::KwIf) => self.parse_if(),
            Some(Tok::KwWhile) => self.parse_while(false),
            Some(Tok::KwUntil) => self.parse_while(true),
            Some(Tok::KwFor) => self.parse_for(),
            Some(Tok::KwCase) => self.parse_case(),
            Some(Tok::LBrace) => self.parse_group(),
            Some(Tok::KwFunction) => self.parse_function_kw(),
            Some(Tok::Word(_, _)) if self.looks_like_func_def() => self.parse_function_paren(),
            Some(Tok::Subshell(_)) => {
                let raw = match self.advance() {
                    Some(Tok::Subshell(raw)) => raw,
                    _ => unreachable!(),
                };
                let redirects = self.parse_trailing_redirects()?;
                Ok(Command::Subshell(raw, redirects))
            }
            Some(Tok::Arith(_)) => {
                let raw = match self.advance() {
                    Some(Tok::Arith(raw)) => raw,
                    _ => unreachable!(),
                };
                let redirects = self.parse_trailing_redirects()?;
                Ok(Command::Arith(raw, redirects))
            }
            Some(Tok::KwLBracket2) => self.parse_double_bracket(),
            _ => Ok(Command::Simple(self.parse_simple_command()?)),
        }
    }

    // `[[ expr ]]`. Collects a flat TestAtom stream up to the matching
    // `]]`, treating `&&`/`||`/a bare `!` word as test-expression operators
    // instead of the shell-level combinators/pipeline-negation they'd
    // normally be -- this is what actually keeps `[[ a && b ]]` from being
    // split into two separate commands by the outer AndOr grammar.
    fn parse_double_bracket(&mut self) -> Result<Command, String> {
        self.advance(); // KwLBracket2
        let atoms = self.parse_test_atoms()?;
        match self.advance() {
            Some(Tok::KwRBracket2) => {}
            other => return Err(format!("expected ']]', got {:?}", other)),
        }
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::Test(atoms, redirects))
    }

    fn parse_test_atoms(&mut self) -> Result<Vec<TestAtom>, String> {
        let mut atoms = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::KwRBracket2) | None => break,
                Some(Tok::And) => {
                    self.advance();
                    atoms.push(TestAtom::And);
                }
                Some(Tok::Or) => {
                    self.advance();
                    atoms.push(TestAtom::Or);
                }
                Some(Tok::Subshell(_)) => {
                    let raw = match self.advance() {
                        Some(Tok::Subshell(raw)) => raw,
                        _ => unreachable!(),
                    };
                    let inner_toks = Lexer::new(&raw).tokenize()?;
                    let mut inner = Parser::new(inner_toks);
                    let group = inner.parse_test_atoms()?;
                    atoms.push(TestAtom::Group(group));
                }
                Some(Tok::Word(chunks, plain)) if *plain && matches!(chunks.as_slice(), [Chunk::Str(s)] if s == "!") =>
                {
                    self.advance();
                    atoms.push(TestAtom::Not);
                }
                Some(Tok::Word(_, _)) => {
                    let w = self.expect_word()?;
                    atoms.push(TestAtom::Word(w));
                }
                Some(Tok::Newline) => {
                    self.advance();
                }
                other => return Err(format!("unexpected token in '[[ ]]': {:?}", other)),
            }
        }
        Ok(atoms)
    }

    fn looks_like_func_def(&self) -> bool {
        if let Some(Tok::Word(chunks, _)) = self.toks.get(self.pos) {
            if word_to_plain_name(chunks).is_some() {
                return matches!(self.toks.get(self.pos + 1), Some(Tok::Subshell(raw)) if raw.is_empty());
            }
        }
        false
    }

    fn parse_function_paren(&mut self) -> Result<Command, String> {
        let name = match self.advance() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks).unwrap(),
            _ => unreachable!(),
        };
        self.advance(); // empty Subshell("") standing in for `()`
        self.skip_terminators();
        let body = self.parse_command()?;
        Ok(Command::FuncDef { name, body: Box::new(body) })
    }

    fn parse_function_kw(&mut self) -> Result<Command, String> {
        self.advance(); // KwFunction
        let name = match self.advance() {
            Some(Tok::Word(chunks, _)) => {
                word_to_plain_name(&chunks).ok_or_else(|| "expected a plain function name".to_string())?
            }
            other => return Err(format!("expected function name, got {:?}", other)),
        };
        if matches!(self.peek(), Some(Tok::Subshell(raw)) if raw.is_empty()) {
            self.advance();
        }
        self.skip_terminators();
        let body = self.parse_command()?;
        Ok(Command::FuncDef { name, body: Box::new(body) })
    }

    fn parse_if(&mut self) -> Result<Command, String> {
        self.advance(); // KwIf
        let mut branches = Vec::new();

        let cond = self.parse_list_until(&[Tok::KwThen])?;
        self.expect(Tok::KwThen)?;
        let body = self.parse_list_until(&[Tok::KwElif, Tok::KwElse, Tok::KwFi])?;
        branches.push((cond, body));

        loop {
            match self.peek() {
                Some(Tok::KwElif) => {
                    self.advance();
                    let c = self.parse_list_until(&[Tok::KwThen])?;
                    self.expect(Tok::KwThen)?;
                    let b = self.parse_list_until(&[Tok::KwElif, Tok::KwElse, Tok::KwFi])?;
                    branches.push((c, b));
                }
                Some(Tok::KwElse) => {
                    self.advance();
                    let else_body = self.parse_list_until(&[Tok::KwFi])?;
                    self.expect(Tok::KwFi)?;
                    let redirects = self.parse_trailing_redirects()?;
                    return Ok(Command::If { branches, else_branch: Some(else_body), redirects });
                }
                Some(Tok::KwFi) => {
                    self.advance();
                    let redirects = self.parse_trailing_redirects()?;
                    return Ok(Command::If { branches, else_branch: None, redirects });
                }
                other => return Err(format!("expected elif/else/fi, got {:?}", other)),
            }
        }
    }

    fn parse_while(&mut self, until: bool) -> Result<Command, String> {
        self.advance(); // KwWhile / KwUntil
        let cond = self.parse_list_until(&[Tok::KwDo])?;
        self.expect(Tok::KwDo)?;
        let body = self.parse_list_until(&[Tok::KwDone])?;
        self.expect(Tok::KwDone)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::While { cond, body, until, redirects })
    }

    fn parse_for(&mut self) -> Result<Command, String> {
        self.advance(); // KwFor
        if let Some(Tok::Arith(_)) = self.peek() {
            let raw = match self.advance() {
                Some(Tok::Arith(raw)) => raw,
                _ => unreachable!(),
            };
            let mut parts = raw.splitn(3, ';');
            let init = parts.next().unwrap_or("").trim().to_string();
            let cond = parts.next().unwrap_or("").trim().to_string();
            let step = parts.next().unwrap_or("").trim().to_string();
            self.skip_terminators();
            self.expect(Tok::KwDo)?;
            let body = self.parse_list_until(&[Tok::KwDone])?;
            self.expect(Tok::KwDone)?;
            let redirects = self.parse_trailing_redirects()?;
            return Ok(Command::CFor { init, cond, step, body, redirects });
        }
        let var = match self.advance() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks)
                .ok_or_else(|| "expected a plain variable name after 'for'".to_string())?,
            other => return Err(format!("expected variable name after 'for', got {:?}", other)),
        };
        self.skip_terminators();

        let mut words = None;
        if matches!(self.peek(), Some(Tok::KwIn)) {
            self.advance();
            let mut list = Vec::new();
            while let Some(Tok::Word(_, _)) = self.peek() {
                if let Some(Tok::Word(chunks, globbable)) = self.advance() {
                    list.push(Word { chunks, globbable });
                }
            }
            if matches!(self.peek(), Some(Tok::Semi) | Some(Tok::Newline)) {
                self.advance();
            }
            words = Some(list);
        }
        // No "in ...": bash iterates "$@" here (words stays None).
        self.skip_terminators();
        self.expect(Tok::KwDo)?;
        let body = self.parse_list_until(&[Tok::KwDone])?;
        self.expect(Tok::KwDone)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::For { var, words, body, redirects })
    }

    fn parse_case(&mut self) -> Result<Command, String> {
        self.advance(); // KwCase
        let word = self.expect_word()?;
        self.skip_terminators();
        self.expect(Tok::KwIn)?;
        self.skip_terminators();

        let mut arms = Vec::new();
        while !matches!(self.peek(), Some(Tok::KwEsac)) {
            let mut patterns = vec![self.expect_word()?];
            while matches!(self.peek(), Some(Tok::Pipe)) {
                self.advance();
                patterns.push(self.expect_word()?);
            }
            self.expect(Tok::RParen)?;
            self.skip_terminators();
            let body = self.parse_list_until(&[Tok::DSemi, Tok::KwEsac])?;
            if matches!(self.peek(), Some(Tok::DSemi)) {
                self.advance();
            }
            self.skip_terminators();
            arms.push((patterns, body));
        }
        self.expect(Tok::KwEsac)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::Case { word, arms, redirects })
    }

    fn parse_group(&mut self) -> Result<Command, String> {
        self.advance(); // LBrace
        let body = self.parse_list_until(&[Tok::RBrace])?;
        self.expect(Tok::RBrace)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::Group(body, redirects))
    }

    fn parse_trailing_redirects(&mut self) -> Result<Vec<Redirect>, String> {
        let mut redirects = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::RedirOut { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Out { word, append });
                }
                Some(Tok::RedirIn) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::In(word));
                }
                Some(Tok::RedirErr { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Err { word, append });
                }
                Some(Tok::RedirBoth { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Both { word, append });
                }
                Some(Tok::DupErrToOut) => {
                    self.advance();
                    redirects.push(Redirect::DupErrToOut);
                }
                Some(Tok::HereString) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::HereString(word));
                }
                Some(Tok::HereDoc(_)) => {
                    let chunks = match self.advance() {
                        Some(Tok::HereDoc(c)) => c,
                        _ => unreachable!(),
                    };
                    redirects.push(Redirect::HereDoc(Word { chunks, globbable: false }));
                }
                _ => break,
            }
        }
        Ok(redirects)
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand, String> {
        let mut assigns = Vec::new();
        let mut array_assigns = Vec::new();
        let mut index_assigns = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        let mut in_assign_phase = true;

        loop {
            match self.peek() {
                Some(Tok::Word(_, _)) => {
                    let (chunks, globbable) = match self.advance() {
                        Some(Tok::Word(c, g)) => (c, g),
                        _ => unreachable!(),
                    };
                    let w = Word { chunks, globbable };
                    if in_assign_phase {
                        if let Some((name, index, val)) = word_as_index_assignment(&w) {
                            index_assigns.push((name, index, val));
                            continue;
                        }
                        if let Some((name, mode, val)) = word_as_assignment(&w) {
                            // `name=(...)` / `name+=(...)` array literal:
                            // the lexer already captured the parenthesized
                            // content as a raw Subshell token (no space
                            // allowed between `=`/`+=` and `(`, matching
                            // bash's own syntax rule).
                            if is_empty_word(&val) {
                                if let Some(Tok::Subshell(_)) = self.peek() {
                                    let raw = match self.advance() {
                                        Some(Tok::Subshell(r)) => r,
                                        _ => unreachable!(),
                                    };
                                    let items = split_array_literal_words(&raw)?;
                                    array_assigns.push((name, mode, items));
                                    continue;
                                }
                            }
                            assigns.push((name, mode, val));
                            continue;
                        }
                        in_assign_phase = false;
                    }
                    words.push(w);
                }
                Some(Tok::RedirOut { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Out { word, append });
                }
                Some(Tok::RedirIn) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::In(word));
                }
                Some(Tok::RedirErr { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Err { word, append });
                }
                Some(Tok::RedirBoth { append }) => {
                    let append = *append;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Both { word, append });
                }
                Some(Tok::DupErrToOut) => {
                    self.advance();
                    redirects.push(Redirect::DupErrToOut);
                }
                Some(Tok::HereString) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::HereString(word));
                }
                Some(Tok::HereDoc(_)) => {
                    let chunks = match self.advance() {
                        Some(Tok::HereDoc(c)) => c,
                        _ => unreachable!(),
                    };
                    redirects.push(Redirect::HereDoc(Word { chunks, globbable: false }));
                }
                _ => break,
            }
        }

        if assigns.is_empty()
            && array_assigns.is_empty()
            && index_assigns.is_empty()
            && words.is_empty()
            && redirects.is_empty()
        {
            return Err("expected command".to_string());
        }
        Ok(SimpleCommand { assigns, array_assigns, index_assigns, words, redirects })
    }

    fn expect_word(&mut self) -> Result<Word, String> {
        match self.advance() {
            Some(Tok::Word(chunks, globbable)) => Ok(Word { chunks, globbable }),
            other => Err(format!("expected word, got {:?}", other)),
        }
    }
}

pub(crate) fn word_as_assignment(w: &Word) -> Option<(String, AssignMode, Word)> {
    let first = w.chunks.first()?;
    if let Chunk::Str(s) = first {
        let eq = s.find('=')?;
        let (name, mode) = if let Some(n) = s[..eq].strip_suffix('+') {
            (n, AssignMode::Append)
        } else {
            (&s[..eq], AssignMode::Set)
        };
        if is_valid_ident(name) {
            let mut rest_chunks = vec![Chunk::Str(s[eq + 1..].to_string())];
            rest_chunks.extend(w.chunks[1..].iter().cloned());
            // Assignment RHS is never glob-expanded in bash, regardless of
            // whether the original word would have been.
            return Some((name.to_string(), mode, Word { chunks: rest_chunks, globbable: false }));
        }
    }
    None
}

// `name[index]=value` -- the whole `name[index]=` prefix must land in a
// single leading literal chunk, so `arr[i]=x` works (the common case: `i`
// resolves as a bare identifier in the index's arithmetic context, no `$`
// needed) but a `$`-containing index like `arr[$i]=x` isn't recognized here
// (falls through to being read as a plain word instead).
fn word_as_index_assignment(w: &Word) -> Option<(String, String, Word)> {
    let first = w.chunks.first()?;
    if let Chunk::Str(s) = first {
        let bracket = s.find('[')?;
        let name = &s[..bracket];
        if !is_valid_ident(name) {
            return None;
        }
        let after_bracket = &s[bracket + 1..];
        let close = after_bracket.find(']')?;
        let index = after_bracket[..close].to_string();
        let after_close = &after_bracket[close + 1..];
        let value_start = after_close.strip_prefix('=')?;
        let mut rest_chunks = vec![Chunk::Str(value_start.to_string())];
        rest_chunks.extend(w.chunks[1..].iter().cloned());
        return Some((name.to_string(), index, Word { chunks: rest_chunks, globbable: false }));
    }
    None
}

fn is_empty_word(w: &Word) -> bool {
    matches!(w.chunks.as_slice(), [Chunk::Str(s)] if s.is_empty()) || w.chunks.is_empty()
}

// Re-lexes the raw captured text of an array literal's `(...)` body as a
// plain whitespace-separated word list. Reuses the general word tokenizer
// (rather than a bespoke splitter) so quoting/expansions inside the literal
// (`arr=("$x" 'lit' $(cmd))`) work exactly like anywhere else.
fn split_array_literal_words(raw: &str) -> Result<Vec<Word>, String> {
    let toks = Lexer::new(raw).tokenize()?;
    let mut words = Vec::new();
    for t in toks {
        match t {
            Tok::Word(chunks, globbable) => words.push(Word { chunks, globbable }),
            Tok::Newline => {}
            other => return Err(format!("unexpected token in array literal: {:?}", other)),
        }
    }
    Ok(words)
}

fn word_to_plain_name(chunks: &[Chunk]) -> Option<String> {
    if let [Chunk::Str(s)] = chunks {
        if is_valid_ident(s) {
            return Some(s.clone());
        }
    }
    None
}

fn is_valid_ident(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}
