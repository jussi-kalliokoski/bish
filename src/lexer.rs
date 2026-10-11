// The tokenizer for bish's shell grammar: pipelines, redirects,
// sequencing, quoting, control flow, expansions, globbing and heredocs.

/// How a literal run of text was spelled. The parse tree keeps what a
/// word means; this is the part of how it was written that `declare -f`
/// needs to print it back as written (`trap "echo t" EXIT`, not
/// `trap 'echo t' EXIT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quoting {
    /// `'...'`: nothing inside is special.
    Single,
    /// One literal run inside `"..."`. A double-quoted word is usually
    /// several chunks, written back inside one pair of quotes.
    Double,
    /// A single character behind a backslash, as in `c\ d`.
    Escape,
    /// `$'...'`, whose escapes were resolved here and are put back to be
    /// written.
    Dollar,
}

/// How a heredoc was introduced, for writing one back out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HereDocSpelling {
    pub delimiter: String,
    /// `<<'E'` or `<<"E"`: the body is literal.
    pub quoted: bool,
    /// `<<-`, which strips leading tabs from the body and delimiter line.
    pub strip_tabs: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Chunk {
    Str(String),
    // A `~` prefix at the start of a word. `name` is what followed: empty
    // for `~` and `~/...`, `+`/`-` for `$PWD`/`$OLDPWD`, or a user name.
    // Resolved at expansion, which needs the shell and /etc/passwd.
    Tilde { name: String },
    // `$name` or `${name}`. `braced` doesn't change the expansion; it's for
    // writing the word back as written, as bash's `declare -f` does. Every
    // `quoted` here means inside `"..."` or otherwise not word-split: `"$x"`
    // never splits, bare `$x` does.
    Var { name: String, quoted: bool, braced: bool },
    // The raw text of a `$( )` or `` ` ` `` substitution, parsed and run at
    // expansion. `backticks` only matters for writing it back, as bash's
    // `declare -f` does.
    Sub { raw: String, quoted: bool, backticks: bool },
    // Raw source text of a $((...)) arithmetic expansion.
    Arith { raw: String, quoted: bool },
    VarExpand { name: String, op: VarOp, quoted: bool },
    // `${name[index]}`. `index` is raw: `@`/`*` (every element, a field each
    // like `$@`/`$*`) or arithmetic evaluated at access (`${arr[i+1]}`).
    ArrayVar { name: String, index: String, quoted: bool },
    // `${#name[index]}`: with `@`/`*` the array's length, else the element's
    // length.
    ArrayLength { name: String, index: String },
    // `${name[index]:-word}` and friends: `VarExpand`'s operators on an
    // element.
    ArrayVarExpand { name: String, index: String, op: VarOp, quoted: bool },
    // `${!name}`: the variable named by `name`'s value, one level, as in
    // bash.
    Indirect { name: String, quoted: bool },
    // `${!name[@]}` / `${!name[*]}`: the array's indices (decimal) or keys,
    // a field each like `${name[@]}`.
    ArrayKeys { name: String, quoted: bool },
    // `${!prefix*}` / `${!prefix@}`: every variable name starting with
    // `prefix`, sorted and deduplicated; `at` is which spelling, splitting
    // as `@` does for `ArrayKeys`.
    VarNamesMatchingPrefix { prefix: String, at: bool, quoted: bool },
    // `<(cmd)` / `>(cmd)`: see exec.rs's `run_proc_sub_in` and
    // `run_proc_sub_out`.
    ProcSubIn { raw: String },
    ProcSubOut { raw: String },
    // Literal text that was quoted or backslash-escaped. `read_word` emits
    // one at each quote or escape boundary rather than merging it into a
    // `Chunk::Str`, so a quoted metacharacter beside real glob or regex
    // syntax stays literal (`expand_word_split`'s pattern and
    // `expand_regex_operand` escape `LiteralStr` only). Elsewhere it's the
    // same as `Str`.
    LiteralStr(String, Quoting),
}

