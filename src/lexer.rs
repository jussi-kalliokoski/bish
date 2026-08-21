// Hand-rolled tokenizer for bish's shell grammar: pipelines, redirects,
// sequencing, quoting, control flow, $VAR/command/arithmetic expansion,
// globbing, and here-docs.

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
    // source -- read_word always flushes buf and emits one of these at a
    // quote/escape boundary rather than merging the quoted text into the
    // surrounding Chunk::Str, so a quoted/escaped metachar next to real
    // unquoted glob or regex syntax in the same word stays distinguishable
    // (see expand_word_split's pattern-building and expand_regex_operand
    // in exec.rs, both of which escape LiteralStr but not Chunk::Str).
    // Treated identically to Chunk::Str everywhere else that just wants
    // the plain text value (word-splitting, serialization, command-name
    // lookup): see the `Chunk::Str(s) | Chunk::LiteralStr(s)` arms.
    LiteralStr(String),
}

// The operand ("word"/"pattern") of each variant is kept as raw source text
// and re-expanded/glob-matched at evaluation time (see exec.rs), same
// deferred-parsing approach as Sub/Arith. `colon` distinguishes `${V:-x}`
// (triggers on unset-or-empty) from `${V-x}` (triggers on unset only) --
// eval_var_op (exec.rs) branches on it via var_is_set vs is_empty.
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
    // Word(chunks, globbable) -- globbable is a fast-path flag, true only
    // if the word had no quoting/escaping/expansion at all (see
    // read_word); expand_words (exec.rs) takes a cheap literal-string path
    // for these. Words that don't qualify (partly quoted, or containing
    // any expansion) still glob-expand -- expand_word_split builds a
    // second, per-chunk-escaped pattern string alongside the split fields,
    // escaping each LiteralStr chunk (quoted/escaped source text) so only
    // its literal characters are excluded from matching, while Chunk::Str
    // and expansion results stay real pattern syntax.
    Word(Vec<Chunk>, bool),
    Pipe,
    And,
    Or,
    Semi,
    DSemi,
    // `;&` (case: fall through to the next arm's body unconditionally) and
    // `;;&` (case: keep testing subsequent patterns instead of stopping).
    SemiAmp,
    DSemiAmp,
    Amp,
    RedirOut { append: bool },
    RedirIn,
    RedirErr { append: bool },
    RedirBoth { append: bool },
    DupErrToOut,
    // Arbitrary-fd redirects: `N>`/`N>>`/`N<`/`N>&M`/`N<&M`. The fd=2 forms
    // (`2>`, `2>>`, `2>&1`) stay on the dedicated tokens above -- this is
    // for every other explicit fd number, plus `2<`/`2>&M` (M != 1), which
    // the fd=2 fast path above doesn't cover.
    RedirFdOut { fd: u32, append: bool },
    RedirFdIn { fd: u32 },
    RedirFdDup { fd: u32, target: u32 },
    // `[N]>&WORD` / `[N]<&WORD` where WORD isn't a bare literal digit
    // sequence -- e.g. `>&"$fd"`, needed for anything using a dynamically-
    // obtained fd (like a coproc's array entries) rather than a fixed
    // number known at parse time. Same shape as RedirOut/RedirFdOut/etc:
    // just the operator, with the target read as the *next* token via the
    // ordinary word-lexing fallback, consumed by the parser's expect_word.
    // `fd` is the source side (1 for a bare `>&`, 0 for a bare `<&`, or the
    // explicit leading digit for `N>&`/`N<&`); the target word is expanded
    // and parsed as the target fd number at redirect-resolution time.
    RedirDupWord { fd: u32 },
    // `[N]>&-` / `[N]<&-`: closes fd N (1/0 if no leading digit).
    RedirFdClose { fd: u32 },
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
    KwSelect,
    KwCoproc,
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
        "select" => Tok::KwSelect,
        "coproc" => Tok::KwCoproc,
        "in" => Tok::KwIn,
        "case" => Tok::KwCase,
        "esac" => Tok::KwEsac,
        "function" => Tok::KwFunction,
        "[[" => Tok::KwLBracket2,
        "]]" => Tok::KwRBracket2,
        _ => return None,
    })
}

