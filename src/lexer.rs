// Hand-rolled tokenizer for the v1 shell grammar: pipelines, redirects,
// sequencing, quoting, and $VAR expansion. No globbing, no command
// substitution, no here-docs yet -- those come with later iterations.

#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Str(String),
    Var(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Word(Vec<Chunk>),
    Pipe,
    And,
    Or,
    Semi,
    DSemi,
    Amp,
    RedirOut { append: bool },
    RedirIn,
    RedirErr { append: bool },
    RedirBoth { append: bool },
    DupErrToOut,
    Newline,
    LBrace,
    RBrace,
    RParen,
    KwIf,
    KwThen,
    KwElif,
    KwElse,
    KwFi,
    KwWhile,
    KwUntil,
    KwDo,
    KwDone,
    KwFor,
    KwIn,
    KwCase,
    KwEsac,
    KwFunction,
}

fn keyword(s: &str) -> Option<Tok> {
    Some(match s {
        "if" => Tok::KwIf,
        "then" => Tok::KwThen,
        "elif" => Tok::KwElif,
        "else" => Tok::KwElse,
        "fi" => Tok::KwFi,
        "while" => Tok::KwWhile,
        "until" => Tok::KwUntil,
        "do" => Tok::KwDo,
        "done" => Tok::KwDone,
        "for" => Tok::KwFor,
        "in" => Tok::KwIn,
        "case" => Tok::KwCase,
        "esac" => Tok::KwEsac,
        "function" => Tok::KwFunction,
        _ => return None,
    })
}

pub struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { chars: src.chars().peekable() }
    }

    pub fn tokenize(mut self) -> Result<Vec<Tok>, String> {
        let mut toks = Vec::new();
        loop {
            self.skip_spaces();
            match self.chars.peek().copied() {
                None => break,
                Some('#') => {
                    while let Some(c) = self.chars.peek().copied() {
                        if c == '\n' {
                            break;
                        }
                        self.chars.next();
                    }
                }
                Some('\n') => {
                    self.chars.next();
                    toks.push(Tok::Newline);
                }
                Some('|') => {
                    self.chars.next();
                    if self.chars.peek().copied() == Some('|') {
                        self.chars.next();
                        toks.push(Tok::Or);
                    } else {
                        toks.push(Tok::Pipe);
                    }
                }
                Some('&') => {
                    self.chars.next();
                    if self.chars.peek().copied() == Some('&') {
                        self.chars.next();
                        toks.push(Tok::And);
                    } else if self.chars.peek().copied() == Some('>') {
                        self.chars.next();
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.chars.next();
                        }
                        toks.push(Tok::RedirBoth { append });
                    } else {
                        toks.push(Tok::Amp);
                    }
                }
                Some(';') => {
                    self.chars.next();
                    if self.chars.peek().copied() == Some(';') {
                        self.chars.next();
                        toks.push(Tok::DSemi);
                    } else {
                        toks.push(Tok::Semi);
                    }
                }
                Some(')') => {
                    self.chars.next();
                    toks.push(Tok::RParen);
                }
                Some('{') if self.next_char_is_word_boundary() => {
                    self.chars.next();
                    toks.push(Tok::LBrace);
                }
                Some('}') if self.next_char_is_word_boundary() => {
                    self.chars.next();
                    toks.push(Tok::RBrace);
                }
                Some('>') => {
                    self.chars.next();
                    let append = self.chars.peek().copied() == Some('>');
                    if append {
                        self.chars.next();
                    }
                    toks.push(Tok::RedirOut { append });
                }
                Some('<') => {
                    self.chars.next();
                    toks.push(Tok::RedirIn);
                }
                Some('2') if self.peek_is_fd2_redirect() => {
                    self.chars.next(); // '2'
                    self.chars.next(); // '>'
                    if self.chars.peek().copied() == Some('&') && self.peek2() == Some('1') {
                        self.chars.next(); // '&'
                        self.chars.next(); // '1'
                        toks.push(Tok::DupErrToOut);
                    } else {
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.chars.next();
                        }
                        toks.push(Tok::RedirErr { append });
                    }
                }
                _ => {
                    let word = self.read_word()?;
                    if let [Chunk::Str(s)] = word.as_slice() {
                        if let Some(kw) = keyword(s) {
                            toks.push(kw);
                            continue;
                        }
                    }
                    toks.push(Tok::Word(word));
                }
            }
        }
        Ok(toks)
    }

    fn peek_is_fd2_redirect(&self) -> bool {
        let mut it = self.chars.clone();
        if it.next() != Some('2') {
            return false;
        }
        it.next() == Some('>')
    }

    fn peek2(&self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.next()
    }

    fn next_char_is_word_boundary(&self) -> bool {
        let mut it = self.chars.clone();
        it.next(); // the brace itself
        match it.next() {
            None => true,
            Some(c) => c == ' ' || c == '\t' || c == '\n' || c == ';' || c == '|' || c == '&' || c == ')',
        }
    }

    fn skip_spaces(&mut self) {
        while let Some(c) = self.chars.peek().copied() {
            if c == ' ' || c == '\t' {
                self.chars.next();
            } else {
                break;
            }
        }
    }

    fn read_word(&mut self) -> Result<Vec<Chunk>, String> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut buf = String::new();

        loop {
            match self.chars.peek().copied() {
                None => break,
                Some(c) if c == ' ' || c == '\t' || c == '\n' => break,
                Some('|') | Some('&') | Some(';') | Some('<') | Some('>') | Some('#') | Some(')') => break,
                Some('\'') => {
                    self.chars.next();
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated single quote".to_string()),
                            Some('\'') => break,
                            Some(c) => buf.push(c),
                        }
                    }
                }
                Some('"') => {
                    self.chars.next();
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => break,
                            Some('\\') => match self.chars.peek().copied() {
                                Some(n) if n == '"' || n == '\\' || n == '$' => {
                                    self.chars.next();
                                    buf.push(n);
                                }
                                _ => buf.push('\\'),
                            },
                            Some('$') => {
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                self.push_var(&mut chunks, &mut buf);
                            }
                            Some(c) => buf.push(c),
                        }
                    }
                }
                Some('\\') => {
                    self.chars.next();
                    if let Some(n) = self.chars.next() {
                        buf.push(n);
                    }
                }
                Some('$') => {
                    self.chars.next();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    self.push_var(&mut chunks, &mut buf);
                }
                Some(c) => {
                    self.chars.next();
                    buf.push(c);
                }
            }
        }

        if !buf.is_empty() {
            chunks.push(Chunk::Str(buf));
        }
        if chunks.is_empty() {
            chunks.push(Chunk::Str(String::new()));
        }
        Ok(chunks)
    }

    // Consumes a variable reference after the '$' has already been consumed
    // and pushes either a Chunk::Var, or a literal "$" if nothing valid follows.
    fn push_var(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String) {
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
        } else {
            chunks.push(Chunk::Var(name));
        }
    }

    fn read_var_name(&mut self) -> String {
        if self.chars.peek().copied() == Some('{') {
            self.chars.next();
            let mut name = String::new();
            while let Some(c) = self.chars.peek().copied() {
                self.chars.next();
                if c == '}' {
                    break;
                }
                name.push(c);
            }
            name
        } else if self.chars.peek().copied() == Some('?') {
            self.chars.next();
            "?".to_string()
        } else {
            let mut name = String::new();
            while let Some(c) = self.chars.peek().copied() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    self.chars.next();
                } else {
                    break;
                }
            }
            name
        }
    }
}