// Which redirect a `{name}...` form is, kept apart from the numbered
// tokens because the number doesn't exist until the shell allocates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarFdKind {
    In,
    Out { append: bool, clobber: bool },
    InOut,
    // `{v}<&N` / `{v}>&N` duplicate N onto a new descriptor named `v`;
    // `{v}<&-` / `{v}>&-` close the one `v` names. The word after the
    // operator says which.
    Dup,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VarOp {
    Length,
    Default { word: String, colon: bool },
    AssignDefault { word: String, colon: bool },
    ErrorIfUnset { word: String, colon: bool },
    AltIfSet { word: String, colon: bool },
    RemovePrefix { pattern: String, longest: bool },
    RemoveSuffix { pattern: String, longest: bool },
    // `${V^pat}`, `${V^^pat}`, `${V,pat}`, `${V,,pat}`: `upper` is the
    // direction, `all` every char or just the first. An empty pattern
    // (`${V^^}`) matches any character; otherwise each is matched as a
    // `case` pattern.
    CaseConvert { pattern: String, upper: bool, all: bool },
    // `${V:offset}` / `${V:offset:length}`, raw arithmetic evaluated at
    // expansion (`${V:$i:$n}`). A negative offset counts from the end, and a
    // negative length is an end position from the end, as in bash (see
    // `substring_expand`).
    Substring { offset: String, length: Option<String> },
    // `${V/pat/repl}`, `${V//pat/repl}`, `${V/#pat/repl}`, `${V/%pat/repl}`:
    // `global` is first or all matches, `anchor` pins the match to the
    // start or end. Raw text, expanded when matching.
    Replace { pattern: String, repl: String, global: bool, anchor: ReplaceAnchor },
    // bash's parameter transformations (`${v@Q}` and the rest).
    Transform(TransformKind),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplaceAnchor {
    None,
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransformKind {
    // Quoted so the result can be reused as input (`${v@Q}`).
    Quote,
    // `${v@U}`, `${v@L}` and `${v@u}` (first character only).
    Upper,
    UpperFirst,
    Lower,
    // Backslash escapes expanded as `$'...'` does (`${v@E}`).
    Escape,
    // A statement that recreates the variable with its attributes
    // (`${v@A}`); see exec.rs's `transform_attributes`.
    Attributes,
    // The attribute letters (`${v@a}`): "rx" for readonly and exported.
    AttributeFlags,
    // `${arr[@]@K}`: "key value key value ..." with double-quoted values,
    // as `declare -p` prints elements. A scalar or a single element behaves
    // like `@Q`, as in bash.
    KeyValue,
    // The value expanded as a prompt string (`${v@P}`): `\u`, `\h`, `\w`,
    // `\$` and the rest, from the shell's current state.
    Prompt,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // `Word(chunks, globbable)`: `globbable` is true only for a word with no
    // quoting, escaping or expansion, for `expand_words`'s fast path. Other
    // words still glob: `expand_word_split` builds a pattern alongside the
    // fields with `LiteralStr` chunks escaped.
    Word(Vec<Chunk>, bool),
    Pipe,
    And,
    Or,
    Semi,
    DSemi,
    // `;&` (case: run the next arm's body too) and `;;&` (case: keep testing
    // the following patterns).
    SemiAmp,
    DSemiAmp,
    Amp,
    // `clobber` is `>|`, overwriting under `set -C`; there's no `>>|`.
    RedirOut { append: bool, clobber: bool },
    RedirIn,
    // `<>`: open the target for reading and writing, on fd 0 or the given
    // digit (`RedirFdInOut`). Mostly for `/dev/tcp/HOST/PORT` (see
    // `dev_socket_file`), where `<` and `>` would open two connections.
    RedirInOut,
    RedirErr { append: bool, clobber: bool },
    RedirBoth { append: bool },
    DupErrToOut,
    // `N>`, `N>>`, `N<`, `N<>`, `N>&M`, `N<&M` for any other fd. `2>`, `2>>`
    // and `2>&1` have the tokens above; this covers the rest, including
    // `2<`, `2<>` and `2>&M` for M other than 1.
    RedirFdOut { fd: u32, append: bool, clobber: bool },
    // `{name}<`, `{name}>`, `{name}>>`, `{name}<>`: the shell picks a free fd
    // and assigns it to `name` (see `Redirect::VarFd`).
    RedirVarFd { var: String, kind: VarFdKind },
    RedirFdIn { fd: u32 },
    RedirFdInOut { fd: u32 },
    // `input` is whether it was `<&`, not `>&`; the same dup2, but
    // `declare -f` writes it back as written.
    RedirFdDup { fd: u32, target: u32, input: bool },
    // `[N]>&WORD` / `[N]<&WORD` where WORD isn't plain digits (`>&"$fd"`,
    // for a coproc's fd). Just the operator: the target is the next word,
    // read by the parser's `expect_word` and parsed as a number when the
    // redirect is resolved. `fd` is the source: 1 for `>&`, 0 for `<&`, or
    // the leading digit.
    RedirDupWord { fd: u32, input: bool },
    // `[N]>&-` / `[N]<&-`: closes fd N (1 or 0 by default).
    RedirFdClose { fd: u32 },
    HereString,
    // `N<<<` / `N<<`: the descriptor a here-string or heredoc goes to,
    // pushed just before its `HereString`/`HereDoc` token, so a numbered
    // heredoc's body is filled by the same code as any other.
    RedirFdHere { fd: u32 },
    // A heredoc: pushed as a placeholder at `<<WORD` and patched with the
    // expanded body at the line's newline (see `pending_heredocs`). Its
    // spelling (delimiter, quoting, `<<-`) is kept for `declare -f`.
    HereDoc(Vec<Chunk>, HereDocSpelling),
    Newline,
    LBrace,
    RBrace,
    RParen,
    // The raw text of a `( )` subshell. `attached` means the `(` touched the
    // previous token: `arr=(a b)` is an array, `arr= (a b)` a syntax error.
    Subshell { raw: String, attached: bool },
    // The raw text of a standalone `(( ))` arithmetic command.
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
    // `[[` and `]]` are tokens so the parser takes everything between as one
    // test; otherwise `&&` and `||` inside would split it into commands.
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

// The source text a keyword token stands for. The lexer doesn't know
// command position, so a bare word matching a keyword always becomes
// its token, and `echo function` or `case $x in if)` would fail to
// parse. Wherever a new command can't start (arguments, a for list, a
// case pattern, `[[ ]]`, a redirect target), the parser uses this to
// read the token as the word it is, as bash does
// (`for x in if while do done; ...`).
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
    // (index into `toks`, delimiter, strip tabs, expand body) for each
    // `<<`/`<<-` on this line, resolved at the line's newline (see the body
    // capture below).
    pending_heredocs: Vec<(usize, String, bool, bool)>,
    // How many `[[` are open, so `=~`'s word reading applies only inside.
    bracket2_depth: u32,
    // Set after the `=~` word inside `[[ ]]`: the next word is read with
    // `read_word(relaxed: true)`. See the `=~` handling in `tokenize`.
    regex_operand_next: bool,
    // Chars consumed through `advance`: a char index, as `HighlightSpan`
    // uses, not a byte offset. Only highlighting reads it
    // (`tokenize_spanned`); it's always kept, which costs an increment per
    // char.
    pos: usize,
    // Char ranges, one per chunk pushed while reading a word other than
    // `Chunk::Str`, in chunk order: `LiteralStr`, `Sub`, `Arith`, `Var`,
    // `VarExpand`, `ArrayVar`, `ArrayLength`, `ArrayVarExpand`, `Indirect`,
    // `ArrayKeys`, `ProcSubIn`, `ProcSubOut`. `Str` needs none: it's a
    // verbatim copy of the source, unlike `LiteralStr`, whose escapes make
    // its decoded length differ.
    //
    // Ranges exclude the construct's delimiters (`$( )`, backticks, quotes,
    // braces), which for `Sub`, `Arith` and process substitutions is the raw
    // text a highlighter re-lexes. `capture_double_paren` pushes none, since
    // its inner `capture_balanced_parens` already did. Always kept, like
    // `pos`.
    raw_capture_spans: Vec<std::ops::Range<usize>>,
    // The 1-based line of the next char, bumped by `advance` on `\n`, so
    // `tokenize` can tag tokens with their line (`push_tok!`) for the
    // debugger's breakpoints; the AST otherwise has no positions.
    line: usize,
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
            line: 1,
        }
    }

    // The only place a char is consumed, so `pos` stays accurate wherever
    // spans are recorded.
    fn advance(&mut self) -> Option<char> {
        let c = self.chars.next();
        if c.is_some() {
            self.pos += 1;
        }
        if c == Some('\n') {
            self.line += 1;
        }
        c
    }

    pub fn tokenize(mut self) -> Result<Vec<(Tok, usize)>, String> {
        let mut toks = Vec::new();
        // Tags every token pushed here with its starting line, so the
        // parser's AST (unlike `tokenize_spanned`'s) has line numbers.
        macro_rules! push_tok {
            ($tok:expr) => {
                toks.push(($tok, self.line))
            };
        }
        loop {
            // Whether this token touches the previous one: only `(` after
            // `name=` cares (see `Tok::Subshell`).
            let attached = !self.skip_spaces() && !toks.is_empty();
            if self.regex_operand_next {
                self.regex_operand_next = false;
                if self.chars.peek().is_some() {
                    let (word, plain) = self.read_word(true, false)?;
                    // bash rejects an unquoted `<` or `>` right after a
                    // `=~` operand. `read_word`'s relaxed mode stops before
                    // it without consuming it, so otherwise the pattern
                    // would be cut short and the rest read as redirects.
                    if let Some(c @ ('<' | '>')) = self.chars.peek().copied() {
                        return Err(format!("syntax error near unexpected token `{}'", c));
                    }
                    push_tok!(Tok::Word(word, plain));
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
                    push_tok!(Tok::Newline);
                    // A `<<WORD` body is the lines after this one, so the
                    // operator was tokenized as a placeholder, and the body
                    // is captured at this newline and patched in.
                    if !self.pending_heredocs.is_empty() {
                        let pending = std::mem::take(&mut self.pending_heredocs);
                        for (tok_idx, delim, strip_tabs, expand) in pending {
                            let body = self.capture_heredoc_body(&delim, strip_tabs);
                            let chunks = if expand { expand_heredoc_chunks(&body)? } else { vec![Chunk::Str(body)] };
                            let spelling = HereDocSpelling { delimiter: delim, quoted: !expand, strip_tabs };
                            toks[tok_idx].0 = Tok::HereDoc(chunks, spelling);
                        }
                    }
                }
                Some('|') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('|') {
                        self.advance();
                        push_tok!(Tok::Or);
                    } else {
                        push_tok!(Tok::Pipe);
                    }
                }
                Some('&') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        push_tok!(Tok::And);
                    } else if self.chars.peek().copied() == Some('>') {
                        self.advance();
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.advance();
                        }
                        push_tok!(Tok::RedirBoth { append });
                    } else {
                        push_tok!(Tok::Amp);
                    }
                }
                Some(';') => {
                    self.advance();
                    if self.chars.peek().copied() == Some(';') {
                        self.advance();
                        if self.chars.peek().copied() == Some('&') {
                            self.advance();
                            push_tok!(Tok::DSemiAmp);
                        } else {
                            push_tok!(Tok::DSemi);
                        }
                    } else if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        push_tok!(Tok::SemiAmp);
                    } else {
                        push_tok!(Tok::Semi);
                    }
                }
                Some('(') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_double_paren()?;
                        push_tok!(Tok::Arith(raw));
                    } else {
                        let raw = self.capture_balanced_parens()?;
                        push_tok!(Tok::Subshell { raw, attached });
                    }
                }
                Some(')') => {
                    self.advance();
                    push_tok!(Tok::RParen);
                }
                // `{name}>file` and friends, before the brace-group arm: a
                // group's `{` is followed by a word boundary, this one by a
                // name and a redirect.
                Some('{') if self.var_fd_redirect().is_some() => {
                    let (var, kind, consumed) = self.var_fd_redirect().unwrap();
                    for _ in 0..consumed {
                        self.advance();
                    }
                    push_tok!(Tok::RedirVarFd { var, kind });
                }
                Some('{') if self.next_char_is_word_boundary() => {
                    self.advance();
                    push_tok!(Tok::LBrace);
                }
                Some('}') if self.next_char_is_word_boundary() => {
                    self.advance();
                    push_tok!(Tok::RBrace);
                }
                Some('>') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_balanced_parens()?;
                        push_tok!(Tok::Word(vec![Chunk::ProcSubOut { raw }], false));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        push_tok!(self.lex_dup_target(1, false));
                        continue;
                    }
                    let append = self.chars.peek().copied() == Some('>');
                    if append {
                        self.advance();
                    }
                    // `>|`: past `set -C`.
                    let clobber = !append && self.chars.peek().copied() == Some('|');
                    if clobber {
                        self.advance();
                    }
                    push_tok!(Tok::RedirOut { append, clobber });
                }
                Some('<') => {
                    self.advance();
                    if self.chars.peek().copied() == Some('(') {
                        self.advance();
                        let raw = self.capture_balanced_parens()?;
                        push_tok!(Tok::Word(vec![Chunk::ProcSubIn { raw }], false));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('&') {
                        self.advance();
                        push_tok!(self.lex_dup_target(0, true));
                        continue;
                    }
                    if self.chars.peek().copied() == Some('>') {
                        self.advance();
                        push_tok!(Tok::RedirInOut);
                        continue;
                    }
                    if self.chars.peek().copied() == Some('<') {
                        self.advance();
                        if self.chars.peek().copied() == Some('<') {
                            self.advance();
                            push_tok!(Tok::HereString);
                        } else {
                            let strip_tabs = self.chars.peek().copied() == Some('-');
                            if strip_tabs {
                                self.advance();
                            }
                            self.skip_spaces();
                            let (delim, expand) = self.read_heredoc_delimiter();
                            let tok_idx = toks.len();
                            push_tok!(Tok::HereDoc(vec![Chunk::Str(String::new())], HereDocSpelling::default()));
                            self.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                        }
                    } else {
                        push_tok!(Tok::RedirIn);
                    }
                }
                Some('2') if self.peek_is_fd2_redirect() => {
                    self.advance(); // '2'
                    self.advance(); // '>'
                    if self.chars.peek().copied() == Some('&') && self.peek2() == Some('1') {
                        self.advance(); // '&'
                        self.advance(); // '1'
                        push_tok!(Tok::DupErrToOut);
                    } else if self.chars.peek().copied() == Some('&') {
                        // `2>&3`, `2>&$fd`, `2>&-`. This arm comes before
                        // the numbered-fd path, so it handles all of them.
                        self.advance(); // '&'
                        push_tok!(self.lex_dup_target(2, false));
                    } else {
                        let append = self.chars.peek().copied() == Some('>');
                        if append {
                            self.advance();
                        }
                        let clobber = !append && self.chars.peek().copied() == Some('|');
                        if clobber {
                            self.advance();
                        }
                        push_tok!(Tok::RedirErr { append, clobber });
                    }
                }
                // Any other fd number (0, 1, 3-9, and `2<`) directly
                // followed by `>` or `<`: `3>file`, `4<&0`. A digit not
                // followed by one is an ordinary word.
                Some(c) if c.is_ascii_digit() && self.peek_numbered_fd_redirect().is_some() => {
                    let (fd, ndigits) = self.peek_numbered_fd_redirect().unwrap();
                    for _ in 0..ndigits {
                        self.advance();
                    }
                    match self.advance() {
                        Some('>') => {
                            if self.chars.peek().copied() == Some('&') {
                                self.advance();
                                push_tok!(self.lex_dup_target(fd, false));
                            } else {
                                let append = self.chars.peek().copied() == Some('>');
                                if append {
                                    self.advance();
                                }
                                let clobber = !append && self.chars.peek().copied() == Some('|');
                                if clobber {
                                    self.advance();
                                }
                                push_tok!(Tok::RedirFdOut { fd, append, clobber });
                            }
                        }
                        Some('<') => {
                            if self.chars.peek().copied() == Some('&') {
                                self.advance();
                                push_tok!(self.lex_dup_target(fd, true));
                            } else if self.chars.peek().copied() == Some('>') {
                                self.advance();
                                push_tok!(Tok::RedirFdInOut { fd });
                            } else if self.chars.peek().copied() == Some('<') {
                                // `N<<<` and `N<<DELIM`, which would
                                // otherwise lex as `N<` and a stray `<<`.
                                self.advance();
                                push_tok!(Tok::RedirFdHere { fd });
                                if self.chars.peek().copied() == Some('<') {
                                    self.advance();
                                    push_tok!(Tok::HereString);
                                } else {
                                    let strip_tabs = self.chars.peek().copied() == Some('-');
                                    if strip_tabs {
                                        self.advance();
                                    }
                                    self.skip_spaces();
                                    let (delim, expand) = self.read_heredoc_delimiter();
                                    let tok_idx = toks.len();
                                    push_tok!(Tok::HereDoc(vec![Chunk::Str(String::new())], HereDocSpelling::default()));
                                    self.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                                }
                            } else {
                                push_tok!(Tok::RedirFdIn { fd });
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
                                push_tok!(kw);
                                continue;
                            }
                            if self.bracket2_depth > 0 && s == "=~" {
                                self.regex_operand_next = true;
                            }
                            if s.contains('{') {
                                let expanded = brace_expand(s);
                                if expanded.len() > 1 || expanded[0] != *s {
                                    for e in expanded {
                                        push_tok!(Tok::Word(vec![Chunk::Str(e)], true));
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                    push_tok!(Tok::Word(word, plain));
                }
            }
        }
        Ok(toks)
    }

    // Called after the `&` of `>&`/`<&`; `fd` is the source. Bare digits
    // give the literal-target token; otherwise the word-based token, with
    // the target lexed as the next word, like any redirect target.
    fn lex_dup_target(&mut self, fd: u32, input: bool) -> Tok {
        if self.chars.peek().copied() == Some('-') {
            self.advance();
            Tok::RedirFdClose { fd }
        } else if self.chars.peek().copied().is_some_and(|c| c.is_ascii_digit()) {
            Tok::RedirFdDup { fd, target: self.read_fd_number(), input }
        } else {
            Tok::RedirDupWord { fd, input }
        }
    }

    fn peek_is_fd2_redirect(&self) -> bool {
        let mut it = self.chars.clone();
        if it.next() != Some('2') {
            return false;
        }
        it.next() == Some('>')
    }

    // Looks ahead for digits followed by `>` or `<` without consuming.
    // Returns the fd and how many digits it took, since a leading zero
    // (`007>`) differs from `fd.to_string()`.
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

    // Reads the target fd after `>&`/`<&` (the `1` in `3>&1`). The `-`
    // case is already ruled out; with no digits this gives 0 rather than
    // failing.
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

    // With the next char the `[` after an identifier: is this `NAME[...]=`
    // (or `+=`), whose subscript may hold spaces, rather than a glob class
    // or the `[` builtin? Looks ahead on a clone.
    fn subscript_is_an_assignment(&self) -> bool {
        let mut it = self.chars.clone();
        if it.next() != Some('[') {
            return false;
        }
        let mut depth = 1;
        for c in it.by_ref() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                // A newline or metacharacter means it was never a
                // subscript; stop rather than scan the script for a `]`.
                '\n' | ';' | '|' | '&' => return false,
                _ => {}
            }
        }
        if depth != 0 {
            return false;
        }
        match it.next() {
            Some('=') => true,
            Some('+') => it.next() == Some('='),
            _ => false,
        }
    }

    fn peek2(&self) -> Option<char> {
        let mut it = self.chars.clone();
        it.next();
        it.next()
    }

    // `{name}<`, `{name}>`, `{name}>>`, `{name}<>`: the variable-fd
    // redirect. Returns the name, the form and how many chars to consume;
    // `None` for every other `{`.
    fn var_fd_redirect(&self) -> Option<(String, VarFdKind, usize)> {
        let mut it = self.chars.clone();
        if it.next() != Some('{') {
            return None;
        }
        let mut name = String::new();
        loop {
            match it.next() {
                Some('}') => break,
                Some(c) if c.is_ascii_alphanumeric() || c == '_' => name.push(c),
                _ => return None,
            }
        }
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        // `{` + name + `}` so far.
        let base = name.len() + 2;
        match it.next() {
            Some('<') => match it.next() {
                Some('&') => Some((name, VarFdKind::Dup, base + 2)),
                Some('>') => Some((name, VarFdKind::InOut, base + 2)),
                _ => Some((name, VarFdKind::In, base + 1)),
            },
            Some('>') => match it.next() {
                Some('&') => Some((name, VarFdKind::Dup, base + 2)),
                Some('>') => Some((name, VarFdKind::Out { append: true, clobber: false }, base + 2)),
                Some('|') => Some((name, VarFdKind::Out { append: false, clobber: true }, base + 2)),
                _ => Some((name, VarFdKind::Out { append: false, clobber: false }, base + 1)),
            },
            _ => None,
        }
    }

    fn next_char_is_word_boundary(&self) -> bool {
        let mut it = self.chars.clone();
        it.next(); // the brace itself
        match it.next() {
            None => true,
            Some(c) => c == ' ' || c == '\t' || c == '\n' || c == ';' || c == '|' || c == '&' || c == ')',
        }
    }

    // With one `(` consumed, scans past nested parens and quotes to the
    // matching `)`, returning the raw text before it. For `$( )` and `( )`,
    // both parsed only when run.
    fn capture_balanced_parens(&mut self) -> Result<String, String> {
        // The opening `(` is consumed, so `pos` is the content's first char
        // (see `raw_capture_spans`).
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
                        // `pos` is past the closing `)`, so `pos - 1` is
                        // its index, the content's end.
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

    // With both `((` consumed: the inner text, then the final `)`, since
    // `capture_balanced_parens` stops after the first of the two.
    fn capture_double_paren(&mut self) -> Result<String, String> {
        // No span push: the inner `capture_balanced_parens` recorded it.
        let raw = self.capture_balanced_parens()?;
        // The second `)` is required: `for ((i=0; i<2; i++) do` has one.
        if self.chars.peek().copied() != Some(')') {
            return Err(format!("near `{}'", raw.trim()));
        }
        self.advance();
        Ok(raw)
    }

    fn capture_backtick(&mut self) -> Result<String, String> {
        // As in `capture_balanced_parens`: the opening backtick is consumed.
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

    // `$'...'` (opening consumed): C's escapes rather than the shell's, no
    // `$` expansion inside, as in bash. Built as bytes and decoded at the
    // end, so `\303\244` is the two bytes of `ä`, the form `printf %q`
    // writes.
    fn read_ansi_c_string(&mut self) -> Result<String, String> {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            match self.advance() {
                None => return Err("unterminated $'...'".to_string()),
                Some('\'') => break,
                Some('\\') => match self.advance() {
                    Some('n') => bytes.push(b'\n'),
                    Some('t') => bytes.push(b'\t'),
                    Some('r') => bytes.push(b'\r'),
                    Some('\\') => bytes.push(b'\\'),
                    Some('\'') => bytes.push(b'\''),
                    Some('"') => bytes.push(b'"'),
                    Some('?') => bytes.push(b'?'),
                    Some('a') => bytes.push(0x07),
                    Some('b') => bytes.push(0x08),
                    // `\E` too: `printf %q` writes the capital.
                    Some('e' | 'E') => bytes.push(0x1b),
                    Some('f') => bytes.push(0x0c),
                    Some('v') => bytes.push(0x0b),
                    // Up to three octal digits including this one: `\101`
                    // is `A`, and `\0101` is `\010` then `1`.
                    Some(d @ '0'..='7') => {
                        let mut value = d.to_digit(8).unwrap();
                        for _ in 0..2 {
                            match self.chars.peek().and_then(|c| c.to_digit(8)) {
                                Some(next) => {
                                    value = value * 8 + next;
                                    self.advance();
                                }
                                None => break,
                            }
                        }
                        bytes.push(value as u8);
                    }
                    // `\xHH`, one or two hex digits.
                    Some('x') => bytes.push(read_hex(&mut self.chars, 2) as u8),
                    // `\u`/`\U` are code points, written as UTF-8.
                    Some(u @ ('u' | 'U')) => {
                        let width = if u == 'u' { 4 } else { 8 };
                        let value = read_hex(&mut self.chars, width);
                        match char::from_u32(value) {
                            Some(c) => {
                                let mut buf = [0u8; 4];
                                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            }
                            None => bytes.push(b'?'),
                        }
                    }
                    // `\cX` is the control character for X.
                    Some('c') => match self.advance() {
                        Some(c) => bytes.push((c as u8) & 0x1f),
                        None => return Err("unterminated $'...'".to_string()),
                    },
                    Some(other) => {
                        bytes.push(b'\\');
                        let mut buf = [0u8; 4];
                        bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
                    }
                    None => return Err("unterminated $'...'".to_string()),
                },
                Some(c) => {
                    let mut buf = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    // The delimiter after `<<`/`<<-`: a bare word, or one wrapped in single
    // or double quotes, which disables expansion in the body (as any
    // quoting does in bash).
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

    // Raw lines up to the one equal to the delimiter, bypassing
    // tokenization. `strip_tabs` (`<<-`) strips leading tabs from the lines
    // and the delimiter comparison.
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

    // Whether anything was skipped: `arr=(a b)` is an array assignment, and
    // `arr= (a b)` a syntax error, as in bash.
    fn skip_spaces(&mut self) -> bool {
        let mut skipped = false;
        while let Some(c) = self.chars.peek().copied() {
            if c == ' ' || c == '\t' {
                self.advance();
                skipped = true;
            } else if c == '\\' && self.peek2() == Some('\n') {
                // A line continuation between words is erased, which can
                // leave more spaces, so loop again. `read_word` handles one
                // inside a word (`a\<newline>b`), but reached with no word
                // started it would make an empty word:
                // `cmd a \<newline>    b` would have three arguments.
                self.advance();
                self.advance();
                skipped = true;
            } else {
                break;
            }
        }
        skipped
    }

    /// Takes a tilde prefix here, if there is one, as a `Chunk::Tilde`,
    /// flushing pending text first, since a tilde can begin mid-word
    /// (`PATH=~/bin`, `x=a:~/b`). Returns whether it took one.
    fn take_tilde_prefix(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String) -> bool {
        if self.chars.peek().copied() != Some('~') {
            return false;
        }
        let mut probe = self.chars.clone();
        probe.next();
        let mut name = String::new();
        let mut consumed = 1;
        match probe.peek().copied() {
            Some(c @ ('+' | '-')) => {
                name.push(c);
                consumed += 1;
                probe.next();
            }
            _ => {
                while let Some(c) = probe.peek().copied() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                        name.push(c);
                        consumed += 1;
                        probe.next();
                    } else {
                        break;
                    }
                }
            }
        }
        // A tilde prefix ends at `/`, `:` (`PATH=~/a:~/b` has two), or the
        // end of the word, which a metacharacter ends as well as a space
        // (`cd ~;`).
        let ends_the_prefix = match probe.peek().copied() {
            None | Some('/') | Some(':') => true,
            Some(c) => c.is_whitespace() || is_metacharacter(c),
        };
        if !ends_the_prefix {
            return false;
        }
        for _ in 0..consumed {
            self.advance();
        }
        if !buf.is_empty() {
            chunks.push(Chunk::Str(std::mem::take(buf)));
        }
        chunks.push(Chunk::Tilde { name });
        true
    }

    // Returns the word's chunks and whether it had no quoting or escaping:
    // only a plain word can be a keyword (`"if"` and `\if` never are).
    // `relaxed` is for the operand of `[[ ... =~ pattern ]]`, where bash
    // takes `|`, `(` and `)` as regex text but still rejects `&`, `<` and
    // `>`; whitespace and `]]` still end the word. `literal_ws` is for a
    // `${...}` operator word (`parse_expansion_word`), which is one unit:
    // an unquoted space is content (`${s#hello }`).
    fn read_word(&mut self, relaxed: bool, literal_ws: bool) -> Result<(Vec<Chunk>, bool), String> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut buf = String::new();
        let mut plain = true;

        // Tilde expansion at the start of a word: `~`, `~/...`, `~user`,
        // `~+` and `~-`, resolved at expansion (`Chunk::Tilde`). An
        // assignment's value is the other place one can begin; see the `=`
        // and `:` arms.
        self.take_tilde_prefix(&mut chunks, &mut buf);

        // `m[x y]=1`: an associative key may contain spaces in one word, so
        // whitespace doesn't break inside those brackets. Not any `[`
        // (`echo m[x y]` is two words, `[ x = y ]` the test builtin; see
        // `subscript_is_an_assignment`). A depth, so `a[b[0]]=1` closes
        // correctly.
        let mut in_assign_subscript = 0u32;
        // Set after this word's `=`, after which a `:` may begin a tilde
        // prefix.
        let mut assignment_value = false;

        loop {
            match self.chars.peek().copied() {
                None => break,
                Some('[') if in_assign_subscript == 0 && is_ident(&buf) && self.subscript_is_an_assignment() => {
                    in_assign_subscript = 1;
                    buf.push(self.advance().unwrap());
                }
                Some('[') if in_assign_subscript > 0 => {
                    in_assign_subscript += 1;
                    buf.push(self.advance().unwrap());
                }
                Some(']') if in_assign_subscript > 0 => {
                    in_assign_subscript -= 1;
                    buf.push(self.advance().unwrap());
                }
                // Raw text inside the brackets, expansions included:
                // `m[$k]=1` keeps `$k` for `array_set_index` to resolve, as
                // `${m[$k]}` does; expanding here would field-split the
                // key.
                Some(_) if in_assign_subscript > 0 => {
                    buf.push(self.advance().unwrap());
                }
                Some(c) if !literal_ws && (c == ' ' || c == '\t' || c == '\n') => break,
                // extglob: `@(`, `!(`, `+(`, `*(`, `?(`, the prefix already
                // in `buf`, is a pattern group, not a subshell. bash needs
                // `shopt -s extglob`; bish always recognizes them (see
                // glob.rs).
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
                // `#` opens a comment only where a word could begin: bash
                // reads `echo a#b` as `a#b`, and `printf %q` leaves a
                // mid-word `#` unescaped. With `literal_ws` the input is
                // already one word, so there's no comment to open
                // (`${x:-# hi}`, `PS4='# '`).
                Some('#') if literal_ws || !buf.is_empty() || !chunks.is_empty() => {
                    buf.push(self.advance().unwrap());
                }
                // `x=~/y`, `m[k]=~`, `x+=~`: an assignment's value is the
                // second place a tilde prefix can begin. The name must be
                // real and unquoted: bash leaves `--opt=~/y`, `9x=~/y` and
                // `"a"=~/b` alone. A quoted run would have pushed its own
                // chunk by now, and `is_assignment_target` checks the rest.
                Some('=') if in_assign_subscript == 0 && chunks.is_empty() && is_assignment_target(&buf) => {
                    buf.push(self.advance().expect("peeked"));
                    assignment_value = true;
                    self.take_tilde_prefix(&mut chunks, &mut buf);
                }
                // And after each unquoted `:` in one: `PATH=~/bin:~/sbin`
                // has two. Only in an assignment; `echo a:~/b` is
                // untouched, as in bash.
                Some(':') if assignment_value => {
                    buf.push(self.advance().expect("peeked"));
                    self.take_tilde_prefix(&mut chunks, &mut buf);
                }
                // A metacharacter ends a word, except inside a `${...}`
                // operator, already scanned to its brace
                // (`capture_var_expansion_body`): the `|` in `${s//:/|}` is
                // text.
                Some(c) if !literal_ws && (is_metacharacter(c) || c == '#') => break,
                Some(c) if is_metacharacter(c) => {
                    self.advance();
                    buf.push(c);
                }
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
                    // The quoted run goes in as its own `LiteralStr`, so a
                    // quoted metacharacter stays distinct from real glob or
                    // regex syntax.
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    {
                        // Pushed even when empty: `''`, `""` and `a""b` are
                        // written back as they were. `pos - 1` is the
                        // closing quote, so the span excludes the
                        // delimiters.
                        self.raw_capture_spans.push(lit_start..self.pos - 1);
                        chunks.push(Chunk::LiteralStr(lit, Quoting::Single));
                    }
                }
                Some('"') => {
                    plain = false;
                    self.advance();
                    // What this quoted region has produced, so the empty
                    // case can tell "held nothing" from "held only an
                    // expansion" (`"${a[@]:9}"`, which may vanish). Text
                    // pending in `buf` counts: `a""b` has an `a`.
                    let region_start = chunks.len() + usize::from(!buf.is_empty());
                    let mut lit = String::new();
                    // Where the current literal run started. Double-quote
                    // escapes take two source chars for one decoded char,
                    // so the span is tracked by position, not `lit`'s
                    // length.
                    let mut lit_start = self.pos;
                    loop {
                        match self.advance() {
                            None => return Err("unterminated double quote".to_string()),
                            Some('"') => break,
                            Some('\\') => match self.chars.peek().copied() {
                                Some(n) if n == '"' || n == '\\' || n == '$' || n == '`' => {
                                    self.advance();
                                    lit.push(n);
                                }
                                // A continuation inside quotes too:
                                // `"a\<newline>b"` is `"ab"`.
                                Some('\n') => {
                                    self.advance();
                                }
                                _ => lit.push('\\'),
                            },
                            Some('$') => {
                                // `pos - 1` is the `$` just consumed: the
                                // literal run's end.
                                let dollar_pos = self.pos - 1;
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    self.raw_capture_spans.push(lit_start..dollar_pos);
                                    chunks.push(Chunk::LiteralStr(std::mem::take(&mut lit), Quoting::Double));
                                }
                                // Pass `lit`, not `buf`: if nothing valid
                                // follows the `$`, `push_var` writes a
                                // literal `$` into the buffer it's given,
                                // which must be the quoted one (a `$`
                                // end-anchor inside quotes stays quoted).
                                let produced = self.push_var(&mut chunks, &mut lit, true)?;
                                // If `push_var` wrote a literal `$` back,
                                // the next run starts at the `$`, which is
                                // now in `lit`.
                                lit_start = if produced { self.pos } else { dollar_pos };
                            }
                            Some('`') => {
                                let backtick_pos = self.pos - 1;
                                if !buf.is_empty() {
                                    chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                                }
                                if !lit.is_empty() {
                                    self.raw_capture_spans.push(lit_start..backtick_pos);
                                    chunks.push(Chunk::LiteralStr(std::mem::take(&mut lit), Quoting::Double));
                                }
                                chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: true, backticks: true });
                                lit_start = self.pos;
                            }
                            Some(c) => lit.push(c),
                        }
                    }
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    if !lit.is_empty() || chunks.len() == region_start {
                        // Empty only when the quotes held nothing; see the
                        // single-quoted case.
                        self.raw_capture_spans.push(lit_start..self.pos - 1);
                        chunks.push(Chunk::LiteralStr(lit, Quoting::Double));
                    }
                }
                Some('\\') => {
                    plain = false;
                    // The escape's two source chars map to one decoded
                    // char; the span covers both.
                    let start = self.pos;
                    self.advance();
                    // A backslash-newline is a continuation: both go and
                    // the word goes on. `echo a\<newline>b` is `echo ab`,
                    // and `then \<newline>echo t` works.
                    if self.chars.peek().copied() == Some('\n') {
                        self.advance();
                    } else if let Some(n) = self.advance() {
                        if !buf.is_empty() {
                            chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                        }
                        self.raw_capture_spans.push(start..self.pos);
                        chunks.push(Chunk::LiteralStr(n.to_string(), Quoting::Escape));
                    }
                }
                // `$"..."` is a translated string. With no message
                // catalogues it's a double-quoted string, as in bash when
                // no translation is found.
                Some('$') if self.peek2() == Some('"') => {
                    self.advance(); // '$'
                }
                Some('$') if self.peek2() == Some('\'') => {
                    plain = false;
                    self.advance(); // '$'
                    self.advance(); // '\''
                    let lit = self.read_ansi_c_string()?;
                    // `LiteralStr`, not `Str`: `$'...'` is quoting, so its
                    // result doesn't glob any more than `'...'`'s
                    // (`echo $'*'` prints an asterisk), and the serializer
                    // writes it back quoted.
                    if !buf.is_empty() {
                        chunks.push(Chunk::Str(std::mem::take(&mut buf)));
                    }
                    chunks.push(Chunk::LiteralStr(lit, Quoting::Dollar));
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
                    chunks.push(Chunk::Sub { raw: self.capture_backtick()?, quoted: false, backticks: true });
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

    // Scans the text between `${` and its `}`, counting plain `{`/`}` so a
    // `}` inside a nested expansion (`${v+"${v}"}`) isn't taken for the end.
    // Quoting follows `capture_balanced_parens`. As in bash: `${x:-{}}` is
    // `{}`, `${x:-\{}` is `{`, `${y:-"a}b"}` is `a}b`. No unterminated-input
    // error.
    fn capture_var_expansion_body(&mut self) -> (String, bool) {
        let mut inner = String::new();
        let mut depth: usize = 0;
        loop {
            match self.advance() {
                None => return (inner, false),
                Some('}') if depth == 0 => return (inner, true),
                Some('}') => {
                    depth -= 1;
                    inner.push('}');
                }
                Some('{') => {
                    depth += 1;
                    inner.push('{');
                }
                Some('\\') => {
                    inner.push('\\');
                    if let Some(n) = self.advance() {
                        inner.push(n);
                    }
                }
                Some('\'') => {
                    inner.push('\'');
                    loop {
                        match self.advance() {
                            None => return (inner, false),
                            Some('\'') => {
                                inner.push('\'');
                                break;
                            }
                            Some(c) => inner.push(c),
                        }
                    }
                }
                Some('"') => {
                    inner.push('"');
                    loop {
                        match self.advance() {
                            None => return (inner, false),
                            Some('"') => {
                                inner.push('"');
                                break;
                            }
                            Some('\\') => {
                                inner.push('\\');
                                if let Some(n) = self.advance() {
                                    inner.push(n);
                                }
                            }
                            Some(c) => inner.push(c),
                        }
                    }
                }
                Some(c) => inner.push(c),
            }
        }
    }

    // Reads an expansion after its `$` (command substitution, arithmetic or
    // `${...}`) and pushes the chunk, or a literal `$` when nothing valid
    // follows. Returns whether a real expansion was pushed, so callers
    // tracking whether a word is plain don't count a literal `$` (a regex
    // end-anchor) as quoting.
    fn push_var(&mut self, chunks: &mut Vec<Chunk>, buf: &mut String, quoted: bool) -> Result<bool, String> {
        if self.chars.peek().copied() == Some('(') {
            self.advance();
            if self.chars.peek().copied() == Some('(') {
                self.advance();
                chunks.push(Chunk::Arith { raw: self.capture_double_paren()?, quoted });
            } else {
                chunks.push(Chunk::Sub { raw: self.capture_balanced_parens()?, quoted, backticks: false });
            }
            return Ok(true);
        }
        if self.chars.peek().copied() == Some('{') {
            self.advance();
            // The span excludes the braces, as elsewhere; recorded once a
            // chunk is certain (an empty name pushes none).
            let start = self.pos;
            // This scan has no unterminated-input error, so `closed` tells
            // a real `}` (span excludes it) from running out of input (span
            // covers what was consumed, as `inner` does).
            let (inner, closed) = self.capture_var_expansion_body();
            if !closed {
                // `echo ${x`: an error, not expanded as if the brace were
                // there.
                return Err("unexpected EOF while looking for matching `}'".to_string());
            }
            let span = start..(self.pos - 1);
            match parse_brace_content(&inner) {
                BraceContent::Plain(name) => {
                    if name.is_empty() {
                        buf.push('$');
                        return Ok(false);
                    } else {
                        self.raw_capture_spans.push(span);
                        chunks.push(Chunk::Var { name, quoted, braced: true });
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
                BraceContent::VarNamesMatchingPrefix(prefix, at) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::VarNamesMatchingPrefix { prefix, at, quoted });
                }
            }
            return Ok(true);
        }
        // Bare `$NAME`: no delimiters, so the span is the name.
        let start = self.pos;
        let name = self.read_var_name();
        if name.is_empty() {
            buf.push('$');
            Ok(false)
        } else {
            self.raw_capture_spans.push(start..self.pos);
            chunks.push(Chunk::Var { name, quoted, braced: false });
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

// One item from `tokenize_spanned` with its source range, for
// highlighting and lint. Comments aren't a `Tok` variant, so the
// parser's matches over `Tok` never see one.
#[derive(Debug, Clone, PartialEq)]
pub enum SpannedItem {
    Tok(Tok, std::ops::Range<usize>),
    Comment(std::ops::Range<usize>),
}

pub struct SpannedResult {
    pub items: Vec<SpannedItem>,
    pub raw_capture_spans: Vec<std::ops::Range<usize>>,
    // On a lex error, `items` keeps what came before it, unlike
    // `tokenize`'s `Result`: a half-typed line is the usual case, and
    // should highlight up to the error rather than not at all.
    pub error: Option<String>,
}

// A copy of `tokenize`'s dispatch loop rather than a change to it, so
// nothing here touches execution. Kept in step with `tokenize` by hand;
// the reasoning for each arm is there.
/// The characters that end a word wherever one is read. Not `#`, which
/// ends a word only where one could begin (a comment); inside a word it's
/// ordinary (`echo a#b`, which `printf %q` relies on). The two places
/// that care add it.
fn is_metacharacter(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

/// Whether `text` is an assignment's name part, as bash decides: an
/// identifier, maybe subscripted, maybe appending (`x`, `x+`, `m[k]`,
/// `m[k]+`), not `--opt`, `9x` or `a-b`, after whose `=` a tilde stays
/// literal.
fn is_assignment_target(text: &str) -> bool {
    let text = text.strip_suffix('+').unwrap_or(text);
    match text.split_once('[') {
        // A subscript already judged an assignment's
        // (`subscript_is_an_assignment`), so only the name is checked.
        Some((name, subscript)) => subscript.ends_with(']') && is_ident(name),
        None => is_ident(text),
    }
}

// A plain shell identifier, which may precede `[subscript]=`. Not
// parser.rs's `is_valid_ident`: the lexer doesn't depend on the parser.
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn tokenize_spanned(src: &str) -> SpannedResult {
    let mut lexer = Lexer::new(src);
    let mut items: Vec<SpannedItem> = Vec::new();
    let mut error: Option<String> = None;
    'outer: loop {
        let attached = !lexer.skip_spaces() && !items.is_empty();
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
                            *t = Tok::HereDoc(chunks, HereDocSpelling { delimiter: delim, quoted: !expand, strip_tabs });
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
                        Ok(raw) => items.push(SpannedItem::Tok(Tok::Subshell { raw, attached }, start..lexer.pos)),
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
                        Ok(raw) => items.push(SpannedItem::Tok(Tok::Word(vec![Chunk::ProcSubOut { raw }], false), start..lexer.pos)),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                    continue;
                }
                if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    let tok = lexer.lex_dup_target(1, false);
                    items.push(SpannedItem::Tok(tok, start..lexer.pos));
                    continue;
                }
                let append = lexer.chars.peek().copied() == Some('>');
                if append {
                    lexer.advance();
                }
                let clobber = !append && lexer.chars.peek().copied() == Some('|');
                if clobber {
                    lexer.advance();
                }
                items.push(SpannedItem::Tok(Tok::RedirOut { append, clobber }, start..lexer.pos));
            }
            Some('<') => {
                lexer.advance();
                if lexer.chars.peek().copied() == Some('(') {
                    lexer.advance();
                    match lexer.capture_balanced_parens() {
                        Ok(raw) => items.push(SpannedItem::Tok(Tok::Word(vec![Chunk::ProcSubIn { raw }], false), start..lexer.pos)),
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                    continue;
                }
                if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance();
                    let tok = lexer.lex_dup_target(0, true);
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
                        items.push(SpannedItem::Tok(Tok::HereDoc(vec![Chunk::Str(String::new())], HereDocSpelling::default()), start..lexer.pos));
                        lexer.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                    }
                } else if lexer.chars.peek().copied() == Some('>') {
                    lexer.advance();
                    items.push(SpannedItem::Tok(Tok::RedirInOut, start..lexer.pos));
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
                } else if lexer.chars.peek().copied() == Some('&') {
                    lexer.advance(); // '&'
                    let tok = lexer.lex_dup_target(2, false);
                    items.push(SpannedItem::Tok(tok, start..lexer.pos));
                } else {
                    let append = lexer.chars.peek().copied() == Some('>');
                    if append {
                        lexer.advance();
                    }
                    let clobber = !append && lexer.chars.peek().copied() == Some('|');
                    if clobber {
                        lexer.advance();
                    }
                    items.push(SpannedItem::Tok(Tok::RedirErr { append, clobber }, start..lexer.pos));
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
                            let tok = lexer.lex_dup_target(fd, false);
                            items.push(SpannedItem::Tok(tok, start..lexer.pos));
                        } else {
                            let append = lexer.chars.peek().copied() == Some('>');
                            if append {
                                lexer.advance();
                            }
                            let clobber = !append && lexer.chars.peek().copied() == Some('|');
                            if clobber {
                                lexer.advance();
                            }
                            items.push(SpannedItem::Tok(Tok::RedirFdOut { fd, append, clobber }, start..lexer.pos));
                        }
                    }
                    Some('<') => {
                        if lexer.chars.peek().copied() == Some('&') {
                            lexer.advance();
                            let tok = lexer.lex_dup_target(fd, true);
                            items.push(SpannedItem::Tok(tok, start..lexer.pos));
                        } else if lexer.chars.peek().copied() == Some('>') {
                            lexer.advance();
                            items.push(SpannedItem::Tok(Tok::RedirFdInOut { fd }, start..lexer.pos));
                        } else if lexer.chars.peek().copied() == Some('<') {
                            // `N<<<` and `N<<DELIM`, the same tokens
                            // `tokenize` makes, so `3<<<x` isn't
                            // highlighted as starting a heredoc.
                            lexer.advance();
                            items.push(SpannedItem::Tok(Tok::RedirFdHere { fd }, start..lexer.pos));
                            let here = lexer.pos;
                            if lexer.chars.peek().copied() == Some('<') {
                                lexer.advance();
                                items.push(SpannedItem::Tok(Tok::HereString, here..lexer.pos));
                            } else {
                                let strip_tabs = lexer.chars.peek().copied() == Some('-');
                                if strip_tabs {
                                    lexer.advance();
                                }
                                lexer.skip_spaces();
                                let (delim, expand) = lexer.read_heredoc_delimiter();
                                let tok_idx = items.len();
                                items.push(SpannedItem::Tok(
                                    Tok::HereDoc(vec![Chunk::Str(String::new())], HereDocSpelling::default()),
                                    here..lexer.pos,
                                ));
                                lexer.pending_heredocs.push((tok_idx, delim, strip_tabs, expand));
                            }
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
                                    // Every token from one `{a,b,c}` shares
                                    // the region's span, so it highlights as
                                    // one unit.
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

// Parses a `${...}` operator word (the pattern in `${v#pat}`, the
// replacement in `${v/pat/rep}`, the word in `${v:-word}`) in
// `read_word`'s `literal_ws` mode: expansions and quoting work, but
// unquoted whitespace is content, so `${s#hello }` strips "hello ".
pub(crate) fn parse_expansion_word(raw: &str) -> Vec<Chunk> {
    Lexer::new(raw).read_word(false, true).map(|(chunks, _)| chunks).unwrap_or_else(|_| vec![Chunk::Str(raw.to_string())])
}

// Expansions in an expanding heredoc body. Not `read_word`: quotes are
// literal in a body and newlines are content. Backslash works as in
// double quotes (only `\$`, `\\` and `` \` ``). Expansions are
// `quoted`, since the body is stdin text, never split.
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
                chunks.push(Chunk::Sub { raw: lexer.capture_backtick()?, quoted: true, backticks: true });
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
    // `${!prefix*}` / `${!prefix@}`: every variable name starting with
    // `prefix`; the bool is the `@` spelling, which splits as `$@` does.
    VarNamesMatchingPrefix(String, bool),
}

// An identifier's first-character rule, but any prefix of a name, for
// `${!prefix*}`.
fn is_name_prefix(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => chars.all(|c| c.is_alphanumeric() || c == '_'),
        _ => false,
    }
}

// `name[index]rest`, name an identifier. Returns what follows `]`, so
// the caller can tell a plain access from one with an operator
// (`${arr[0]:-x}`).
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

// The operator suffix after a name or `name[index]` in `${...}`.
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
    // `${V:offset}` / `${V:offset:length}`, reached only after the four
    // `:x` operators fail, so `${V:-x}` is a default; bash needs a space
    // before a negative offset (`${V: -1}`) for this reason.
    if let Some(spec) = rest.strip_prefix(':') {
        return Some(match spec.find(':') {
            Some(idx) => VarOp::Substring { offset: spec[..idx].to_string(), length: Some(spec[idx + 1..].to_string()) },
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
    if let Some(spec) = rest.strip_prefix('@') {
        return match spec {
            "Q" => Some(VarOp::Transform(TransformKind::Quote)),
            "U" => Some(VarOp::Transform(TransformKind::Upper)),
            "u" => Some(VarOp::Transform(TransformKind::UpperFirst)),
            "L" => Some(VarOp::Transform(TransformKind::Lower)),
            "E" => Some(VarOp::Transform(TransformKind::Escape)),
            "A" => Some(VarOp::Transform(TransformKind::Attributes)),
            "a" => Some(VarOp::Transform(TransformKind::AttributeFlags)),
            "K" => Some(VarOp::Transform(TransformKind::KeyValue)),
            "P" => Some(VarOp::Transform(TransformKind::Prompt)),
            // Anything else after `@` isn't recognized, so the caller
            // treats the whole as a name rather than misparsing it.
            _ => None,
        };
    }
    None
}

// Splits "pattern/repl" at the first unescaped `/` (`\/` matches a
// slash). No `/` (`${V/pat}`) replaces with nothing, as in bash.
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

// Parses the text between `${` and `}`. `${#VAR}` comes first, since `#`
// would otherwise read as RemovePrefix on an empty name.
fn parse_brace_content(inner: &str) -> BraceContent {
    if let Some(rest) = inner.strip_prefix('!') {
        if let Some((name, index, after)) = try_split_array(rest) {
            if after.is_empty() && (index == "@" || index == "*") {
                return BraceContent::ArrayKeys(name);
            }
        } else if !rest.is_empty() {
            if let Some(prefix) = rest.strip_suffix('*') {
                if is_name_prefix(prefix) {
                    return BraceContent::VarNamesMatchingPrefix(prefix.to_string(), false);
                }
            } else if let Some(prefix) = rest.strip_suffix('@') {
                if is_name_prefix(prefix) {
                    return BraceContent::VarNamesMatchingPrefix(prefix.to_string(), true);
                }
            } else {
                let mut chars = rest.chars();
                let first = chars.next().unwrap();
                if (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_') {
                    return BraceContent::Indirect(rest.to_string());
                }
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
        // An unknown operator after an index falls to the scalar path, which
        // treats the whole as a name.
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

    // Unknown operator syntax: the whole is a name, which likely doesn't
    // exist.
    BraceContent::Plain(inner.to_string())
}

// Brace expansion (`{a,b,c}`, `{1..5}`, `{a..e}`, nested), only for
// fully literal unquoted words (see `tokenize`); bash also expands
// braces beside other expansions, which is rare.
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
    // `{start..end}` or `{start..end..step}`: bash always steps from
    // `start` toward `end`, so only the step's magnitude is used.
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
        let items: Vec<i64> = if a <= b { (a..=b).step_by(step).collect() } else { (b..=a).rev().step_by(step).collect() };
        let pad_width = brace_range_zero_pad_width(parts[0], parts[1]);
        return Some(items.into_iter().map(|n| format_brace_range_int(n, pad_width)).collect());
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (parts[0].chars().collect(), parts[1].chars().collect());
    if ca.len() == 1 && cb.len() == 1 {
        let (a, b) = (ca[0] as u32, cb[0] as u32);
        let range: Vec<u32> = if a <= b { (a..=b).step_by(step).collect() } else { (b..=a).rev().step_by(step).collect() };
        return Some(range.into_iter().filter_map(char::from_u32).map(String::from).collect());
    }
    None
}

// bash pads a numeric range to a common width when either endpoint is
// written with a leading zero on more than one digit (`{01..5}`); a lone
// `0` or `-0` doesn't count. The width is the longer endpoint as
// written, sign included, so non-negative members fill the sign's column
// with a zero (`{-01..3}` is `-01 000 001 002 003`).
fn brace_range_zero_pad_width(a: &str, b: &str) -> Option<usize> {
    let has_leading_zero = |s: &str| {
        let digits = s.trim_start_matches(['+', '-']);
        digits.len() >= 2 && digits.starts_with('0')
    };
    if has_leading_zero(a) || has_leading_zero(b) { Some(a.chars().count().max(b.chars().count())) } else { None }
}

// Rust's zero padding puts `-` before the zeros and counts it in the
// width, as `brace_range_zero_pad_width` defines it.
fn format_brace_range_int(n: i64, pad_width: Option<usize>) -> String {
    match pad_width {
        Some(width) => format!("{:0width$}", n, width = width),
        None => n.to_string(),
    }
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

/// The most alias expansions in one pass. bash guards each chain; this
/// adds a hard ceiling, which no real command line nears, against
/// `alias a=b; alias b=a`.
const MAX_ALIAS_EXPANSIONS: usize = 200;

/// Substitutes aliases into a tokenized line. bash expands aliases
/// textually while reading; here a command-position word is replaced by
/// the tokens of its value, operators included, before the parser sees
/// them, so `alias l='ls | less'` is a pipeline.
///
/// bash's rules:
///
///  * Only the first word of a command, where a command can start.
///    Assignments before it don't count, so `FOO=1 ll` expands `ll`.
///  * Only an unquoted, unescaped word: `\ll` and `'ll'` run the real
///    thing (`Tok::Word`'s plain flag).
///  * A value ending in a blank makes the next word eligible
///    (`alias sudo='sudo '`).
///  * A value can't re-trigger its own alias: `alias ls='ls -F'`
///    terminates.
///
/// `lookup` keeps the lexer ignorant of `Shell`.
pub fn expand_aliases(toks: Vec<(Tok, usize)>, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(Tok, usize)> {
    let mut queue: std::collections::VecDeque<(Tok, usize)> = toks.into();
    let mut out: Vec<(Tok, usize)> = Vec::new();
    // For each alias being spliced in: tokens left, and whether its value
    // ended in a blank. Popped as each runs out, when the trailing-blank rule
    // applies.
    let mut frames: Vec<(usize, bool)> = Vec::new();
    let mut command_position = true;
    let mut budget = MAX_ALIAS_EXPANSIONS;

    while let Some((tok, line)) = queue.pop_front() {
        // Count this token against the alias that produced it first.
        let mut trailing_blank = false;
        while let Some(frame) = frames.last_mut() {
            frame.0 -= 1;
            if frame.0 > 0 {
                break;
            }
            trailing_blank |= frame.1;
            frames.pop();
        }

        let eligible = command_position && budget > 0 && matches!(&tok, Tok::Word(_, globbable) if *globbable);
        let name = if eligible { literal_word(&tok) } else { None };
        let value = name.as_deref().and_then(lookup);

        if let (Some(name), Some(value)) = (name, value)
            && let Ok(mut sub) = Lexer::new(&value).tokenize()
        {
            // The lexer ends a line with `Newline`; an alias's value is a
            // fragment.
            while matches!(sub.last(), Some((Tok::Newline, _))) {
                sub.pop();
            }
            if !sub.is_empty() {
                budget -= 1;
                // When the value's first word is the alias itself, it's
                // emitted as is, so `alias ls='ls -F'` terminates.
                let self_referential = literal_word(&sub[0].0).as_deref() == Some(name.as_str());
                let ends_blank = value.ends_with([' ', '\t']);
                if self_referential {
                    let (first, _) = sub.remove(0);
                    out.push((first, line));
                    command_position = false;
                }
                if !sub.is_empty() {
                    frames.push((sub.len(), ends_blank));
                    for (t, _) in sub.into_iter().rev() {
                        queue.push_front((t, line));
                    }
                    if !self_referential {
                        // Still at a command's start: the value's first
                        // token is the command word.
                        command_position = true;
                    }
                } else if ends_blank {
                    command_position = true;
                }
                continue;
            }
        }

        command_position = starts_a_command(&tok) || (command_position && is_assignment_word(&tok)) || trailing_blank;
        out.push((tok, line));
    }
    out
}

/// A word of exactly one literal run, nothing expanded or quoted: the
/// only shape that can name an alias.
fn literal_word(tok: &Tok) -> Option<String> {
    match tok {
        Tok::Word(chunks, true) => match chunks.as_slice() {
            [Chunk::Str(s)] => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `NAME=` before the command word: bash expands an alias after these, so
/// they don't use up command position.
fn is_assignment_word(tok: &Tok) -> bool {
    let Some(word) = literal_word(tok) else { return false };
    let Some(eq) = word.find('=') else { return false };
    let name = &word[..eq];
    !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a command can start right after this token: separators and
/// block openers only. After `for` comes a name and after `in` a list,
/// which mustn't expand.
fn starts_a_command(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Pipe
            | Tok::And
            | Tok::Or
            | Tok::Semi
            | Tok::DSemi
            | Tok::SemiAmp
            | Tok::DSemiAmp
            | Tok::Amp
            | Tok::Newline
            | Tok::LBrace
            | Tok::KwIf
            | Tok::KwThen
            | Tok::KwElif
            | Tok::KwElse
            | Tok::KwWhile
            | Tok::KwUntil
            | Tok::KwDo
    )
}

#[cfg(test)]
mod tests {
    // The literal text of each word a line lexes to.
    fn words_of(src: &str) -> Vec<String> {
        super::Lexer::new(src)
            .tokenize()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(tok, _)| match tok {
                super::Tok::Word(chunks, _) => Some(
                    chunks
                        .iter()
                        .map(|c| match c {
                            super::Chunk::Str(s) | super::Chunk::LiteralStr(s, _) => s.as_str(),
                            _ => "",
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect()
    }

    // An assignment's value carries a `Tilde` chunk per prefix; a word bash
    // wouldn't call an assignment carries none.
    #[test]
    fn an_assignments_value_gets_a_tilde_chunk_and_a_flag_does_not() {
        let tildes = |src: &str| {
            let toks = super::Lexer::new(src).tokenize().expect("lexes");
            let chunks = toks
                .into_iter()
                .find_map(|(tok, _)| match tok {
                    super::Tok::Word(chunks, _) => Some(chunks),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no word in {src:?}"));
            chunks
                .iter()
                .map(|c| match c {
                    super::Chunk::Tilde { name } => format!("~{name}"),
                    super::Chunk::Str(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(tildes("x=~/y"), vec!["x=", "~", "/y"]);
        assert_eq!(tildes("p=~/a:~/b"), vec!["p=", "~", "/a:", "~", "/b"]);
        assert_eq!(tildes("x+=~"), vec!["x+=", "~"]);
        assert_eq!(tildes("m[k]=~"), vec!["m[k]=", "~"]);
        assert_eq!(tildes("~-"), vec!["~-"]);

        // Not assignments, so not expanded, as in bash.
        assert_eq!(tildes("--opt=~/y"), vec!["--opt=~/y"]);
        assert_eq!(tildes("9x=~/y"), vec!["9x=~/y"]);
        assert_eq!(tildes("a:~/b"), vec!["a:~/b"]);
        assert_eq!(tildes("x==~/y"), vec!["x==~/y"]);
    }

    // `$'...'`'s numeric escapes are bytes: `\303\244` is `ä`, as
    // `printf %q` writes it.
    #[test]
    fn ansi_c_quoting_reads_the_escapes_printf_q_writes() {
        let read = |body: &str| words_of(&format!("x $'{body}'")).pop().unwrap_or_default();
        assert_eq!(read(r"a\tb"), "a\tb");
        assert_eq!(read(r"a\Eb"), "a\u{1b}b", "capital E, which is what `printf %q` writes");
        assert_eq!(read(r"a\eb"), "a\u{1b}b");
        assert_eq!(read(r"\101\102"), "AB", "up to three octal digits");
        assert_eq!(read(r"\0101"), "\u{8}1", "three at most, so this is \\010 and then a `1`");
        assert_eq!(read(r"\303\244"), "\u{e4}", "octal bytes, decoded as UTF-8 at the end");
        assert_eq!(read(r"\x41\x7"), "A\u{7}", "one or two hex digits");
        assert_eq!(read(r"\U0001F600"), "\u{1f600}", "a code point rather than a byte");
        assert_eq!(read(r"\cA"), "\u{1}");
        assert_eq!(read(r"\q"), r"\q", "an escape it does not know keeps its backslash");
    }

    // `#` opens a comment only where a word could begin, so a mid-word `#`
    // from `printf %q` reads back.
    #[test]
    fn a_hash_inside_a_word_is_an_ordinary_character() {
        assert_eq!(words_of("echo a#b"), vec!["echo".to_string(), "a#b".to_string()]);
        assert_eq!(words_of("echo ab#"), vec!["echo".to_string(), "ab#".to_string()]);
        assert_eq!(words_of("echo a #b"), vec!["echo".to_string(), "a".to_string()], "one that starts a word still does");
        assert_eq!(words_of("# whole line"), Vec::<String>::new());
    }

    use super::*;

    #[test]
    fn brace_range_with_a_leading_zero_endpoint_pads_every_member() {
        assert_eq!(brace_expand("{01..5}"), vec!["01", "02", "03", "04", "05"]);
        assert_eq!(brace_expand("{1..05}"), vec!["01", "02", "03", "04", "05"]);
        assert_eq!(brace_expand("{001..5}"), vec!["001", "002", "003", "004", "005"]);
    }

    #[test]
    fn brace_range_zero_padding_reserves_a_sign_column_for_negative_endpoints() {
        assert_eq!(brace_expand("{-01..3}"), vec!["-01", "000", "001", "002", "003"]);
        assert_eq!(brace_expand("{-5..-01}"), vec!["-05", "-04", "-03", "-02", "-01"]);
    }

    #[test]
    fn brace_range_a_lone_zero_endpoint_does_not_trigger_padding() {
        assert_eq!(brace_expand("{0..3}"), vec!["0", "1", "2", "3"]);
        assert_eq!(brace_expand("{-0..3}"), vec!["0", "1", "2", "3"]);
    }

    #[test]
    fn brace_range_without_any_leading_zero_is_unpadded() {
        assert_eq!(brace_expand("{1..5}"), vec!["1", "2", "3", "4", "5"]);
        assert_eq!(brace_expand("{5..1}"), vec!["5", "4", "3", "2", "1"]);
    }

    #[test]
    fn brace_range_padding_still_applies_with_a_reversed_or_stepped_range() {
        assert_eq!(brace_expand("{5..01}"), vec!["05", "04", "03", "02", "01"]);
        assert_eq!(brace_expand("{00..10..2}"), vec!["00", "02", "04", "06", "08", "10"]);
    }

    // A `${var#pattern}` pattern is one word: a trailing space is content.
    #[test]
    fn parse_expansion_word_preserves_trailing_space() {
        assert_eq!(parse_expansion_word("hello "), vec![Chunk::Str("hello ".to_string())]);
    }

    #[test]
    fn parse_expansion_word_preserves_internal_whitespace_runs() {
        assert_eq!(parse_expansion_word("a  b"), vec![Chunk::Str("a  b".to_string())]);
    }

    // A `}` inside a nested expansion in a quoted alternate value isn't
    // the outer terminator: `${x:-"${x}"}`, the `${VAR+"${VAR}"}` idiom
    // mise's activation uses.
    #[test]
    fn tokenize_handles_a_nested_quoted_expansion_inside_a_var_op_word() {
        assert!(Lexer::new(r#"echo ${x:-"${x}"}"#).tokenize().is_ok());
    }

    #[test]
    fn parse_expansion_word_still_expands_vars_around_literal_spaces() {
        assert_eq!(
            parse_expansion_word("hello $v"),
            vec![Chunk::Str("hello ".to_string()), Chunk::Var { name: "v".to_string(), quoted: false, braced: false }]
        );
    }

    fn spanned_text<'a>(src: &'a str, r: &std::ops::Range<usize>) -> &'a str {
        let chars: Vec<char> = src.chars().collect();
        &src[chars[..r.start].iter().map(|c| c.len_utf8()).sum()..chars[..r.end].iter().map(|c| c.len_utf8()).sum()]
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
        // `echo` was lexed before the unterminated `$(`.
        assert!(res.items.iter().any(|it| matches!(it, SpannedItem::Tok(Tok::Word(w, true), _) if w == &vec![Chunk::Str("echo".to_string())])));
    }

    #[test]
    fn tokenize_spanned_nested_quote_inside_substitution_recurses_via_raw_capture_spans() {
        // The single-quoted string inside `$(...)` can be re-lexed from its
        // own recorded span.
        let src = "echo \"yooo, $(printf 'hello %' world)\"";
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);
        // Two entries, in order: the `"yooo, "` literal run, then the
        // substitution.
        assert_eq!(res.raw_capture_spans.len(), 2);
        assert_eq!(spanned_text(src, &res.raw_capture_spans[0]), "yooo, ");
        let sub_span = &res.raw_capture_spans[1];
        let inner_src = spanned_text(src, sub_span);
        assert_eq!(inner_src, "printf 'hello %' world");
        // Re-lexing the captured text finds its quoted string: how the
        // highlighter recurses.
        let inner = tokenize_spanned(inner_src);
        assert_eq!(inner.error, None);
        assert!(inner.items.iter().any(|it| matches!(
            it,
            SpannedItem::Tok(Tok::Word(w, false), _)
                if w.iter().any(|c| matches!(c, Chunk::LiteralStr(s, _) if s == "hello %"))
        )));
    }

    #[test]
    fn tokenize_spanned_arithmetic_gets_one_flat_span_no_recursion_artifacts() {
        let src = "echo $((1 + 2))";
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);
        // `$((...))` gives one span: `capture_double_paren` doesn't push its
        // own.
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

    // A word made only of expansions gives one `raw_capture_spans` entry
    // per chunk, in order, each matching its chunk; a missed or extra push
    // would desync them.
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

    // `LiteralStr` spans: single quotes (one to one), double quotes
    // (escapes make decoded text shorter, so position is tracked), and a
    // lone backslash escape.
    #[test]
    fn literal_str_spans_cover_source_text_not_decoded_length() {
        let src = r#"echo 'a b'"c\"d"\x"#;
        let res = tokenize_spanned(src);
        assert_eq!(res.error, None);

        // The `LiteralStr` values: `'a b'` is `a b`; `"c\"d"` decodes to
        // `c"d`; `\x` to `x`.
        let expected = ["a b", "c\"d", "x"];
        assert_eq!(res.raw_capture_spans.len(), expected.len(), "spans: {:?}", res.raw_capture_spans);

        // The spans cover the source, quotes excluded and escaping
        // backslashes included.
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
                Chunk::LiteralStr(s, _) => Some(s),
                _ => None,
            })
            .collect();
        assert_eq!(literal_strs, expected.iter().collect::<Vec<_>>());
    }
}

#[cfg(test)]
mod alias_tests {
    use super::*;

    // Through the lexer, so a test reads as the shell line and asserts the
    // token stream the parser sees.
    fn expand(line: &str, aliases: &[(&str, &str)]) -> String {
        let toks = Lexer::new(line).tokenize().expect("lexes");
        let out = expand_aliases(toks, &|name| aliases.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string()));
        out.iter().map(|(t, _)| describe(t)).collect::<Vec<_>>().join(" ")
    }

    fn describe(t: &Tok) -> String {
        match t {
            // Both literal shapes print as text: the quoting tests check
            // that a word survived unexpanded.
            Tok::Word(chunks, _) => chunks
                .iter()
                .map(|c| match c {
                    Chunk::Str(s) | Chunk::LiteralStr(s, _) => s.clone(),
                    _ => "<expansion>".to_string(),
                })
                .collect(),
            Tok::Pipe => "|".to_string(),
            Tok::Semi => ";".to_string(),
            Tok::And => "&&".to_string(),
            Tok::Newline => "\\n".to_string(),
            Tok::KwIf => "if".to_string(),
            Tok::KwThen => "then".to_string(),
            Tok::KwFi => "fi".to_string(),
            Tok::KwFor => "for".to_string(),
            Tok::KwIn => "in".to_string(),
            Tok::KwDo => "do".to_string(),
            Tok::KwDone => "done".to_string(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn the_first_word_of_a_command_is_replaced() {
        assert_eq!(expand("ll", &[("ll", "ls -la")]), "ls -la");
        assert_eq!(expand("ll /tmp", &[("ll", "ls -la")]), "ls -la /tmp");
        // `ll` as an argument stays.
        assert_eq!(expand("echo ll", &[("ll", "ls -la")]), "echo ll");
    }

    // On tokens, not words: a value that's a pipeline becomes one.
    #[test]
    fn an_alias_can_expand_to_a_whole_pipeline() {
        assert_eq!(expand("g", &[("g", "echo x | tr a-z A-Z")]), "echo x | tr a-z A-Z");
        assert_eq!(expand("two", &[("two", "echo one; echo two")]), "echo one ; echo two");
    }

    #[test]
    fn a_command_can_start_after_a_separator_or_a_block_opener() {
        let a = &[("ll", "ls -la")][..];
        assert_eq!(expand("echo a | ll", a), "echo a | ls -la");
        assert_eq!(expand("true && ll", a), "true && ls -la");
        assert_eq!(expand("true; ll", a), "true ; ls -la");
        assert_eq!(expand("if true; then ll; fi", a), "if true ; then ls -la ; fi");
        assert_eq!(expand("echo x\nll", a), "echo x \\n ls -la");
    }

    // An assignment before the command word isn't the command word.
    #[test]
    fn an_assignment_does_not_consume_command_position() {
        assert_eq!(expand("FOO=1 ll", &[("ll", "ls")]), "FOO=1 ls");
        assert_eq!(expand("FOO=1 BAR=2 ll", &[("ll", "ls")]), "FOO=1 BAR=2 ls");
        // But something that only looks like one is.
        assert_eq!(expand("1FOO=x ll", &[("ll", "ls")]), "1FOO=x ll");
    }

    #[test]
    fn quoting_or_escaping_the_word_runs_the_real_thing() {
        assert_eq!(expand("\\ll", &[("ll", "ls -la")]), "ll");
        assert_eq!(expand("'ll'", &[("ll", "ls -la")]), "ll");
        assert_eq!(expand("\"ll\"", &[("ll", "ls -la")]), "ll");
    }

    #[test]
    fn an_alias_cannot_re_trigger_itself() {
        assert_eq!(expand("ls", &[("ls", "ls -F")]), "ls -F");
        assert_eq!(expand("ls /tmp", &[("ls", "ls -F")]), "ls -F /tmp");
        // Two aliases naming each other terminate too.
        let mutual = &[("a", "b"), ("b", "a")][..];
        let out = expand("a", mutual);
        assert!(out == "a" || out == "b", "terminated with {out:?}");
    }

    #[test]
    fn a_trailing_blank_makes_the_next_word_eligible() {
        let a = &[("sudo", "sudo "), ("ll", "ls -la")][..];
        assert_eq!(expand("sudo ll", a), "sudo ls -la");
        // Without the blank, the next word is an argument.
        let b = &[("sudo", "sudo"), ("ll", "ls -la")][..];
        assert_eq!(expand("sudo ll", b), "sudo ll");
    }

    // Only separators and block openers return to command position.
    #[test]
    fn a_loop_variable_and_its_list_are_not_commands() {
        let a = &[("i", "echo NO"), ("x", "echo NO")][..];
        assert_eq!(expand("for i in x; do echo hi; done", a), "for i in x ; do echo hi ; done");
    }

    #[test]
    fn nothing_to_expand_leaves_the_stream_alone() {
        assert_eq!(expand("echo hello", &[]), "echo hello");
        assert_eq!(expand("echo hello", &[("ll", "ls")]), "echo hello");
    }
}

// Up to `max` hex digits as one value; fewer is fine (`\x7`).
fn read_hex(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, max: usize) -> u32 {
    let mut value = 0u32;
    for _ in 0..max {
        match chars.peek().and_then(|c| c.to_digit(16)) {
            Some(digit) => {
                value = value * 16 + digit;
                chars.next();
            }
            None => break,
        }
    }
    value
}