// Reverses `keyword` above -- the literal source text a keyword token
// stands for. This lexer has no notion of "command position": any bare
// word exactly matching one of these names always becomes its keyword
// token regardless of where it appears (see the tokenize() call site
// above), so `echo function` or `case $x in if)` would otherwise fail to
// parse at all, since "function"/"if" show up as Tok::KwFunction/Tok::KwIf
// instead of an ordinary Tok::Word there. The parser uses this everywhere
// it's already past the point where a bare word could legitimately start
// a *new* command (mid-argument-list, a for/select wordlist, a case
// pattern/subject, inside `[[ ]]`, a redirect target, ...) to fall back to
// treating an accidentally-keyword-shaped word as the plain literal it
// was always meant to be -- confirmed against real bash, which allows
// exactly this (`for x in if while do done; do echo $x; done` prints each
// of those words verbatim).
pub(crate) fn keyword_text(tok: &Tok) -> Option<&'static str> {
    Some(match tok {
        Tok::KwIf => "if",
        Tok::KwThen => "then",
        Tok::KwElif => "elif",
        Tok::KwElse => "else",
        Tok::KwFi => "fi",
        Tok::KwWhile => "while",
        Tok::KwUntil => "until",
        Tok::KwDo => "do",
        Tok::KwDone => "done",
        Tok::KwFor => "for",
        Tok::KwSelect => "select",
        Tok::KwCoproc => "coproc",
        Tok::KwIn => "in",
        Tok::KwCase => "case",
        Tok::KwEsac => "esac",
        Tok::KwFunction => "function",
        Tok::KwLBracket2 => "[[",
        Tok::KwRBracket2 => "]]",
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
    // How many chars have been consumed via advance() so far -- a CHAR
    // index (matching Vec<char>/bishedit's HighlightSpan contract), not a
    // byte offset. Exists purely for the syntax-highlighting path
    // (tokenize_spanned and the raw-capture-span instrumentation below);
    // ordinary tokenize()/execution never reads it. Always tracked
    // (unconditionally, not behind a flag) since that's simpler than
    // threading an opt-in through every call site, and the cost is one
    // usize increment per char already being consumed regardless.
    pos: usize,
    // Half-open char-index ranges, one per chunk pushed while reading a
    // word EXCEPT plain Chunk::Str, in the same left-to-right order as the
    // chunks themselves -- i.e. one entry per Chunk::{LiteralStr, Sub,
    // Arith, Var, VarExpand, ArrayVar, ArrayLength, ArrayVarExpand,
    // Indirect, ArrayKeys, ProcSubIn, ProcSubOut}, pushed by
    // capture_balanced_parens/capture_backtick, by push_var's own two
    // non-recursive dispatch branches (the ${...} brace-content path and
    // the bare $NAME fallback), or by read_word's own three LiteralStr
    // sites (single-quoted, double-quoted, and single-char backslash-
    // escape runs). Chunk::Str is the one exception: its source span is
    // always recoverable without a side-channel entry, since it's a
    // verbatim, unescaped copy of source chars (never true for LiteralStr,
    // whose decoded length can differ from its source span whenever a
    // double-quote escape sequence -- \$, \\, \" -- consumed 2 source
    // chars for 1 decoded char).
    // For Sub/Arith/ProcSubIn/ProcSubOut the range excludes the
    // construct's own delimiters ($( ), backtick, <( )/>( )) since that's
    // the raw text a highlighter recursively re-lexes. For the terminal
    // (non-recursive) LiteralStr/${...}/bare $NAME chunks, the range
    // similarly excludes their own delimiters (quotes, `{`/`}` braces, or
    // nothing to exclude for the bare $NAME/backslash-escape forms) --
    // same "delimiters excluded" convention either way, even though these
    // never get recursed into.
    // capture_double_paren doesn't push its own -- it just wraps a
    // capture_balanced_parens call whose span already covers the same raw
    // text -- so pushing there too would desync the one-span-per-chunk
    // correspondence.
    // Always populated (no Option-gating), like `pos`: ordinary
    // tokenize()/execution never reads this either.
    raw_capture_spans: Vec<std::ops::Range<usize>>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            chars: src.chars().peekable(),
            pending_heredocs: Vec::new(),
            bracket2_depth: 0,
            regex_operand_next: false,
            pos: 0,
            raw_capture_spans: Vec::new(),
        }
    }

    // The one and only place that actually consumes a char from `chars` --
    // every other call site in this file goes through this instead of
    // calling `self.chars.next()` directly, purely so `pos` (see its own
    // doc comment) stays accurate everywhere spans get recorded, not just
    // at the top-level tokenize() loop. Behaviorally identical to a bare
    // `self.chars.next()` in every other respect.
    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    pub fn tokenize(mut self) -> Result<Vec<Tok>, String> {
        let mut toks = Vec::new();
        loop {
            self.skip_spaces();
            if self.regex_operand_next {
                self.regex_operand_next = false;
                if self.chars.peek().is_some() {
                    let (word, plain) = self.read_word(true, false)?;
                    // Real bash rejects an unquoted `<`/`>` immediately
                    // following (no space) a `=~` regex operand with a
                    // syntax error, confirmed empirically -- read_word's
                    // own relaxed-mode word-breaking just stops *before*
                    // the character without consuming it, which would
                    // otherwise silently truncate the pattern there and
                    // let the rest of the line be reinterpreted as
                    // ordinary redirect/word tokens instead of erroring.
                    if let Some(c @ ('<' | '>')) = self.chars.peek().copied() {
                        return Err(format!("syntax error near unexpected token `{}'", c));
                    }
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
                        self.advance();
                    }
                }
                Some('\n') => {
                    self.advance();
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
                    self.advance();
                    if self.chars.peek().copied() == Some('|') {
                        self.advance();
                        toks.push(Tok::Or);
                    } else {
                        toks.push(Tok::Pipe);
                    }
                }
                Some('&') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        toks.push(Tok::And);
                    } else if self.chars.peek().copied() == Some('>') {
                        self.advance();
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.advance();
                        }
                        toks.push(Tok::RedirBoth { append });
                    } else {
                        toks.push(Tok::Amp);
                    }
                }
                Some(';') => {
                    self.advance();
                    if self.chars.peek().copied() == Some(';') {
                        self.advance();
                        if self.chars.peek().copied() == Some('&') {
                            self.advance();
                            toks.push(Tok::DSemiAmp);
                        } else {
                            toks.push(Tok::DSemi);
                        }
                    } else if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        toks.push(Tok::SemiAmp);
                    } else {
                        toks.push(Tok::Semi);
                    }
                }
                Some('(') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_double_paren()?;
                        toks.push(Tok::Arith(raw));
                    } else {
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Subshell(raw));
                    }
                }
                Some(')') => {
                    self.advance();
                    toks.push(Tok::RParen);
                }
                Some('{') if self.next_char_is_word_boundary() => {
                    self.advance();
                    toks.push(Tok::LBrace);
                }
                Some('}') if self.next_char_is_word_boundary() => {
                    self.advance();
                    toks.push(Tok::RBrace);
                }
                Some('>') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Word(vec![Chunk::ProcSubOut { raw }], false));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        toks.push(self.lex_dup_target(1));
                        continue;
                    }
                    let append = self.chars.peek().copied() == Some('>');
                    if append {
                        self.advance();
                    }
                    toks.push(Tok::RedirOut { append });
                }
                Some('<') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_balanced_parens()?;
                        toks.push(Tok::Word(vec![Chunk::ProcSubIn { raw }], false));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        toks.push(self.lex_dup_target(0));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('<') {
                        self.advance();
                        if self.chars.peek().copied() == Some('<') {
                            self.advance();
                            toks.push(Tok::HereString);
                        } else {
                            let strip_tabs = self.chars.peek().copied() == Some('-');
                            if strip_tabs {
                                self.advance();
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
                    self.advance(); // '2'
                    self.advance(); // '>'
                    if self.chars.peek().copied() == Some('&') && self.peek2() == Some('1') {
                        self.advance(); // '&'
                        self.advance(); // '1'
                        toks.push(Tok::DupErrToOut);
                    } else {
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.advance();
                        }
                        toks.push(Tok::RedirErr { append });
                    }
                }
                // Any other explicit fd number (0,1,3-9,...; also covers
                // `2<`/`2>&M` for M != 1, which the fd=2 fast path above
                // doesn't handle) immediately followed by `>`/`<` with no
                // intervening whitespace -- e.g. `3>file`, `4<&0`. A digit
                // with no immediately-following `>`/`<` (an ordinary
                // numeric word, or a redirect target's own value) isn't
                // touched by this arm and falls through to read_word below
                // as before.
                Some(c) if c.is_ascii_digit() && self.peek_numbered_fd_redirect().is_some() => {
                    let (fd, ndigits) = self.peek_numbered_fd_redirect().unwrap();
                    for _ in 0..ndigits {
                        self.advance();
                    }
                    match self.advance() {
                        Some('>') => {
                            if self.chars.peek().copied() == Some('&') {
                                self.advance();
                                toks.push(self.lex_dup_target(fd));
                            } else {
                                let append = self.chars.peek().copied() == Some('>');
                                if append {
                                    self.advance();
                                }
                                toks.push(Tok::RedirFdOut { fd, append });
                            }
                        }
                        Some('<') => {
                            if self.chars.peek().copied() == Some('&') {
                                self.advance();
                                toks.push(self.lex_dup_target(fd));
                            } else {
                                toks.push(Tok::RedirFdIn { fd });
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => {
                    let (word, plain) = self.read_word(false, false)?;
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

    // Called with the '&' of a `>&`/`<&` dup-redirect already consumed.
    // `fd` is the source side, already determined by the caller (1/0 for
    // the bare forms, or the explicit leading digit). Produces the plain
    // literal-target token when a bare digit sequence follows (the common,
    // already-well-tested case); otherwise pushes the word-based token and
    // leaves the target itself to be lexed as an ordinary following word,
    // same as every other redirect operator's target.
    fn lex_dup_target(&mut self, fd: u32) -> Tok {
        if self.chars.peek().copied() == Some('-') {
            self.advance();
            Tok::RedirFdClose { fd }
        } else if self.chars.peek().copied().is_some_and(|c| c.is_ascii_digit()) {
            Tok::RedirFdDup { fd, target: self.read_fd_number() }
        } else {
            Tok::RedirDupWord { fd }
        }
    }

    fn peek_is_fd2_redirect(&self) -> bool {
        let mut it = self.chars.clone();
        if it.next() != Some('2') {
            return false;
        }
        it.next() == Some('>')
    }

    // Looks ahead for a digit run immediately followed by `>`/`<` (no
    // consumption). Returns the fd number and how many digit characters it
    // spanned -- needed separately from the number itself since a leading
    // zero (`007>`) would otherwise desync consumption from `fd.to_string()`.
    fn peek_numbered_fd_redirect(&self) -> Option<(u32, usize)> {
        let mut it = self.chars.clone();
        let mut digits = String::new();
        while let Some(c) = it.clone().next() {
            if c.is_ascii_digit() {
                digits.push(c);
                it.next();
            } else {
                break;
            }
        }
        if digits.is_empty() {
            return None;
        }
        match it.next() {
            Some('>') | Some('<') => digits.parse::<u32>().ok().map(|n| (n, digits.len())),
            _ => None,
        }
    }

    // Consumes a target fd number after `>&`/`<&` (e.g. the `1` in `3>&1`).
    // Only called once lex_dup_target has already ruled out the `-` (close)
    // case, so a malformed redirect with no digits here just yields fd 0
    // rather than hard-failing the lexer, matching this codebase's
    // best-effort stance elsewhere.
    fn read_fd_number(&mut self) -> u32 {
        let mut s = String::new();
        while let Some(c) = self.chars.peek().copied() {
            if c.is_ascii_digit() {
                s.push(c);
                self.advance();
            } else {
                break;
            }
        }
        s.parse().unwrap_or(0)
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
        // The opening '(' this call is scoped to was already consumed by
        // the caller, so `pos` right now is exactly the raw content's
        // first char index -- see raw_capture_spans's own doc comment.
        let start = self.pos;
        let mut depth = 1;
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err("unterminated '('".to_string()),
                Some('(') => {
                    depth += 1;
                    s.push('(');
                }
                Some(')') => {
                    depth -= 1;
                    if depth == 0 {
                        // `pos` was just bumped past the closing ')' by the
                        // advance() above, so pos - 1 is that ')' 's own
                        // index -- the content's exclusive end.
                        self.raw_capture_spans.push(start..self.pos - 1);
                        break;
                    }
                    s.push(')');
                }
                Some('\'') => {
                    s.push('\'');
                    loop {
                        match self.advance() {
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
                        match self.advance() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => {
                                s.push('"');
                                break;
                            }
                            Some('\\') => {
                                s.push('\\');
                                if let Some(n) = self.advance() {
                                    s.push(n);
                                }
                            }
                            Some(c) => s.push(c),
                        }
                    }
                }
                Some('\\') => {
                    s.push('\\');
                    if let Some(n) = self.advance() {
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
        // No separate raw_capture_spans push here: the inner
        // capture_balanced_parens call below already records the exact
        // same raw text's span (this function doesn't append anything to
        // `raw` afterward, just consumes the second closing paren).
        let raw = self.capture_balanced_parens()?;
        if self.chars.peek().copied() == Some(')') {
            self.advance();
        }
        Ok(raw)
    }

    fn capture_backtick(&mut self) -> Result<String, String> {
        // Same reasoning as capture_balanced_parens's own `start`: the
        // opening backtick is already consumed by the caller.
        let start = self.pos;
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err("unterminated '`'".to_string()),
                Some('`') => {
                    self.raw_capture_spans.push(start..self.pos - 1);
                    break;
                }
                Some('\\') => match self.chars.peek().copied() {
                    Some(n) if n == '`' || n == '\\' || n == '$' => {
                        self.advance();
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
            match self.advance() {
                None => return Err("unterminated $'...'".to_string()),
                Some('\'') => break,
                Some('\\') => match self.advance() {
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
                self.advance();
                let mut s = String::new();
                while let Some(c) = self.advance() {
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
                    self.advance();
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
                match self.advance() {
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
                self.advance();
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
    // `literal_ws`: true for a `${...}` operator word (pattern/replacement/
    // default text) parsed out of band via `parse_expansion_word` -- that
    // text is a single semantic unit with no field splitting inside it, so
    // an unquoted space there is pattern content (`${s#hello }`), not a
    // word boundary. False (the normal command-line case) still breaks the
    // word on whitespace.
    fn read_word(&mut self, relaxed: bool, literal_ws: bool) -> Result<(Vec<Chunk>, bool), String> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut buf = String::new();
        let mut plain = true;

        // Tilde expansion: only the bare `~` / `~/...` form at the very
        // start of a word (no `~user` lookup).
        if self.chars.peek().copied() == Some('~') {
            let mut probe = self.chars.clone();
            probe.next();
            if matches!(probe.peek().copied(), None | Some('/') | Some(' ') | Some('\t') | Some('\n')) {
                self.advance();
                chunks.push(Chunk::Var { name: "HOME".to_string(), quoted: false });
            }
        }

        loop {
            match self.chars.peek().copied() {
                None => break,
                Some(c) if !literal_ws && (c == ' ' || c == '\t' || c == '\n') => break,
                // extglob: @(...) !(...) +(...) *(...) ?(...) -- the '('
                // immediately follows one of these prefix chars (already in
                // buf, having fallen through the default char arm below) --
                // so bish always recognizes it as a pattern group rather
                // than a subshell/word boundary, unlike real bash which
                // gates this behind `shopt -s extglob` (see glob.rs).
                Some('(') if matches!(buf.chars().last(), Some('@') | Some('!') | Some('+') | Some('*') | Some('?')) => {
                    self.advance();
                    let inner = self.capture_balanced_parens()?;
                    buf.push('(');
                    buf.push_str(&inner);
                    buf.push(')');
                }
                Some('|') | Some('(') | Some(')') if relaxed => {
                    buf.push(self.advance().unwrap());
                }
                Some('|') | Some('&') | Some(';') | Some('<') | Some('>') | Some('#') | Some('(') | Some(')') => break,
                Some('\'') => {
                    plain = false;
                    self.advance();
                    let lit_start = self.pos;
                    let mut lit = String::new();
                    loop {
                        match self.advance() {
                            None => return Err("unterminated single quote".to_string()),
                            Some('\'') => break,
                            Some(c) => lit.push(c),
                        }
                    }
                    // Always flush buf and push the quoted run as its own
                    // LiteralStr rather than merging it into buf -- merging
                    // would make a quoted metachar indistinguishable from
                    // real unquoted glob/regex syntax elsewhere in the same
                    // word (see LiteralStr's doc comment).
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    if !lit.is_empty() {
                        // pos was just bumped past the closing quote, so
                        // pos - 1 is that quote's own index -- same
                        // "excludes delimiters" convention as everywhere
                        // else in raw_capture_spans.
                        self.raw_capture_spans.push(lit_start..self.pos - 1);
                        chunks.push(Chunk::LiteralStr(lit));
                    }
                }
                Some('"') => {
                    plain = false;
                    self.advance();
                    let mut lit = String::new();
                    // Tracks where the *current* literal run (since the
                    // last flush) started -- unlike single quotes,
                    // double-quote escapes ($/`/\ prefixed with a
                    // backslash) consume 2 source chars but produce 1
                    // decoded char, so `lit`'s own length can't be used to
                    // recover its source span; this has to be tracked by
                    // position, not length, across every flush point below.
                    let mut lit_start = self.pos;
                    loop {
                        match self.advance() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => break,
                            Some('\\') => match self.chars.peek().copied() {
                                Some(n) if n == '"' || n == '\\' || n == '$' => {
                                    self.advance();
                                    lit.push(n);
                                }
                                _ => lit.push('\\'),
                            },
                            Some('$') => {
                                // `$` was just consumed by the advance()
                                // above, so pos - 1 is its own index -- the
                                // literal run's exclusive end.
                                let dollar_pos = self.pos - 1;
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    self.raw_capture_spans.push(lit_start..dollar_pos);
                                    chunks.push(Chunk::LiteralStr(std::mem::take(&mut lit)));
                                }
                                // Pass `lit` (now empty), not `buf`: if
                                // nothing valid follows the `$`, push_var's
                                // fallback writes a literal "$" back into
                                // whatever buffer it's given -- it must
                                // land in the quoted accumulator here, or a
                                // trailing "$" (e.g. a regex end-anchor
                                // written *inside* quotes) would wrongly
                                // lose its quoted-ness.
                                let produced = self.push_var(&mut chunks, &mut lit, true)?;
                                // If push_var fell back to a literal "$"
                                // (produced == false, 0 chars consumed by
                                // push_var itself), the next literal run
                                // actually starts AT the '$' -- not after
                                // it -- since that's exactly what just got
                                // written back into `lit`.
                                lit_start = if produced { self.pos } else { dollar_pos };
                            }
                            Some('`') => {
                                let backtick_pos = self.pos - 1;
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    self.raw_capture_spans.push(lit_start..backtick_pos);
                                    chunks.push(Chunk::LiteralStr(std::mem::take(&mut lit)));
                                }
                                chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: true });
                                lit_start = self.pos;
                            }
                            Some(c) => lit.push(c),
                        }
                    }
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    if !lit.is_empty() {
                        // pos was just bumped past the closing '"'.
                        self.raw_capture_spans.push(lit_start..self.pos - 1);
                        chunks.push(Chunk::LiteralStr(lit));
                    }
                }
                Some('\\') => {
                    plain = false;
                    // The whole 2-char escape (backslash + escaped char)
                    // maps to LiteralStr's 1-char decoded value -- the span
                    // covers both source chars, not just the escaped one.
                    let start = self.pos;
                    self.advance();
                    if let Some(n) = self.advance() {
                        if !buf.is_empty() {
                            chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                        }
                        self.raw_capture_spans.push(start..self.pos);
                        chunks.push(Chunk::LiteralStr(n.to_string()));
                    }
                }
                Some('$') if self.peek2() == Some('\'') => {
                    plain = false;
                    self.advance(); // '$'
                    self.advance(); // '\''
                    buf.push_str(&self.read_ansi_c_string()?);
                }
                Some('$') => {
                    self.advance();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    if self.push_var(&mut chunks, &mut buf, false)? {
                        plain = false;
                    }
                }
                Some('`') => {
                    plain = false;
                    self.advance();
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: false });
                }
                Some(c) => {
                    self.advance();
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
            self.advance();
            if self.chars.peek().copied() == Some('(') {
                self.advance();
                chunks.push(Chunk::Arith { raw: self.capture_double_paren()?, quoted });
            } else {
                chunks.push(Chunk::Sub { raw: self.capture_balanced_parens()?, quoted });
            }
            return Ok(true);
        }
        if self.chars.peek().copied() == Some('{') {
            self.advance();
            // Content span excludes the braces, same convention as
            // capture_balanced_parens -- see raw_capture_spans's own doc
            // comment. Recorded once we know a real chunk will be pushed
            // (the empty-name case below returns early with none).
            let start = self.pos;
            let mut inner = String::new();
            // Unlike capture_balanced_parens/capture_backtick, this loop
            // has no unterminated-input error path (matches this
            // function's pre-existing best-effort behavior) -- so `closed`
            // distinguishes "stopped at a real '}'" (span excludes it,
            // same delimiter-excluded convention as elsewhere) from "ran
            // out of input first" (span includes every char actually
            // consumed, matching what `inner` itself already contains).
            let mut closed = false;
            while let Some(c) = self.chars.peek().copied() {
                self.advance();
                if c == '}' {
                    closed = true;
                    break;
                }
                inner.push(c);
            }
            let span = start..(if closed { self.pos - 1 } else { self.pos });
            match parse_brace_content(&inner) {
                BraceContent::Plain(name) => {
                    if name.is_empty() {
                        buf.push('$');
                        return Ok(false);
                    } else {
                        self.raw_capture_spans.push(span);
                        chunks.push(Chunk::Var { name, quoted });
                    }
                }
                BraceContent::Op(name, op) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::VarExpand { name, op, quoted });
                }
                BraceContent::ArrayIndex(name, index) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::ArrayVar { name, index, quoted });
                }
                BraceContent::ArrayLength(name, index) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::ArrayLength { name, index });
                }
                BraceContent::ArrayOp(name, index, op) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::ArrayVarExpand { name, index, op, quoted });
                }
                BraceContent::Indirect(name) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::Indirect { name, quoted });
                }
                BraceContent::ArrayKeys(name) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::ArrayKeys { name, quoted });
                }
            }
            return Ok(true);
        }
        // Bare $NAME: no delimiters at all, so the span is just the name's
        // own char range.
        let start = self.pos;
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
            Ok(false)
        } else {
            self.raw_capture_spans.push(start..self.pos);
            chunks.push(Chunk::Var { name, quoted });
            Ok(true)
        }
    }

    fn read_var_name(&mut self) -> String {
        if matches!(self.chars.peek().copied(), Some('?') | Some('#') | Some('@') | Some('*') | Some('$') | Some('!') | Some('-')) {
            self.advance().unwrap().to_string()
        } else {
            let mut name = String::new();
            while let Some(c) = self.chars.peek().copied() {
                if c.is_alphanumeric() || c == '_' {
                    name.push(c);
                    self.advance();
                } else {
                    break;
                }
            }
            name
        }
    }
}

