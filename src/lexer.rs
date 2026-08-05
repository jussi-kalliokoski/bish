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
    // ${name[index]} -- index is raw text: "@"/"*" (all elements, one
    // element per field like $@/$*) or an arithmetic expression evaluated
    // at access time (so ${arr[i+1]} works).
    ArrayVar { name: String, index: String, quoted: bool },
    // ${#name[index]} -- index "@"/"*" gives the array length; a specific
    // index gives that element's string length.
    ArrayLength { name: String, index: String },
    // ${name[index]:-word} and friends -- same operators as VarExpand, but
    // reading from the array element instead of a scalar.
    ArrayVarExpand { name: String, index: String, op: VarOp, quoted: bool },
    // ${!name} -- indirect expansion: look up the variable NAMED by the
    // current value of `name` (one level only, same as bash).
    Indirect { name: String, quoted: bool },
    // ${!name[@]} / ${!name[*]} -- the array's keys/indices, not its values
    // (indexed arrays: their set indices as decimal strings; associative:
    // their string keys), one field per key like ${name[@]}.
    ArrayKeys { name: String, quoted: bool },
    // <(cmd) / >(cmd) process substitution. Real bash backs these with a
    // FIFO/`/dev/fd/N` so the substituted command streams concurrently;
    // that needs fd-passing into a spawned child, which isn't available
    // from safe std without unsafe pre_exec plumbing. Approximated instead
    // with a temp file: ProcSubIn runs `cmd` to completion and substitutes
    // the file it wrote; ProcSubOut substitutes the file path immediately
    // and runs `cmd` reading it back after the enclosing command finishes.
    // Correct data flow, but not concurrent -- documented gap.
    ProcSubIn { raw: String },
    ProcSubOut { raw: String },
    // A literal run of text that was quoted or backslash-escaped in the
    // source, ONLY ever produced while lexing a `[[ ... =~ pattern ]]`
    // operand (relaxed mode in read_word). Plain Chunk::Str can't carry
    // this distinction -- quoted and unquoted literal runs normally merge
    // losslessly into one Chunk::Str, since ordinary word-splitting never
    // needs to tell them apart. The =~ operand does: quoting/escaping part
    // of a regex pattern forces that part to match literally instead of
    // as regex syntax (see expand_regex_operand in exec.rs), so this
    // variant exists to keep those runs distinguishable from the
    // surrounding unquoted regex syntax. Treated identically to
    // Chunk::Str everywhere else (word-splitting, serialization).
    LiteralStr(String),
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
    // ${V^pattern}/${V^^pattern}/${V,pattern}/${V,,pattern} -- `upper`
    // picks the direction, `all` picks first-char-only vs every char. An
    // empty pattern (the common case, e.g. `${V^^}`) means "any character";
    // otherwise the pattern is matched against each candidate char with the
    // same glob matcher `case` patterns use.
    CaseConvert { pattern: String, upper: bool, all: bool },
    // ${V:offset} / ${V:offset:length}. offset/length are raw arithmetic
    // expression text, evaluated at expansion time so `${V:$i:$n}` works.
    // A negative offset counts from the end; a negative length is an end
    // position counted from the end too (bash: "an offset from the end of
    // the string"), not an error -- both match real bash's substring
    // semantics exactly (see substring_expand in exec.rs).
    Substring { offset: String, length: Option<String> },
    // ${V/pat/repl}, ${V//pat/repl}, ${V/#pat/repl}, ${V/%pat/repl} --
    // `global` picks first-match vs all-matches, `anchor` restricts the
    // match to the start/end of the string (mirrors RemovePrefix/
    // RemoveSuffix's pattern-matching, but for substitution instead of
    // deletion). pattern/repl are raw text, expanded at match/substitution
    // time.
    Replace { pattern: String, repl: String, global: bool, anchor: ReplaceAnchor },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplaceAnchor {
    None,
    Start,
    End,
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
    // `[[`/`]]` are recognized here (not left as plain Word tokens) so the
    // parser can treat everything between them as a single test expression
    // -- otherwise `&&`/`||` inside `[[ ]]` would be swallowed by the outer
    // AndOr grammar and silently split one logical `[[ ]]` into two
    // commands.
    KwLBracket2,
    KwRBracket2,
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
        "[[" => Tok::KwLBracket2,
        "]]" => Tok::KwRBracket2,
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
    // How many `[[`s we're currently nested inside (tracked so `=~`'s
    // special word-reading below only kicks in there, not for a stray
    // `=~` word elsewhere).
    bracket2_depth: u32,
    // Set right after emitting the `=~` word while inside `[[ ]]`; makes
    // the *next* word read via read_word(relaxed: true) instead of the
    // normal dispatch. See the =~ handling in tokenize().
    regex_operand_next: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            chars: src.chars().peekable(),
            pending_heredocs: Vec::new(),
            bracket2_depth: 0,
            regex_operand_next: false,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Tok>, String> {
        let mut toks = Vec::new();
        loop {
            self.skip_spaces();
            if self.regex_operand_next {
                self.regex_operand_next = false;
                if self.chars.peek().is_some() {
                    let (word, plain) = self.read_word(true)?;
                    toks.push(Tok::Word(word, plain));
                }
                continue;
            }
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
                    if self.chars.peek().copied() == Some('(') {
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
                    if self.chars.peek().copied() == Some('(') {
                        self.chars.next();
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Word(vec![Chunk::ProcSubOut { raw }], false));
                        continue;
                    }
                    let append = self.chars.peek().copied() == Some('>');
                    if append {
                        self.chars.next();
                    }
                    toks.push(Tok::RedirOut { append });
                }
                Some('<') => {
                    self.chars.next();
                    if self.chars.peek().copied() == Some('(') {
                        self.chars.next();
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Word(vec![Chunk::ProcSubIn { raw }], false));
                        continue;
                    }
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
                    let (word, plain) = self.read_word(false)?;
                    if plain {
                        if let [Chunk::Str(s)] = word.as_slice() {
                            if let Some(kw) = keyword(s) {
                                match kw {
                                    Tok::KwLBracket2 => self.bracket2_depth += 1,
                                    Tok::KwRBracket2 => self.bracket2_depth = self.bracket2_depth.saturating_sub(1),
                                    _ => {}
                                }
                                toks.push(kw);
                                continue;
                            }
                            if self.bracket2_depth > 0 && s == "=~" {
                                self.regex_operand_next = true;
                            }
                            if s.contains('{') {
                                let expanded = brace_expand(s);
                                if expanded.len() > 1 || expanded[0] != *s {
                                    for e in expanded {
                                        toks.push(Tok::Word(vec![Chunk::Str(e)], true));
                                    }
                                    continue;
                                }
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

    // Reads a $'...' ANSI-C quoted string (opening $' already consumed).
    // Result is a plain literal -- no further $ expansion happens inside,
    // matching bash. Common C-style escapes only (no \xHH/\uHHHH/octal).
    fn read_ansi_c_string(&mut self) -> Result<String, String> {
        let mut s = String::new();
        loop {
            match self.chars.next() {
                None => return Err("unterminated $'...'".to_string()),
                Some('\'') => break,
                Some('\\') => match self.chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some('\\') => s.push('\\'),
                    Some('\'') => s.push('\''),
                    Some('"') => s.push('"'),
                    Some('a') => s.push('\x07'),
                    Some('b') => s.push('\x08'),
                    Some('e') => s.push('\x1b'),
                    Some('f') => s.push('\x0c'),
                    Some('v') => s.push('\x0b'),
                    Some('0') => s.push('\0'),
                    Some(other) => {
                        s.push('\\');
                        s.push(other);
                    }
                    None => return Err("unterminated $'...'".to_string()),
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
    // `relaxed` is used for exactly one thing: the right-hand operand of
    // `[[ ... =~ pattern ]]`. A regex pattern routinely contains `|`, `(`,
    // `)` (alternation, grouping) that otherwise mean pipe/subshell to this
    // lexer -- but real bash exempts exactly those three from operator
    // tokenization for an unquoted =~ operand and nothing else (confirmed
    // empirically: bash accepts `=~ ^(cat|dog)$` unquoted but rejects bare
    // `&`/`<`/`>` there with a syntax error, same as it would anywhere
    // else in `[[ ]]`), so this mirrors that narrow exemption rather than
    // relaxing every shell metacharacter. Whitespace and `]]` still end
    // the word normally.
    fn read_word(&mut self, relaxed: bool) -> Result<(Vec<Chunk>, bool), String> {
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
                Some('|') | Some('(') | Some(')') if relaxed => {
                    buf.push(self.chars.next().unwrap());
                }
                Some('|') | Some('&') | Some(';') | Some('<') | Some('>') | Some('#') | Some('(') | Some(')') => break,
                Some('\'') => {
                    plain = false;
                    self.chars.next();
                    let mut lit = String::new();
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated single quote".to_string()),
                            Some('\'') => break,
                            Some(c) => lit.push(c),
                        }
                    }
                    if relaxed {
                        if !buf.is_empty() {
                            chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                        }
                        if !lit.is_empty() {
                            chunks.push(Chunk::LiteralStr(lit));
                        }
                    } else {
                        buf.push_str(&lit);
                    }
                }
                Some('"') => {
                    plain = false;
                    self.chars.next();
                    let mut lit = String::new();
                    loop {
                        match self.chars.next() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => break,
                            Some('\\') => match self.chars.peek().copied() {
                                Some(n) if n == '"' || n == '\\' || n == '$' => {
                                    self.chars.next();
                                    lit.push(n);
                                }
                                _ => lit.push('\\'),
                            },
                            Some('$') => {
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    let taken = std::mem::take(&mut lit);
                                    chunks.push(if relaxed { Chunk::LiteralStr(taken) } else { Chunk::Str(taken) });
                                }
                                // Pass `lit` (now empty), not `buf`: if
                                // nothing valid follows the `$`, push_var's
                                // fallback writes a literal "$" back into
                                // whatever buffer it's given -- it must
                                // land in the quoted accumulator here, or a
                                // trailing "$" (e.g. a regex end-anchor
                                // written *inside* quotes) would wrongly
                                // lose its quoted-ness.
                                self.push_var(&mut chunks, &mut lit, true)?;
                            }
                            Some('`') => {
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    let taken = std::mem::take(&mut lit);
                                    chunks.push(if relaxed { Chunk::LiteralStr(taken) } else { Chunk::Str(taken) });
                                }
                                chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: true });
                            }
                            Some(c) => lit.push(c),
                        }
                    }
                    if !lit.is_empty() {
                        if relaxed {
                            chunks.push(Chunk::LiteralStr(lit));
                        } else {
                            buf.push_str(&lit);
                        }
                    }
                }
                Some('\\') => {
                    plain = false;
                    self.chars.next();
                    if let Some(n) = self.chars.next() {
                        if relaxed {
                            if !buf.is_empty() {
                                chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                            }
                            chunks.push(Chunk::LiteralStr(n.to_string()));
                        } else {
                            buf.push(n);
                        }
                    }
                }
                Some('$') if self.peek2() == Some('\'') => {
                    plain = false;
                    self.chars.next(); // '$'
                    self.chars.next(); // '\''
                    buf.push_str(&self.read_ansi_c_string()?);
                }
                Some('$') => {
                    self.chars.next();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    if self.push_var(&mut chunks, &mut buf, false)? {
                        plain = false;
                    }
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
    // Returns whether a real expansion chunk was produced, as opposed to
    // falling back to a literal "$" (e.g. a trailing `$` with nothing
    // valid after it, like a regex end-anchor). Callers that track a
    // word's "plain" (unquoted/unescaped) status use this to avoid
    // treating a bare literal `$` as if the word had been quoted or
    // escaped -- see the unquoted `$` case in read_word().
    fn push_var(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String, quoted: bool) -> Result<bool, String> {
        if self.chars.peek().copied() == Some('(') {
            self.chars.next();
            if self.chars.peek().copied() == Some('(') {
                self.chars.next();
                chunks.push(Chunk::Arith { raw: self.capture_double_paren()?, quoted });
            } else {
                chunks.push(Chunk::Sub { raw: self.capture_balanced_parens()?, quoted });
            }
            return Ok(true);
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
                        return Ok(false);
                    } else {
                        chunks.push(Chunk::Var { name, quoted });
                    }
                }
                BraceContent::Op(name, op) => chunks.push(Chunk::VarExpand { name, op, quoted }),
                BraceContent::ArrayIndex(name, index) => chunks.push(Chunk::ArrayVar { name, index, quoted }),
                BraceContent::ArrayLength(name, index) => chunks.push(Chunk::ArrayLength { name, index }),
                BraceContent::ArrayOp(name, index, op) => {
                    chunks.push(Chunk::ArrayVarExpand { name, index, op, quoted })
                }
                BraceContent::Indirect(name) => chunks.push(Chunk::Indirect { name, quoted }),
                BraceContent::ArrayKeys(name) => chunks.push(Chunk::ArrayKeys { name, quoted }),
            }
            return Ok(true);
        }
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
            Ok(false)
        } else {
            chunks.push(Chunk::Var { name, quoted });
            Ok(true)
        }
    }

    fn read_var_name(&mut self) -> String {
        if matches!(self.chars.peek().copied(), Some('?') | Some('#') | Some('@') | Some('*') | Some('$') | Some('!') | Some('-')) {
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
    ArrayIndex(String, String),
    ArrayLength(String, String),
    ArrayOp(String, String, VarOp),
    Indirect(String),
    ArrayKeys(String),
}

// `name[index]rest` -- name must be a plain identifier; returns whatever
// text follows the closing ']' (empty if nothing does) so the caller can
// decide between a bare array access and one with an operator suffix
// (${arr[0]:-x}).
fn try_split_array(s: &str) -> Option<(String, String, &str)> {
    let bracket = s.find('[')?;
    let name = &s[..bracket];
    let mut nc = name.chars();
    let first = nc.next()?;
    if !(first.is_alphabetic() || first == '_') || !nc.all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let rest = &s[bracket + 1..];
    let close = rest.find(']')?;
    Some((name.to_string(), rest[..close].to_string(), &rest[close + 1..]))
}

// Shared by the scalar and array paths: parses the operator suffix that
// follows a name (or `name[index]`) inside `${...}`.
fn parse_operator_suffix(rest: &str) -> Option<VarOp> {
    macro_rules! op {
        ($prefix:expr, $variant:ident, $colon:expr) => {
            if let Some(w) = rest.strip_prefix($prefix) {
                return Some(VarOp::$variant { word: w.to_string(), colon: $colon });
            }
        };
    }
    op!(":-", Default, true);
    op!(":=", AssignDefault, true);
    op!(":?", ErrorIfUnset, true);
    op!(":+", AltIfSet, true);
    // ${V:offset} / ${V:offset:length} -- only reached once the four ":x"
    // operators above have failed to match, so `${V:-x}` (default) still
    // takes precedence over reading `-x` as a substring offset; bash's own
    // disambiguation rule is that a *negative* offset needs a space before
    // it (`${V: -1}`) for exactly this reason.
    if let Some(spec) = rest.strip_prefix(':') {
        return Some(match spec.find(':') {
            Some(idx) => {
                VarOp::Substring { offset: spec[..idx].to_string(), length: Some(spec[idx + 1..].to_string()) }
            }
            None => VarOp::Substring { offset: spec.to_string(), length: None },
        });
    }
    if let Some(spec) = rest.strip_prefix("//") {
        let (pattern, repl) = split_replace_spec(spec);
        return Some(VarOp::Replace { pattern, repl, global: true, anchor: ReplaceAnchor::None });
    }
    if let Some(spec) = rest.strip_prefix('/') {
        if let Some(pat_rest) = spec.strip_prefix('#') {
            let (pattern, repl) = split_replace_spec(pat_rest);
            return Some(VarOp::Replace { pattern, repl, global: false, anchor: ReplaceAnchor::Start });
        }
        if let Some(pat_rest) = spec.strip_prefix('%') {
            let (pattern, repl) = split_replace_spec(pat_rest);
            return Some(VarOp::Replace { pattern, repl, global: false, anchor: ReplaceAnchor::End });
        }
        let (pattern, repl) = split_replace_spec(spec);
        return Some(VarOp::Replace { pattern, repl, global: false, anchor: ReplaceAnchor::None });
    }
    if let Some(w) = rest.strip_prefix("##") {
        return Some(VarOp::RemovePrefix { pattern: w.to_string(), longest: true });
    }
    if let Some(w) = rest.strip_prefix('#') {
        return Some(VarOp::RemovePrefix { pattern: w.to_string(), longest: false });
    }
    if let Some(w) = rest.strip_prefix("%%") {
        return Some(VarOp::RemoveSuffix { pattern: w.to_string(), longest: true });
    }
    if let Some(w) = rest.strip_prefix('%') {
        return Some(VarOp::RemoveSuffix { pattern: w.to_string(), longest: false });
    }
    op!("-", Default, false);
    op!("=", AssignDefault, false);
    op!("?", ErrorIfUnset, false);
    op!("+", AltIfSet, false);
    if let Some(w) = rest.strip_prefix("^^") {
        return Some(VarOp::CaseConvert { pattern: w.to_string(), upper: true, all: true });
    }
    if let Some(w) = rest.strip_prefix('^') {
        return Some(VarOp::CaseConvert { pattern: w.to_string(), upper: true, all: false });
    }
    if let Some(w) = rest.strip_prefix(",,") {
        return Some(VarOp::CaseConvert { pattern: w.to_string(), upper: false, all: true });
    }
    if let Some(w) = rest.strip_prefix(',') {
        return Some(VarOp::CaseConvert { pattern: w.to_string(), upper: false, all: false });
    }
    None
}

// Splits "pattern/repl" on the first unescaped '/' (a backslash-escaped
// `\/` in the pattern, e.g. matching a literal path separator, doesn't end
// it). No trailing '/' at all (`${V/pat}`) means "replace with nothing"
// (deletion), matching bash.
fn split_replace_spec(spec: &str) -> (String, String) {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => i += 2,
            '/' => {
                return (chars[..i].iter().collect(), chars[i + 1..].iter().collect());
            }
            _ => i += 1,
        }
    }
    (spec.to_string(), String::new())
}

