// Hand-rolled tokenizer for the v1 shell grammar: pipelines, redirects,
// sequencing, quoting, and $VAR expansion. No globbing, no command
// substitution, no here-docs yet -- those come with later iterations.

// `quoted` records whether this expansion appeared inside "..." (or is
// otherwise not subject to word-splitting) -- exec.rs's expand_word_split
// only splits unquoted expansion results on IFS, matching bash: `"$x"`
// never splits even if $x contains spaces, but bare `$x` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Str(String),
    Var { name: String, quoted: bool },
    // Raw, not-yet-parsed source text of a $(...) or `...` command
    // substitution -- re-tokenized/parsed/run recursively at expansion time.
    Sub { raw: String, quoted: bool },
    // Raw source text of a $((...)) arithmetic expansion.
    Arith { raw: String, quoted: bool },
    VarExpand { name: String, op: VarOp, quoted: bool },
}

// The operand ("word"/"pattern") of each variant is kept as raw source text
// and re-expanded/glob-matched at evaluation time (see exec.rs), same
// deferred-parsing approach as Sub/Arith. `colon` distinguishes `${V:-x}`
// (triggers on unset-or-empty) from `${V-x}` (triggers on unset only) --
// v1 treats both the same (unset and empty are conflated throughout the
// shell's variable lookup), so `colon` is currently unused but kept for
// when that distinction is implemented.
#[derive(Debug, Clone, PartialEq)]
pub enum VarOp {
    Length,
    Default { word: String, colon: bool },
    AssignDefault { word: String, colon: bool },
    ErrorIfUnset { word: String, colon: bool },
    AltIfSet { word: String, colon: bool },
    RemovePrefix { pattern: String, longest: bool },
    RemoveSuffix { pattern: String, longest: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Word(chunks, globbable) -- globbable is true only if the word had no
    // quoting/escaping/expansion at all (see read_word); a word that's
    // partly quoted never glob-expands in v1, a conservative
    // under-globbing simplification vs. bash's per-character quote tracking.
    Word(Vec<Chunk>, bool),
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
    HereString,
    // Placeholder pushed at the `<<WORD` site; patched in place with the
    // real (already expansion-processed) body once the line's newline is
    // reached (see Lexer::pending_heredocs).
    HereDoc(Vec<Chunk>),
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
    // (index into `toks`, delimiter, strip-leading-tabs, expand-in-body) for
    // each `<<`/`<<-` seen on the current logical line, resolved once the
    // line's terminating newline is reached (see the body-capture comment
    // below for why this two-phase approach is needed).
    pending_heredocs: Vec<(usize, String, bool, bool)>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { chars: src.chars().peekable(), pending_heredocs: Vec::new() }
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
                    // A `<<WORD` redirect's body is the lines immediately
                    // following the line it appeared on, not the text right
                    // after the operator -- so the operator is tokenized
                    // where it appears (as a placeholder), and the body is
                    // only captured once we reach this newline, then patched
                    // into the already-pushed placeholder token.
                    if !self.pending_heredocs.is_empty() {
                        let pending = std::mem::take(&mut self.pending_heredocs);
                        for (tok_idx, delim, strip_tabs, expand) in pending {
                            let body = self.capture_heredoc_body(&delim, strip_tabs);
                            let chunks = if expand {
                                expand_heredoc_chunks(&body)?
                            } else {
                                vec![Chunk::Str(body)]
                            };
                            toks[tok_idx] = Tok::HereDoc(chunks);
                        }
                    }
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
                    if self.chars.peek().copied() == Some('<') {
                        self.chars.next();
                        if self.chars.peek().copied() == Some('<') {
                            self.chars.next();
                            toks.push(Tok::HereString);
                        } else {
                            let strip_tabs = self.chars.peek().copied() == Some('-');
                            if strip_tabs {
                                self.chars.next();
                            }
                            self.skip_spaces();
                            let (delim, expand) = self.read_heredoc_delimiter();
                            let tok_idx = toks.len();
                            toks.push(Tok::HereDoc(vec![Chunk::Str(String::new())]));
                            self.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                        }
                    } else {
                        toks.push(Tok::RedirIn);
                    }
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
                    toks.push(Tok::Word(word, plain));
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

    // Reads the delimiter word after `<<`/`<<-`. Only the common forms are
    // supported: a bare identifier-like word, or one fully wrapped in single
    // or double quotes -- either quote form suppresses expansion in the
    // body (matching bash: any quoting anywhere in the delimiter disables
    // it), a bare word allows it.
    fn read_heredoc_delimiter(&mut self) -> (String, bool) {
        match self.chars.peek().copied() {
            Some(q @ ('\'' | '"')) => {
                self.chars.next();
                let mut s = String::new();
                while let Some(c) = self.chars.next() {
                    if c == q {
                        break;
                    }
                    s.push(c);
                }
                (s, false)
            }
            _ => {
                let mut s = String::new();
                while let Some(c) = self.chars.peek().copied() {
                    if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')') {
                        break;
                    }
                    s.push(c);
                    self.chars.next();
                }
                (s, true)
            }
        }
    }