// One lexed item from tokenize_spanned, carrying its own source range --
// the highlighting-only sibling of a plain Tok. Deliberately not folded
// into Tok itself as a `Tok::Comment` variant: keeping comments out of Tok
// entirely means parser.rs's exhaustive matches over Tok never need to
// account for a variant the real grammar never produces.
// Unused outside of this file's own tests until the highlighter (a later
// stage) becomes the first real caller -- same "build ahead of wiring"
// staging bishedit's own modules use, just narrowly scoped here rather
// than a module-wide allow, since lexer.rs is otherwise live execution
// code where a stray real dead-code warning should still surface.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum SpannedItem {
    Tok(Tok, std::ops::Range<usize>),
    Comment(std::ops::Range<usize>),
}

#[allow(dead_code)]
pub struct SpannedResult {
    pub items: Vec<SpannedItem>,
    pub raw_capture_spans: Vec<std::ops::Range<usize>>,
    // On a lex error, `items` still holds everything collected before the
    // error point instead of being discarded (unlike tokenize()'s own
    // Result<_, String>) -- a syntactically incomplete line-in-progress is
    // the common case while typing in bish's single-line editor, so it
    // must degrade to "highlighted up to the error point, plain after",
    // not go fully colorless.
    pub error: Option<String>,
}