// Parses the text captured between `${` and `}`. `${#VAR}` (length) is
// checked first since '#' would otherwise be read as the RemovePrefix
// operator on an empty name.
fn parse_brace_content(inner: &str) -> BraceContent {
    if let Some(rest) = inner.strip_prefix('!') {
        if let Some((name, index, after)) = try_split_array(rest) {
            if after.is_empty() && (index == "@" || index == "*") {
                return BraceContent::ArrayKeys(name);
            }
        } else if !rest.is_empty() {
            let mut chars = rest.chars();
            let first = chars.next().unwrap();
            if (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_') {
                return BraceContent::Indirect(rest.to_string());
            }
        }
    }
    if let Some(rest) = inner.strip_prefix('#') {
        if let Some((name, index, after)) = try_split_array(rest) {
            if after.is_empty() {
                return BraceContent::ArrayLength(name, index);
            }
        }
        if !rest.is_empty() {
            return BraceContent::Op(rest.to_string(), VarOp::Length);
        }
    }
    if let Some((name, index, after)) = try_split_array(inner) {
        if after.is_empty() {
            return BraceContent::ArrayIndex(name, index);
        }
        if let Some(op) = parse_operator_suffix(after) {
            return BraceContent::ArrayOp(name, index, op);
        }
        // Unrecognized operator after an array index: fall through to the
        // scalar path below, which will end up treating the whole thing as
        // a best-effort literal name (same fallback as the scalar case).
    }

    let mut name_end = 0;
    let mut chars = inner.char_indices();
    if let Some((_, c)) = chars.next() {
        if matches!(c, '?' | '#' | '@' | '*' | '$' | '!' | '-') {
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
    if let Some(op) = parse_operator_suffix(rest) {
        return BraceContent::Op(name, op);
    }

    // Unrecognized operator syntax: fall back to treating the whole thing
    // as a literal (best-effort; will just look up a likely-nonexistent
    // variable name rather than crashing).
    BraceContent::Plain(inner.to_string())
}

// Brace expansion ({a,b,c}, {1..5}, {a..e}, and nested combinations).
// Scoped to fully literal, unquoted words only (see the tokenize() call
// site) -- bash technically allows brace expansion alongside other
// expansions in the same word, but that's a rare combination not worth the
// added complexity here.
fn brace_expand(s: &str) -> Vec<String> {
    if let Some((prefix, items, suffix)) = split_brace_group(s) {
        let mut result = Vec::new();
        for item in items {
            for combined in brace_expand(&format!("{}{}{}", prefix, item, suffix)) {
                result.push(combined);
            }
        }
        if !result.is_empty() {
            return result;
        }
    }
    vec![s.to_string()]
}

fn split_brace_group(s: &str) -> Option<(String, Vec<String>, String)> {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.iter().position(|&c| c == '{')?;
    let mut depth = 0;
    let mut end = None;
    for (i, &c) in chars.iter().enumerate().skip(start) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    let prefix: String = chars[..start].iter().collect();
    let inner: String = chars[start + 1..end].iter().collect();
    let suffix: String = chars[end + 1..].iter().collect();

    if let Some(items) = try_brace_range(&inner) {
        return Some((prefix, items, suffix));
    }
    let items = split_top_level_commas(&inner);
    if items.len() > 1 {
        return Some((prefix, items, suffix));
    }
    None
}

fn try_brace_range(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.splitn(2, "..").collect();
    if parts.len() != 2 {
        return None;
    }
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let items = if a <= b { (a..=b).collect::<Vec<_>>() } else { (b..=a).rev().collect::<Vec<_>>() };
        return Some(items.into_iter().map(|n| n.to_string()).collect());
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (parts[0].chars().collect(), parts[1].chars().collect());
    if ca.len() == 1 && cb.len() == 1 {
        let (a, b) = (ca[0] as u32, cb[0] as u32);
        let range: Vec<u32> = if a <= b { (a..=b).collect() } else { (b..=a).rev().collect() };
        return Some(range.into_iter().filter_map(char::from_u32).map(String::from).collect());
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    items.push(cur);
    items
}
