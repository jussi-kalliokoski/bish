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

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub commands: Vec<SimpleCommand>,
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

    pub fn parse_program(&mut self) -> Result<Program, String> {
        let mut items = Vec::new();
        self.skip_terminators();
        while self.peek().is_some() {
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
                None => Sep::Seq,
                Some(other) => return Err(format!("unexpected token: {:?}", other)),
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
        let mut commands = vec![self.parse_simple_command()?];
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_simple_command()?);
        }
        Ok(Pipeline { commands })
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