// A duplicate of tokenize()'s own dispatch loop, not a modification of it
// -- zero risk to real command execution, since nothing here is on that
// code path. Kept in lockstep with tokenize() by hand; see that function
// for arm-by-arm rationale (redirect/heredoc/brace-expansion edge cases
// etc.), which isn't re-explained here.
#[allow(dead_code)]
pub fn tokenize_spanned(src: &str) -> SpannedResult {
    let mut lexer = Lexer::new(src);
    let mut items: Vec<SpannedItem> = Vec::new();
    let mut error: Option<String> = None;
    'outer: loop {
        lexer.skip_spaces();
        let start = lexer.pos;
        if lexer.regex_operand_next {
            lexer.regex_operand_next = false;
            if lexer.chars.peek().is_some() {
                match lexer.read_word(true, false) {
                    Ok((word, plain)) => {
                        if let Some(c @ ('<' | '>')) = lexer.chars.peek().copied() {
                            error = Some(format!("syntax error near unexpected token `{}'", c));
                            break;
                        }
                        items.push(SpannedItem::Tok(Tok::Word(word, plain), start..lexer.pos));
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            continue;
        }
        match lexer.chars.peek().copied() {
            None => break,
            Some('#') => {
                while let Some(c) = lexer.chars.peek().copied() {
                    if c == '\n' {
                        break;
                    }
                    lexer.advance();
                }
                items.push(SpannedItem::Comment(start..lexer.pos));
            }
            Some('\n') => {
                lexer.advance();
                items.push(SpannedItem::Tok(Tok::Newline, start..lexer.pos));
                if !lexer.pending_heredocs.is_empty() {
                    let pending = std::mem::take(&mut lexer.pending_heredocs);
                    for (tok_idx, delim, strip_tabs, expand) in pending {
                        let body = lexer.capture_heredoc_body(&delim, strip_tabs);
                        let chunks = if expand {
                            match expand_heredoc_chunks(&body) {
                                Ok(c) => c,
                                Err(e) => {
                                    error = Some(e);
                                    break 'outer;
                                }
                            }
                        } else {
                            vec![Chunk::Str(body)]
                        };
                        if let SpannedItem::Tok(t, _) = &mut items[tok_idx] {
                            *t = Tok::HereDoc(chunks);
                        }
                    }
                }
            }
            Some('|') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('|') {
                    lexer.advance();
                    items.push(SpannedItem::Tok(Tok::Or, start..lexer.pos));
                } else {
                    items.push(SpannedItem::Tok(Tok::Pipe, start..lexer.pos));
                }
            }
            Some('&') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    items.push(SpannedItem::Tok(Tok::And, start..lexer.pos));
                } else if lexer.chars.peek().copied() == Some('>') {
                    lexer.advance();
                    let append = lexer.chars.peek().copied() == Some('>');
                    if append {
                        lexer.advance();
                    }
                    items.push(SpannedItem::Tok(Tok::RedirBoth { append }, start..lexer.pos));
                } else {
                    items.push(SpannedItem::Tok(Tok::Amp, start..lexer.pos));
                }
            }
            Some(';') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some(';') {
                    lexer.advance();
                    if lexer.chars.peek().copied() == Some('&') {
                        lexer.advance();
                        items.push(SpannedItem::Tok(Tok::DSemiAmp, start..lexer.pos));
                    } else {
                        items.push(SpannedItem::Tok(Tok::DSemi, start..lexer.pos));
                    }
                } else if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    items.push(SpannedItem::Tok(Tok::SemiAmp, start..lexer.pos));
                } else {
                    items.push(SpannedItem::Tok(Tok::Semi, start..lexer.pos));
                }
            }
            Some('(') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('(') {
                    lexer.advance();
                    match lexer.capture_double_paren() {
                        Ok(raw) => items.push(SpannedItem::Tok(Tok::Arith(raw), start..lexer.pos)),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                } else {
                    match lexer.capture_balanced_parens() {
                        Ok(raw) => items.push(SpannedItem::Tok(Tok::Subshell(raw), start..lexer.pos)),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
            }
            Some(')') => {
                lexer.advance();
                items.push(SpannedItem::Tok(Tok::RParen, start..lexer.pos));
            }
            Some('{') if lexer.next_char_is_word_boundary() => {
                lexer.advance();
                items.push(SpannedItem::Tok(Tok::LBrace, start..lexer.pos));
            }
            Some('}') if lexer.next_char_is_word_boundary() => {
                lexer.advance();
                items.push(SpannedItem::Tok(Tok::RBrace, start..lexer.pos));
            }
            Some('>') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('(') {
                    lexer.advance();
                    match lexer.capture_balanced_parens() {
                        Ok(raw) => items.push(SpannedItem::Tok(
                            Tok::Word(vec![Chunk::ProcSubOut { raw }], false),
                            start..lexer.pos,
                        )),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                    continue;
                }
                if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    let tok = lexer.lex_dup_target(1);
                    items.push(SpannedItem::Tok(tok, start..lexer.pos));
                    continue;
                }
                let append = lexer.chars.peek().copied() == Some('>');
                if append {
                    lexer.advance();
                }
                items.push(SpannedItem::Tok(Tok::RedirOut { append }, start..lexer.pos));
            }
            Some('<') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('(') {
                    lexer.advance();
                    match lexer.capture_balanced_parens() {
                        Ok(raw) => items.push(SpannedItem::Tok(
                            Tok::Word(vec![Chunk::ProcSubIn { raw }], false),
                            start..lexer.pos,
                        )),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                    continue;
                }
                if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    let tok = lexer.lex_dup_target(0);
                    items.push(SpannedItem::Tok(tok, start..lexer.pos));
                    continue;
                }
                if lexer.chars.peek().copied() == Some('<') {
                    lexer.advance();
                    if lexer.chars.peek().copied() == Some('<') {
                        lexer.advance();
                        items.push(SpannedItem::Tok(Tok::HereString, start..lexer.pos));
                    } else {
                        let strip_tabs = lexer.chars.peek().copied() == Some('-');
                        if strip_tabs {
                            lexer.advance();
                        }
                        lexer.skip_spaces();
                        let (delim, expand) = lexer.read_heredoc_delimiter();
                        let tok_idx = items.len();
                        items.push(SpannedItem::Tok(Tok::HereDoc(vec![Chunk::Str(String::new())]), start..lexer.pos));
                        lexer.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                    }
                } else {
                    items.push(SpannedItem::Tok(Tok::RedirIn, start..lexer.pos));
                }
            }
            Some('2') if lexer.peek_is_fd2_redirect() => {
                lexer.advance(); // '2'
                lexer.advance(); // '>'
                if lexer.chars.peek().copied() == Some('&') && lexer.peek2() == Some('1') {
                    lexer.advance(); // '&'
                    lexer.advance(); // '1'
                    items.push(SpannedItem::Tok(Tok::DupErrToOut, start..lexer.pos));
                } else {
                    let append = lexer.chars.peek().copied() == Some('>');
                    if append {
                        lexer.advance();
                    }
                    items.push(SpannedItem::Tok(Tok::RedirErr { append }, start..lexer.pos));
                }
            }
            Some(c) if c.is_ascii_digit() && lexer.peek_numbered_fd_redirect().is_some() => {
                let (fd, ndigits) = lexer.peek_numbered_fd_redirect().unwrap();
                for _ in 0..ndigits {
                    lexer.advance();
                }
                match lexer.advance() {
                    Some('>') => {
                        if lexer.chars.peek().copied() == Some('&') {
                            lexer.advance();
                            let tok = lexer.lex_dup_target(fd);
                            items.push(SpannedItem::Tok(tok, start..lexer.pos));
                        } else {
                            let append = lexer.chars.peek().copied() == Some('>');
                            if append {
                                lexer.advance();
                            }
                            items.push(SpannedItem::Tok(Tok::RedirFdOut { fd, append }, start..lexer.pos));
                        }
                    }
                    Some('<') => {
                        if lexer.chars.peek().copied() == Some('&') {
                            lexer.advance();
                            let tok = lexer.lex_dup_target(fd);
                            items.push(SpannedItem::Tok(tok, start..lexer.pos));
                        } else {
                            items.push(SpannedItem::Tok(Tok::RedirFdIn { fd }, start..lexer.pos));
                        }
                    }
                    _ => unreachable!(),
                }
            }
            _ => match lexer.read_word(false, false) {
                Ok((word, plain)) => {
                    if plain {
                        if let [Chunk::Str(s)] = word.as_slice() {
                            if let Some(kw) = keyword(s) {
                                match kw {
                                    Tok::KwLBracket2 => lexer.bracket2_depth += 1,
                                    Tok::KwRBracket2 => lexer.bracket2_depth = lexer.bracket2_depth.saturating_sub(1),
                                    _ => {}
                                }
                                items.push(SpannedItem::Tok(kw, start..lexer.pos));
                                continue;
                            }
                            if lexer.bracket2_depth > 0 && s == "=~" {
                                lexer.regex_operand_next = true;
                            }
                            if s.contains('{') {
                                let expanded = brace_expand(s);
                                if expanded.len() > 1 || expanded[0] != *s {
                                    // Every token expanded from one
                                    // `{a,b,c}` source region shares that
                                    // region's full original span rather
                                    // than being subdivided -- the whole
                                    // expression highlights as one unit.
                                    let end = lexer.pos;
                                    for e in expanded {
                                        items.push(SpannedItem::Tok(Tok::Word(vec![Chunk::Str(e)], true), start..end));
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                    items.push(SpannedItem::Tok(Tok::Word(word, plain), start..lexer.pos));
                }
                Err(e) => {
                    error = Some(e);
                    break;
                }
            },
        }
    }
    SpannedResult { items, raw_capture_spans: lexer.raw_capture_spans, error }
}

// Parses the operator-word text of a `${...}` construct (the pattern in
// `${v#pat}`/`${v%pat}`, the replacement/pattern in `${v/pat/rep}`, the
// word in `${v:-word}`, etc.) for VarOp evaluation. Uses read_word's
// `literal_ws` mode: nested expansions and quoting still work, but unlike
// normal command-line tokenization, unquoted whitespace is literal pattern
// content rather than a word boundary -- `${s#hello }` must strip exactly
// "hello " (trailing space included), not just "hello".
pub(crate) fn parse_expansion_word(raw: &str) -> Vec<Chunk> {
    Lexer::new(raw).read_word(false, true).map(|(chunks, _)| chunks).unwrap_or_else(|_| vec![Chunk::Str(raw.to_string())])
}

// Expands $VAR / $(...) / `...` / $((...)) within an expansion-enabled
// here-doc body. Can't reuse read_word directly since a heredoc body
// doesn't quote-process (' and " are literal there, not quote operators)
// and must keep embedded newlines as literal content, not word separators.
// Backslash escaping here mirrors double-quote semantics (only \$, \\, \`
// are special). Expansions are marked `quoted: true` since the body is
// used verbatim as stdin text, never word-split.
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
    // {start..end} or {start..end..step} -- step's sign doesn't affect
    // direction (bash always steps toward `end` from `start`; the range's
    // own ordering decides ascending vs descending), so only its magnitude
    // is used.
    let parts: Vec<&str> = inner.splitn(3, "..").collect();
    if parts.len() < 2 {
        return None;
    }
    let step: usize = match parts.get(2) {
        Some(s) => match s.parse::<i64>() {
            Ok(0) | Err(_) => return None,
            Ok(s) => s.unsigned_abs() as usize,
        },
        None => 1,
    };
    if let (Ok(a), Ok(b)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let items: Vec<i64> =
            if a <= b { (a..=b).step_by(step).collect() } else { (b..=a).rev().step_by(step).collect() };
        return Some(items.into_iter().map(|n| n.to_string()).collect());
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (parts[0].chars().collect(), parts[1].chars().collect());
    if ca.len() == 1 && cb.len() == 1 {
        let (a, b) = (ca[0] as u32, cb[0] as u32);
        let range: Vec<u32> =
            if a <= b { (a..=b).step_by(step).collect() } else { (b..=a).rev().step_by(step).collect() };
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

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: a `${var#pattern}`-style pattern is one semantic word,
    // not a command line -- an unquoted trailing space is pattern content,
    // not a separator to be dropped. parse_expansion_word previously went
    // through the normal tokenize() path, which splits on whitespace and
    // silently lost it.
    #[test]
    fn parse_expansion_word_preserves_trailing_space() {
        assert_eq!(parse_expansion_word("hello "), vec![Chunk::Str("hello ".to_string())]);
    }

    #[test]
    fn parse_expansion_word_preserves_internal_whitespace_runs() {
        assert_eq!(parse_expansion_word("a  b"), vec![Chunk::Str("a  b".to_string())]);
    }

    #[test]
    fn parse_expansion_word_still_expands_vars_around_literal_spaces() {
        assert_eq!(
            parse_expansion_word("hello $v"),
            vec![Chunk::Str("hello ".to_string()), Chunk::Var { name: "v".to_string(), quoted: false }]
        );
    }

    fn spanned_text<'a>(src: &'a str, r: &std::ops::Range<usize>) -> &'a str {
        let chars: Vec<char> = src.chars().collect();
        &src[chars[..r.start].iter().map(|c| c.len_utf8()).sum()
            ..chars[..r.end].iter().map(|c| c.len_utf8()).sum()]
    }

    #[test]
    fn tokenize_spanned_comment_gets_its_own_span_to_end_of_line() {
        let src = "echo hi # a comment";
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);
        let comment = res.items.iter().find_map(|it| match it {
            SpannedItem::Comment(r) => Some(r.clone()),
            _ => None,
        });
        let r = comment.expect("expected a Comment item");
        assert_eq!(spanned_text(src, &r), "# a comment");
    }

    #[test]
    fn tokenize_spanned_unterminated_paren_keeps_partial_items_and_sets_error() {
        let src = "echo $(foo";
        let res = tokenize_spanned(src);
        assert!(res.error.is_some());
        // "echo" was fully lexed before the unterminated $( was hit.
        assert!(res.items.iter().any(|it| matches!(it, SpannedItem::Tok(Tok::Word(w, true), _) if w == &vec![Chunk::Str("echo".to_string())])));
    }

    #[test]
    fn tokenize_spanned_nested_quote_inside_substitution_recurses_via_raw_capture_spans() {
        // The user's own motivating example: the single-quoted string
        // inside $(...) must be independently re-lexable from its own
        // recorded raw_capture_spans entry, not flattened into one span.
        let src = "echo \"yooo, $(printf 'hello %' world)\"";
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);
        // Two entries now: the leading "yooo, " double-quoted literal run
        // (LiteralStr instrumentation) followed by the $(...) substitution
        // itself, in that left-to-right order.
        assert_eq!(res.raw_capture_spans.len(), 2);
        assert_eq!(spanned_text(src, &res.raw_capture_spans[0]), "yooo, ");
        let sub_span = &res.raw_capture_spans[1];
        let inner_src = spanned_text(src, sub_span);
        assert_eq!(inner_src, "printf 'hello %' world");
        // Re-lexing just the captured raw text independently finds its own
        // quoted string -- this is the recursion mechanism the future
        // BashHighlighter relies on.
        let inner = tokenize_spanned(inner_src);
        assert_eq!(inner.error, None);
        assert!(inner.items.iter().any(|it| matches!(
            it,
            SpannedItem::Tok(Tok::Word(w, false), _)
                if w.iter().any(|c| matches!(c, Chunk::LiteralStr(s) if s == "hello %"))
        )));
    }

    #[test]
    fn tokenize_spanned_arithmetic_gets_one_flat_span_no_recursion_artifacts() {
        let src = "echo $((1 + 2))";
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);
        // $((...)) is captured via capture_double_paren wrapping a single
        // capture_balanced_parens call -- exactly one raw-capture span,
        // not two (capture_double_paren must not push its own on top).
        assert_eq!(res.raw_capture_spans.len(), 1);
        assert_eq!(spanned_text(src, &res.raw_capture_spans[0]), "1 + 2");
        let word = res.items.iter().find_map(|it| match it {
            SpannedItem::Tok(Tok::Word(w, _), _) if matches!(w.as_slice(), [Chunk::Arith { .. }]) => Some(w.clone()),
            _ => None,
        });
        match word.expect("expected a Word token with a Chunk::Arith").as_slice() {
            [Chunk::Arith { raw, .. }] => assert_eq!(raw, "1 + 2"),
            other => panic!("expected a single flat Chunk::Arith, got {:?}", other),
        }
    }

    // Parity test for Stage 3's push_var instrumentation: a word built
    // entirely from expansion constructs (no literal Str/LiteralStr text
    // between them) should produce exactly one raw_capture_spans entry
    // per chunk, in the same order, each substring matching what that
    // chunk semantically represents. A missed instrumentation site would
    // desync the two lists (either a chunk with no span, or an extra span
    // with no corresponding chunk) -- this test would catch either.
    #[test]
    fn push_var_instrumentation_keeps_raw_capture_spans_in_parity_with_chunks() {
        let src = "$a${b}$(echo c)$((1+2))${#arr[@]}";
        let mut lexer = Lexer::new(src);
        let (chunks, _) = lexer.read_word(false, false).unwrap();

        assert_eq!(chunks.len(), 5, "unexpected chunk count: {:?}", chunks);
        assert_eq!(
            lexer.raw_capture_spans.len(),
            chunks.len(),
            "span count must match chunk count: spans={:?} chunks={:?}",
            lexer.raw_capture_spans,
            chunks
        );

        let expected_substrings = ["a", "b", "echo c", "1+2", "#arr[@]"];
        for (span, expected) in lexer.raw_capture_spans.iter().zip(expected_substrings) {
            assert_eq!(spanned_text(src, span), expected);
        }

        match chunks.as_slice() {
            [
                Chunk::Var { name: n0, .. },
                Chunk::Var { name: n1, .. },
                Chunk::Sub { raw: r2, .. },
                Chunk::Arith { raw: r3, .. },
                Chunk::ArrayLength { name: n4, index: i4 },
            ] => {
                assert_eq!(n0, "a");
                assert_eq!(n1, "b");
                assert_eq!(r2, "echo c");
                assert_eq!(r3, "1+2");
                assert_eq!(n4, "arr");
                assert_eq!(i4, "@");
            }
            other => panic!("unexpected chunk shape: {:?}", other),
        }
    }

    // LiteralStr instrumentation (added alongside the highlighter, since
    // it's what lets quoted-string spans get colored): covers single
    // quotes (1:1, no escapes), double quotes (escapes shrink source
    // chars to fewer decoded chars, so this must track by position, not
    // by `lit`'s own length), and the standalone backslash-escape case.
    #[test]
    fn literal_str_spans_cover_source_text_not_decoded_length() {
        let src = r#"echo 'a b'"c\"d"\x"#;
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);

        // Chunk::LiteralStr sources, in order: 'a b' -> "a b" (single
        // quotes, 1:1); "c\"d" -> the double-quoted run containing an
        // escaped quote, decoded to `c"d` (4 source chars -> 3 decoded);
        // \x -> decoded to `x` (2 source chars, backslash + x -> 1
        // decoded char).
        let expected = ["a b", "c\"d", "x"];
        assert_eq!(res.raw_capture_spans.len(), expected.len(), "spans: {:?}", res.raw_capture_spans);

        // The spans must cover the *source* text (surrounding quotes
        // excluded, but the escaping backslash included where present),
        // which differs from the decoded chunk value in both cases above.
        let expected_source = ["a b", "c\\\"d", "\\x"];
        for (span, expected) in res.raw_capture_spans.iter().zip(expected_source) {
            assert_eq!(spanned_text(src, span), expected);
        }

        let literal_strs: Vec<&String> = res
            .items
            .iter()
            .filter_map(|it| match it {
                SpannedItem::Tok(Tok::Word(w, _), _) => Some(w),
                _ => None,
            })
            .flatten()
            .filter_map(|c| match c {
                Chunk::LiteralStr(s) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(literal_strs, expected.iter().collect::<Vec<_>>());
    }
}