    // Reads raw lines (bypassing normal tokenization entirely) until one
    // equals the delimiter, per bash here-doc rules. `strip_tabs` (the `<<-`
    // form) strips leading tabs from both the delimiter comparison and the
    // retained body lines.
    fn capture_heredoc_body(&mut self, delimiter: &str, strip_tabs: bool) -> String {
        let mut body = String::new();
        loop {
            let mut line = String::new();
            let mut saw_newline = false;
            loop {
                match self.chars.next() {
                    None => break,
                    Some('\n') => {
                        saw_newline = true;
                        break;
                    }
                    Some(c) => line.push(c),
                }
            }
            let compare = if strip_tabs { line.trim_start_matches('\t') } else { line.as_str() };
            if compare == delimiter {
                break;
            }
            body.push_str(compare);
            body.push('\n');
            if !saw_newline {
                break; // EOF without a closing delimiter -- best-effort stop
            }
        }
        body
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

        // Tilde expansion: only the bare `~` / `~/...` form at the very
        // start of a word (no `~user` lookup).
        if self.chars.peek().copied() == Some('~') {
            let mut probe = self.chars.clone();
            probe.next();
            if matches!(probe.peek().copied(), None | Some('/') | Some(' ') | Some('\t') | Some('\n')) {
                self.chars.next();
                chunks.push(Chunk::Var { name: "HOME".to_string(), quoted: false });
            }
        }

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
                                self.push_var(&mut chunks, &mut buf, true)?;
                            }
                            Some('`') => {
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: true });
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
                    self.push_var(&mut chunks, &mut buf, false)?;
                }
                Some('`') => {
                    plain = false;
                    self.chars.next();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: false });
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

    // Consumes a variable reference (command substitution, arithmetic
    // expansion, or ${...} parameter expansion) after the '$' has already
    // been consumed, and pushes the appropriate Chunk, or a literal "$" if
    // nothing valid follows.
    fn push_var(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String, quoted: bool) -> Result<(), String> {
        if self.chars.peek().copied() == Some('(') {
            self.chars.next();
            if self.chars.peek().copied() == Some('(') {
                self.chars.next();
                chunks.push(Chunk::Arith { raw: self.capture_double_paren()?, quoted });
            } else {
                chunks.push(Chunk::Sub { raw: self.capture_balanced_parens()?, quoted });
            }
            return Ok(());
        }
        if self.chars.peek().copied() == Some('{') {
            self.chars.next();
            let mut inner = String::new();
            while let Some(c) = self.chars.peek().copied() {
                self.chars.next();
                if c == '}' {
                    break;
                }
                inner.push(c);
            }
            match parse_brace_content(&inner) {
                BraceContent::Plain(name) => {
                    if name.is_empty() {
                        buf.push('$');
                    } else {
                        chunks.push(Chunk::Var { name, quoted });
                    }
                }
                BraceContent::Op(name, op) => chunks.push(Chunk::VarExpand { name, op, quoted }),
            }
            return Ok(());
        }
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
        } else {
            chunks.push(Chunk::Var { name, quoted });
        }
        Ok(())
    }

    fn read_var_name(&mut self) -> String {
        if matches!(self.chars.peek().copied(), Some('?') | Some('#') | Some('@') | Some('*') | Some('$') | Some('!')) {
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

// Expands $VAR / $(...) / `...` / $((...)) within an expansion-enabled
// here-doc body. Can't reuse read_word directly since it breaks words on
// unquoted newlines/whitespace -- a heredoc body must keep embedded
// newlines as literal content, not word separators. Backslash escaping
// here mirrors double-quote semantics (only \$, \\, \` are special).
// Expansions are marked `quoted: true` since the body is used verbatim as
// stdin text, never word-split.
fn expand_heredoc_chunks(body: &str) -> Result<Vec<Chunk>, String> {
    let mut lexer = Lexer::new(body);
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut buf = String::new();
    loop {
        match lexer.chars.peek().copied() {
            None => break,
            Some('\\') => {
                lexer.chars.next();
                match lexer.chars.next() {
                    Some(n) if n == '$' || n == '\\' || n == '`' => buf.push(n),
                    Some(n) => {
                        buf.push('\\');
                        buf.push(n);
                    }
                    None => buf.push('\\'),
                }
            }
            Some('$') => {
                lexer.chars.next();
                if !buf.is_empty() {
                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                }
                lexer.push_var(&mut chunks, &mut buf, true)?;
            }
            Some('`') => {
                lexer.chars.next();
                if !buf.is_empty() {
                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                }
                chunks.push(Chunk::Sub { raw: lexer.capture_backtick()?, quoted: true });
            }
            Some(c) => {
                lexer.chars.next();
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

enum BraceContent {
    Plain(String),
    Op(String, VarOp),
}

// Parses the text captured between `${` and `}`. `${#VAR}` (length) is
// checked first since '#' would otherwise be read as the RemovePrefix
// operator on an empty name.
fn parse_brace_content(inner: &str) -> BraceContent {
    if let Some(rest) = inner.strip_prefix('#') {
        if !rest.is_empty() {
            return BraceContent::Op(rest.to_string(), VarOp::Length);
        }
    }

    let mut name_end = 0;
    let mut chars = inner.char_indices();
    if let Some((_, c)) = chars.next() {
        if matches!(c, '?' | '#' | '@' | '*' | '$' | '!') {
            name_end = c.len_utf8();
        } else if c.is_alphanumeric() || c == '_' {
            name_end = c.len_utf8();
            for (i, c) in chars {
                if c.is_alphanumeric() || c == '_' {
                    name_end = i + c.len_utf8();
                } else {
                    break;
                }
            }
        }
    }
    let name = inner[..name_end].to_string();
    let rest = &inner[name_end..];
    if rest.is_empty() {
        return BraceContent::Plain(name);
    }

    macro_rules! op {
        ($prefix:expr, $variant:ident, $colon:expr) => {
            if let Some(w) = rest.strip_prefix($prefix) {
                return BraceContent::Op(name, VarOp::$variant { word: w.to_string(), colon: $colon });
            }
        };
    }
    op!(":-", Default, true);
    op!(":=", AssignDefault, true);
    op!(":?", ErrorIfUnset, true);
    op!(":+", AltIfSet, true);
    if let Some(w) = rest.strip_prefix("##") {
        return BraceContent::Op(name, VarOp::RemovePrefix { pattern: w.to_string(), longest: true });
    }
    if let Some(w) = rest.strip_prefix('#') {
        return BraceContent::Op(name, VarOp::RemovePrefix { pattern: w.to_string(), longest: false });
    }
    if let Some(w) = rest.strip_prefix("%%") {
        return BraceContent::Op(name, VarOp::RemoveSuffix { pattern: w.to_string(), longest: true });
    }
    if let Some(w) = rest.strip_prefix('%') {
        return BraceContent::Op(name, VarOp::RemoveSuffix { pattern: w.to_string(), longest: false });
    }
    op!("-", Default, false);
    op!("=", AssignDefault, false);
    op!("?", ErrorIfUnset, false);
    op!("+", AltIfSet, false);

    // Unrecognized operator syntax: fall back to treating the whole thing
    // as a literal (best-effort; will just look up a likely-nonexistent
    // variable name rather than crashing).
    BraceContent::Plain(inner.to_string())
}
