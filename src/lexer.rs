// Hand-rolled tokenizer for the v1 shell grammar: pipelines, redirects,
// sequencing, quoting, and $VAR expansion. No globbing, no command
// substitution, no here-docs yet -- those come with later iterations.

#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Str(String),
    Var(String),
    // Raw, not-yet-parsed source text of a $(...) or `...` command
    // substitution -- re-tokenized/parsed/run recursively at expansion time.
    Sub(String),
    // Raw source text of a $((...)) arithmetic expansion.
    Arith(String),
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
    LParen,
    RParen,
    // Raw, not-yet-parsed source text of a (...) subshell pipeline stage.
    Subshell(String),
    // Raw source text of a standalone ((...)) arithmetic command.
    Arith(String),
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
                Some('(') => {
                    self.chars.next();
                    if self.chars.peek().copied() == Some(')') {
                        // empty parens: function-def syntax `name()`
                        self.chars.next();
                        toks.push(Tok::LParen);
                        toks.push(Tok::RParen);
                    } else if self.chars.peek().copied() == Some('(') {
                        self.chars.next();
                        let raw = self.capture_double_paren()?;
                        toks.push(Tok::Arith(raw));
                    } else {
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Subshell(raw));
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
                    let (word, plain) = self.read_word()?;
                    if plain {
                        if let [Chunk::Str(s)] = word.as_slice() {
                            if let Some(kw) = keyword(s) {
                                toks.push(kw);
                                continue;
                            }
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

    // Called with one '(' already consumed (depth 1). Scans forward,
    // honoring nested parens and quotes, and returns the raw text up to
    // (not including) the matching close paren. Shared by $(...) command
    // substitution and (...) subshells -- neither is tokenized/parsed until
    // the substitution actually runs.
    fn capture_balanced_parens(&mut self) -> Result<String, String> {
        let mut depth = 1;
        let mut s = String::new();
        loop {
            match self.chars.next() {
                None => return Err("unterminated '('".to_string()),
                Some('(') => {
                    depth += 1;
                    s.push('(');
                }
                Some(')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    s.push(')');
                }
                Some('\'') => {
                    s.push('\'');
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated single quote".to_string()),
                            Some('\'') => {
                                s.push('\'');
                                break;
                            }
                            Some(c) => s.push(c),
                        }
                    }
                }
                Some('"') => {
                    s.push('"');
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => {
                                s.push('"');
                                break;
                            }
                            Some('\\') => {
                                s.push('\\');
                                if let Some(n) = self.chars.next() {
                                    s.push(n);
                                }
                            }
                            Some(c) => s.push(c),
                        }
                    }
                }
                Some('\\') => {
                    s.push('\\');
                    if let Some(n) = self.chars.next() {
                        s.push(n);
                    }
                }
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    // Called with BOTH opening parens of `((...))` already consumed by the
    // caller. Captures the inner expression text and consumes the matching
    // final ')' (capture_balanced_parens stops after the first of the two
    // closing parens, since it only tracks the depth opened by the second
    // '(').
    fn capture_double_paren(&mut self) -> Result<String, String> {
        let raw = self.capture_balanced_parens()?;
        if self.chars.peek().copied() == Some(')') {
            self.chars.next();
        }
        Ok(raw)
    }

    fn capture_backtick(&mut self) -> Result<String, String> {
        let mut s = String::new();
        loop {
            match self.chars.next() {
                None => return Err("unterminated '`'".to_string()),
                Some('`') => break,
                Some('\\') => match self.chars.peek().copied() {
                    Some(n) if n == '`' || n == '\\' || n == '$' => {
                        self.chars.next();
                        s.push(n);
                    }
                    _ => s.push('\\'),
                },
                Some(c) => s.push(c),
            }
        }
        Ok(s)
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

    // Returns the word's chunks plus whether it was written with no
    // quoting/escaping at all -- only a fully "plain" word is eligible for
    // reserved-word (keyword) recognition, matching bash: `"if"` or `\if`
    // is always a literal word, never the `if` keyword.
    fn read_word(&mut self) -> Result<(Vec<Chunk>, bool), String> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut buf = String::new();
        let mut plain = true;

        loop {
            match self.chars.peek().copied() {
                None => break,
                Some(c) if c == ' ' || c == '\t' || c == '\n' => break,
                Some('|') | Some('&') | Some(';') | Some('<') | Some('>') | Some('#') | Some('(') | Some(')') => break,
                Some('\'') => {
                    plain = false;
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
                    plain = false;
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
                                self.push_var(&mut chunks, &mut buf)?;
                            }
                            Some('`') => {
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                chunks.push(Chunk::Sub(self.capture_backtick()?));
                            }
                            Some(c) => buf.push(c),
                        }
                    }
                }
                Some('\\') => {
                    plain = false;
                    self.chars.next();
                    if let Some(n) = self.chars.next() {
                        buf.push(n);
                    }
                }
                Some('$') => {
                    plain = false;
                    self.chars.next();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    self.push_var(&mut chunks, &mut buf)?;
                }
                Some('`') => {
                    plain = false;
                    self.chars.next();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    chunks.push(Chunk::Sub(self.capture_backtick()?));
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
        Ok((chunks, plain))
    }

    // Consumes a variable reference (or $(...) command substitution) after
    // the '$' has already been consumed, and pushes the appropriate Chunk,
    // or a literal "$" if nothing valid follows.
    fn push_var(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String) -> Result<(), String> {
        if self.chars.peek().copied() == Some('(') {
            self.chars.next();
            if self.chars.peek().copied() == Some('(') {
                self.chars.next();
                chunks.push(Chunk::Arith(self.capture_double_paren()?));
            } else {
                chunks.push(Chunk::Sub(self.capture_balanced_parens()?));
            }
            return Ok(());
        }
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
        } else {
            chunks.push(Chunk::Var(name));
        }
        Ok(())
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
        } else if matches!(self.chars.peek().copied(), Some('?') | Some('#') | Some('@') | Some('*') | Some('$') | Some('!')) {
            self.chars.next().unwrap().to_string()
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
