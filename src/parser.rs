use crate::lexer::{Chunk, Tok};

#[derive(Debug, Clone)]
pub struct Word {
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone)]
pub enum Redirect {
    In(Word),
    Out { word: Word, append: bool },
    Err { word: Word, append: bool },
    Both { word: Word, append: bool },
    DupErrToOut,
}

#[derive(Debug, Clone)]
pub struct SimpleCommand {
    pub assigns: Vec<(String, Word)>,
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
        words: Vec<Word>,
        body: Program,
        redirects: Vec<Redirect>,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, Program)>,
        redirects: Vec<Redirect>,
    },
    Group(Program, Vec<Redirect>),
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<Command>,
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
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands })
    }

    fn parse_command(&mut self) -> Result<Command, String> {
        match self.peek() {
            Some(Tok::KwIf) => self.parse_if(),
            Some(Tok::KwWhile) => self.parse_while(false),
            Some(Tok::KwUntil) => self.parse_while(true),
            Some(Tok::KwFor) => self.parse_for(),
            Some(Tok::KwCase) => self.parse_case(),
            Some(Tok::LBrace) => self.parse_group(),
            _ => Ok(Command::Simple(self.parse_simple_command()?)),
        }
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
        let var = match self.advance() {
            Some(Tok::Word(chunks)) => word_to_plain_name(&chunks)
                .ok_or_else(|| "expected a plain variable name after 'for'".to_string())?,
            other => return Err(format!("expected variable name after 'for', got {:?}", other)),
        };
        self.skip_terminators();

        let mut words = Vec::new();
        if matches!(self.peek(), Some(Tok::KwIn)) {
            self.advance();
            while let Some(Tok::Word(_)) = self.peek() {
                if let Some(Tok::Word(chunks)) = self.advance() {
                    words.push(Word { chunks });
                }
            }
            if matches!(self.peek(), Some(Tok::Semi) | Some(Tok::Newline)) {
                self.advance();
            }
        }
        // No "in ...": bash iterates "$@" here. Positional params land in
        // M3; until then this is an empty (zero-iteration) loop.
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
                _ => break,
            }
        }
        Ok(redirects)
    }

    fn parse_simple_command(&mut self) -> Result<SimpleCommand, String> {
        let mut assigns = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        let mut in_assign_phase = true;

        loop {
            match self.peek() {
                Some(Tok::Word(_)) => {
                    let chunks = match self.advance() {
                        Some(Tok::Word(c)) => c,
                        _ => unreachable!(),
                    };
                    let w = Word { chunks };
                    if in_assign_phase {
                        if let Some((name, val)) = word_as_assignment(&w) {
                            assigns.push((name, val));
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
                _ => break,
            }
        }

        if assigns.is_empty() && words.is_empty() && redirects.is_empty() {
            return Err("expected command".to_string());
        }
        Ok(SimpleCommand { assigns, words, redirects })
    }

    fn expect_word(&mut self) -> Result<Word, String> {
        match self.advance() {
            Some(Tok::Word(chunks)) => Ok(Word { chunks }),
            other => Err(format!("expected word, got {:?}", other)),
        }
    }
}

fn word_as_assignment(w: &Word) -> Option<(String, Word)> {
    let first = w.chunks.first()?;
    if let Chunk::Str(s) = first {
        let eq = s.find('=')?;
        let name = &s[..eq];
        if is_valid_ident(name) {
            let mut rest_chunks = vec![Chunk::Str(s[eq + 1..].to_string())];
            rest_chunks.extend(w.chunks[1..].iter().cloned());
            return Some((name.to_string(), Word { chunks: rest_chunks }));
        }
    }
    None
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
