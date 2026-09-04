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
    // A `~` prefix at the start of a word. `name` is what followed it:
    // empty for `~` and `~/...`, `+`/`-` for `$PWD`/`$OLDPWD`, or a
    // user name to look up. Resolved at expansion time, since it needs
    // the shell (and /etc/passwd).
    Tilde { name: String },
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
    // ${!prefix*} / ${!prefix@} -- the names of every variable (scalar or
    // array) whose name starts with `prefix`, sorted and deduped; `at`
    // distinguishes the two spellings the same way `quoted`+"@" already
    // does for ArrayKeys/ArrayVar (one field per name when quoted, joined
    // otherwise).
    VarNamesMatchingPrefix { prefix: String, at: bool, quoted: bool },
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
// Which redirect a `{name}...` form is. Kept apart from the numbered
// tokens because the number does not exist until the shell allocates
// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarFdKind {
    In,
    Out { append: bool, clobber: bool },
    InOut,
    // `{v}<&N` / `{v}>&N` duplicates N onto a fresh descriptor and
    // names it `v`; `{v}<&-` / `{v}>&-` closes the one `v` already
    // names. The word after the operator says which.
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
    // ${V@Q}/@U/@L/@E -- bash's "parameter transformation" operators,
    // scoped to bash's own set minus `@P` (see TransformKind's own doc
    // comment for why that one's still missing).
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
    // Shell-quoted so the result can be reused as input (`${v@Q}`).
    Quote,
    // Upper/lowercase the entire value (`${v@U}`/`${v@L}`), and
    // uppercase just the first character (`${v@u}`).
    Upper,
    UpperFirst,
    Lower,
    // Expands backslash escape sequences in the value the same way
    // `$'...'` does (`${v@E}`).
    Escape,
    // An assignment/`declare` statement that would recreate the
    // parameter with its attributes and value (`${v@A}`) -- see
    // exec.rs's transform_attributes for the exact format (matches
    // real bash's own quote-style/attribute-prefix rules, which
    // genuinely differ between a plain scalar and a full array).
    Attributes,
    // Just the attribute-flag letters (`${v@a}`), e.g. "rx" for a
    // readonly+exported scalar, "" for a plain one.
    AttributeFlags,
    // `${arr[@]@K}`/`${assoc[@]@K}`: "key value key value ..." pairs
    // (double-quoted values, matching declare -p's own array-element
    // quoting) -- a bare name or a specific single index instead just
    // behaves like `@Q` on that one value, matching real bash exactly
    // (confirmed: `${arr[0]@K}` and a plain scalar `${x@K}` both give
    // the same single-quoted result `@Q` would).
    KeyValue,
    // Expands the value as if it were a PS1-style prompt string (`${v@P}`)
    // -- bash's own backslash escapes (`\u`, `\h`, `\w`, `\$`, `\t`, ...),
    // computed fresh from the shell's live state. Deliberately not wired
    // into bish's own actual prompt (prompt.rs stays exactly as
    // hardcoded as it already was) -- this only ever expands a value on
    // request, standalone, the same as every other transform operator.
    Prompt,
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
    // `clobber` is `>|`: overwrite even under `set -C`. Only ever
    // true for the plain form -- `>>|` is not a thing.
    RedirOut { append: bool, clobber: bool },
    RedirIn,
    // `<>`: opens the target for *both* reading and writing, on fd 0 (or
    // the explicit leading digit, RedirFdInOut below). Real bash's own
    // use case this exists for is almost entirely `/dev/tcp/HOST/PORT`
    // (see exec.rs's dev_socket_file) -- a plain `<`/`>` against that
    // same path would each open an independent, unrelated connection,
    // useless for a request/response protocol that needs one connection
    // used both ways.
    RedirInOut,
    RedirErr { append: bool, clobber: bool },
    RedirBoth { append: bool },
    DupErrToOut,
    // Arbitrary-fd redirects: `N>`/`N>>`/`N<`/`N<>`/`N>&M`/`N<&M`. The fd=2
    // forms (`2>`, `2>>`, `2>&1`) stay on the dedicated tokens above -- this
    // is for every other explicit fd number, plus `2<`/`2<>`/`2>&M` (M !=
    // 1), which the fd=2 fast path above doesn't cover.
    RedirFdOut { fd: u32, append: bool, clobber: bool },
    // `{name}<`, `{name}>`, `{name}>>`, `{name}<>`: bash's own way of
    // not colliding with a hardcoded descriptor number. The shell picks
    // a free fd and assigns it to `name`; see Redirect::VarFd.
    RedirVarFd { var: String, kind: VarFdKind },
    RedirFdIn { fd: u32 },
    RedirFdInOut { fd: u32 },
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
    // `attached` means the `(` sat directly against the previous
    // token, with no space -- the difference between `arr=(a b)` (an
    // array assignment) and `arr= (a b)` (a syntax error in bash).
    Subshell { raw: String, attached: bool },
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
    // 1-based source line the char about to be read by advance() sits on
    // -- bumped there whenever the consumed char is '\n', same "always
    // tracked, one extra increment per char" treatment as `pos`. Used by
    // tokenize() to tag every emitted token with the line it started on
    // (see push_tok!'s own doc comment), for the debugger's breakpoint/
    // current-line machinery -- the real executable AST otherwise has no
    // position information at all.
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
        if c == Some('\n') {
            self.line += 1;
        }
        c
    }

    pub fn tokenize(mut self) -> Result<Vec<(Tok, usize)>, String> {
        let mut toks = Vec::new();
        // Tags every token pushed in this function with the source line
        // it started on -- a thin wrapper (not touching argument
        // parenthesization at all) around every existing `toks.push(...)`
        // call site, so the real Parser (unlike the separate
        // tokenize_spanned copy lint/format/highlight already use) gets
        // line numbers for the actual executed AST.
        macro_rules! push_tok {
            ($tok:expr) => {
                toks.push(($tok, self.line))
            };
        }
        loop {
            // Whether this token touches the one before it, with no
            // space between them. Only `(` after an `name=` word cares
            // -- see Tok::Subshell.
            let attached = !self.skip_spaces() && !toks.is_empty();
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
                            let chunks = if expand { expand_heredoc_chunks(&body)? } else { vec![Chunk::Str(body)] };
                            toks[tok_idx].0 = Tok::HereDoc(chunks);
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
                // `{name}>file` and friends, before the brace-group
                // arm below: a `{` that opens a group is followed by a
                // word boundary, and this one is followed by a name and
                // a redirect operator, so the two never overlap.
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
                        push_tok!(self.lex_dup_target(1));
                        continue;
                    }
                    let append = self.chars.peek().copied() == Some('>');
                    if append {
                        self.advance();
                    }
                    // `>|` -- the escape hatch from `set -C`, and the
                    // only reason a script writes it.
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
                        push_tok!(self.lex_dup_target(0));
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
                            push_tok!(Tok::HereDoc(vec![Chunk::Str(String::new())]));
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
                        // `2>&3`, `2>&$fd`, `2>&-`. This arm matches
                        // before the general numbered-fd path below, so
                        // anything it does not handle itself never
                        // reaches that path -- and everything but `&1`
                        // used to fall through to `RedirErr`, leaving
                        // the `&` for the parser to trip over.
                        self.advance(); // '&'
                        push_tok!(self.lex_dup_target(2));
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
                // Any other explicit fd number (0,1,3-9,...; also covers
                // `2<`, which the fd=2 fast path above doesn't handle)
                // immediately followed by `>`/`<` with no
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
                                push_tok!(self.lex_dup_target(fd));
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
                                push_tok!(self.lex_dup_target(fd));
                            } else if self.chars.peek().copied() == Some('>') {
                                self.advance();
                                push_tok!(Tok::RedirFdInOut { fd });
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

    // Called with the next char being the `[` that follows an
    // identifier: is this `NAME[...]=` (or `+=`), an assignment whose
    // subscript may hold spaces -- rather than a glob character class or
    // the `[` builtin? Looks ahead over a clone, so nothing is consumed
    // when the answer is no.
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
                // A newline or a metacharacter means this was never a
                // subscript -- stop rather than scanning the rest of
                // the script for a `]` that closes something else.
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

    // `{name}<`, `{name}>`, `{name}>>`, `{name}<>` -- the variable-fd
    // redirect. Returns the name, which form it is, and how many
    // characters to consume; `None` when this `{` is something else
    // entirely, which is every other `{` in the language.
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
        // The *second* `)` is not optional. `for ((i=0; i<2; i++) do`
        // has only one, and swallowing that silently turned an
        // unbalanced loop header into a working loop over nonsense.
        if self.chars.peek().copied() != Some(')') {
            return Err(format!("near `{}'", raw.trim()));
        }
        self.advance();
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
    // `$'...'`, whose escapes are C's rather than the shell's.
    //
    // Assembled as *bytes* and decoded at the end, because `\303\244`
    // has to mean the two bytes of `ä` and not the two characters
    // U+00C3 U+00A4. That is the form `printf %q` writes a non-ASCII
    // string in, so this is the reading end of that round trip.
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
                    // `\E` as well as `\e`: `printf %q` writes the
                    // capital one.
                    Some('e' | 'E') => bytes.push(0x1b),
                    Some('f') => bytes.push(0x0c),
                    Some('v') => bytes.push(0x0b),
                    // Up to three octal digits, counting the one just
                    // read -- so `\0` is NUL, `\101` is `A`, and
                    // `\0101` is NUL-then-`101`... no: it is `\010`
                    // then `1`, which is what the three-digit limit
                    // gives on its own.
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
                    // `\uHHHH` / `\UHHHHHHHH` name a code point rather
                    // than a byte, so they go in as UTF-8.
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
                    // `\cX` is the control character X stands for.
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

    // Returns whether anything was actually skipped. `arr=(a b)` and
    // `arr= (a b)` produce the same two tokens otherwise, and only the
    // first is an array assignment -- the second is bash's own syntax
    // error, and was bish quietly doing the assignment anyway.
    fn skip_spaces(&mut self) -> bool {
        let mut skipped = false;
        while let Some(c) = self.chars.peek().copied() {
            if c == ' ' || c == '\t' {
                self.advance();
                skipped = true;
            } else {
                break;
            }
        }
        skipped
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

        // Tilde expansion at the very start of a word: `~`, `~/...`,
        // `~user`, and bash's `~+`/`~-` for `$PWD`/`$OLDPWD`. The name
        // is resolved at expansion time, not here -- see Chunk::Tilde.
        if self.chars.peek().copied() == Some('~') {
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
            // A tilde prefix ends at `/` or at the end of the word.
            if matches!(probe.peek().copied(), None | Some('/') | Some(' ') | Some('\t') | Some('\n') | Some(':')) {
                for _ in 0..consumed {
                    self.advance();
                }
                chunks.push(Chunk::Tilde { name });
            }
        }

        // `m[x y]=1`: an associative array's key may contain spaces, and
        // the assignment is still one word. Set while inside those
        // brackets so the whitespace break below does not split it --
        // see subscript_is_an_assignment for why this cannot simply be
        // "any `[`" (`echo m[x y]` is two words, and `[ x = y ]` is the
        // test builtin).
        // Depth rather than a flag, so `a[b[0]]=1` closes correctly.
        let mut in_assign_subscript = 0u32;

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
                // Everything between the brackets goes in as raw text,
                // expansions included: `m[$k]=1` keeps `$k` unexpanded
                // here and array_set_index resolves it later, exactly
                // as `${m[$k]}` already does. Expanding it here instead
                // would field-split the key.
                Some(_) if in_assign_subscript > 0 => {
                    buf.push(self.advance().unwrap());
                }
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
                // `#` only opens a comment where a word could begin.
                // Inside one it is an ordinary character: bash reads
                // `echo a#b` as `a#b`, and `printf %q` relies on that
                // -- it leaves a mid-word `#` unescaped, so a shell
                // that broke the word there could not read back what it
                // had just written.
                Some('#') if !buf.is_empty() || !chunks.is_empty() => {
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
                // `$"..."` is a *translated* string. With no message
                // catalogues to consult there is nothing to translate,
                // so it is exactly a double-quoted string -- which is
                // also what bash does when no translation is found.
                // Reading it as a literal `$` followed by a quote,
                // which is what happened before, printed the dollar.
                Some('$') if self.peek2() == Some('"') => {
                    self.advance(); // '$'
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

    // Scans the raw text between `${` and its matching `}`, tracking brace
    // depth for plain unquoted/unescaped `{`/`}` so a `}` reached only
    // through a nested expansion -- most commonly a double-quoted
    // alternate value that itself contains `${...}`, e.g. `${v+"${v}"}`
    // -- isn't mistaken for the outer terminator. Quoting/escaping here
    // mirrors capture_balanced_parens's own quote loops. Confirmed against
    // real bash's own scanning rules: `${x:-{}}` => "{}" (plain braces
    // still count toward depth), `${x:-\{}` => "{" (an escaped brace
    // doesn't), `${y:-"a}b"}` => "a}b" (a quoted brace never does). No
    // unterminated-input error path, same best-effort convention as the
    // loop this replaces.
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
            // Unlike capture_balanced_parens/capture_backtick, this scan
            // has no unterminated-input error path (matches this
            // function's pre-existing best-effort behavior) -- so `closed`
            // distinguishes "stopped at a real '}'" (span excludes it,
            // same delimiter-excluded convention as elsewhere) from "ran
            // out of input first" (span includes every char actually
            // consumed, matching what `inner` itself already contains).
            let (inner, closed) = self.capture_var_expansion_body();
            if !closed {
                // `echo ${x` -- expanding it as though the brace were
                // there is how a truncated line went on to run.
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
                BraceContent::VarNamesMatchingPrefix(prefix, at) => {
                    self.raw_capture_spans.push(span);
                    chunks.push(Chunk::VarNamesMatchingPrefix { prefix, at, quoted });
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
// A plain shell identifier -- what may sit in front of a `[subscript]=`
// in an assignment. Deliberately not `is_valid_ident` from parser.rs:
// the lexer does not depend on the parser.
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
                    let tok = lexer.lex_dup_target(1);
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
                    let tok = lexer.lex_dup_target(2);
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
                            let tok = lexer.lex_dup_target(fd);
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
                            let tok = lexer.lex_dup_target(fd);
                            items.push(SpannedItem::Tok(tok, start..lexer.pos));
                        } else if lexer.chars.peek().copied() == Some('>') {
                            lexer.advance();
                            items.push(SpannedItem::Tok(Tok::RedirFdInOut { fd }, start..lexer.pos));
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
    // ${!prefix*}/${!prefix@} -- names of every variable whose name
    // starts with `prefix`, one field per name (bool is true for the
    // '@' spelling, matching the $@-vs-$*-style splitting distinction
    // every other "@"/"*" index already carries elsewhere in this file).
    VarNamesMatchingPrefix(String, bool),
}

// Same identifier-first-character rule as an ordinary variable name,
// but allowed to be any non-empty prefix of one (used by ${!prefix*}/
// ${!prefix@} below, where `prefix` need not itself be a complete name).
fn is_name_prefix(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => chars.all(|c| c.is_alphanumeric() || c == '_'),
        _ => false,
    }
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
            // Anything else after '@': not recognized -- falling back to
            // None here, same as any other unrecognized operator syntax,
            // lets the caller treat the whole thing as a literal name
            // instead of silently misparsing it.
            _ => None,
        };
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

// Zero-padding trigger: bash pads a numeric brace range's output to a
// common field width whenever either endpoint was written with a
// literal leading zero on a multi-digit number (`{01..5}`, `{1..05}`)
// -- a lone "0"/"-0" doesn't count (`{-0..3}` stays unpadded). The
// field width is just the longer endpoint's own written length, sign
// included, so a negative endpoint reserves a column for '-' that
// non-negative members of the same range fill with an extra zero
// instead (`{-01..3}` -> "-01 000 001 002 003").
fn brace_range_zero_pad_width(a: &str, b: &str) -> Option<usize> {
    let has_leading_zero = |s: &str| {
        let digits = s.trim_start_matches(['+', '-']);
        digits.len() >= 2 && digits.starts_with('0')
    };
    if has_leading_zero(a) || has_leading_zero(b) { Some(a.chars().count().max(b.chars().count())) } else { None }
}

// Rust's own zero-padded integer formatting already places the '-'
// before the zero-fill and counts it toward the requested width, which
// is exactly brace_range_zero_pad_width's own field-width definition
// above -- no separate sign handling needed here.
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

// ---------------------------------------------------------------------
// Alias expansion
// ---------------------------------------------------------------------

/// The most alias expansions one pass will perform.
///
/// Two aliases that name each other (`alias a=b; alias b=a`) would
/// otherwise expand forever. bash's own answer is a per-chain guard;
/// this is that plus a hard ceiling, because a ceiling cannot be
/// reasoned around and a real command line never comes near it.
const MAX_ALIAS_EXPANSIONS: usize = 200;

/// Substitutes aliases into an already-tokenized line.
///
/// bash expands aliases while *reading*, textually, before the line is
/// parsed -- which is what let `alias l='ls | less'` work, and what made
/// this look impossible in a shell that tokenizes and parses a whole
/// script up front. The token stream is the seam that resolves it: a
/// word in command position is replaced by the tokens of its alias's
/// value, operators and all, before the parser ever sees either. So an
/// alias whose value is a whole pipeline is a pipeline, not a command
/// with `|` as an argument -- which is exactly the "half-correct
/// expansion" this was held back to avoid.
///
/// The rules are bash's:
///
///  * Only the **first word** of a command, and only where a command can
///    start. A variable assignment before it does not count as the
///    command word, so `FOO=1 ll` still expands `ll`.
///  * Only an **unquoted, unescaped** word. `\ll` and `'ll'` run the real
///    thing -- `Tok::Word`'s own "no quoting or expansion at all" flag is
///    already exactly this question.
///  * An alias whose value **ends in a blank** makes the next word
///    eligible too, which is the whole point of `alias sudo='sudo '`.
///  * An alias's own value cannot re-trigger it, so `alias ls='ls -F'`
///    terminates.
///
/// `lookup` rather than a table so the lexer stays ignorant of `Shell`.
pub fn expand_aliases(toks: Vec<(Tok, usize)>, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(Tok, usize)> {
    let mut queue: std::collections::VecDeque<(Tok, usize)> = toks.into();
    let mut out: Vec<(Tok, usize)> = Vec::new();
    // How many tokens are left from each alias currently being spliced
    // in, and whether that alias's value ended in a blank. Popped as
    // each runs out, which is when the trailing-blank rule fires.
    let mut frames: Vec<(usize, bool)> = Vec::new();
    let mut command_position = true;
    let mut budget = MAX_ALIAS_EXPANSIONS;

    while let Some((tok, line)) = queue.pop_front() {
        // Account for this token against whichever alias produced it,
        // before anything else can change what "next" means.
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
            // The lexer ends a line with a `Newline`; an alias's value is
            // a fragment of one, not a line of its own.
            while matches!(sub.last(), Some((Tok::Newline, _))) {
                sub.pop();
            }
            if !sub.is_empty() {
                budget -= 1;
                // An alias's own value cannot re-trigger it: emit its
                // first word directly when that word *is* the alias, so
                // `alias ls='ls -F'` terminates instead of recursing.
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
                        // Still at the start of a command: the alias's
                        // own first token is the command word now.
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

/// A word made of exactly one literal run, with nothing expanded or
/// quoted in it -- the only shape that can name an alias.
fn literal_word(tok: &Tok) -> Option<String> {
    match tok {
        Tok::Word(chunks, true) => match chunks.as_slice() {
            [Chunk::Str(s)] => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// `NAME=` -- a variable assignment sitting in front of the command
/// word. bash expands an alias after these, so they must not consume
/// command position.
fn is_assignment_word(tok: &Tok) -> bool {
    let Some(word) = literal_word(tok) else { return false };
    let Some(eq) = word.find('=') else { return false };
    let name = &word[..eq];
    !name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a command can start immediately after this token.
///
/// Deliberately only the separators and block openers, not every
/// keyword: the word after `for` is a variable name and the words after
/// `in` are a list, and expanding an alias into either would be wrong.
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
    // The text of every word a line lexes to, for the tests just below.
    // Only the plain-literal chunks matter to them.
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
                            super::Chunk::Str(s) | super::Chunk::LiteralStr(s) => s.as_str(),
                            _ => "",
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect()
    }

    // `$'...'` carries C's escapes, and the numeric ones name *bytes*:
    // `\303\244` is the two bytes of `ä`, not the two characters
    // U+00C3 U+00A4. That is the form `printf %q` writes a non-ASCII
    // string in, and this is the reading end of that round trip.
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

    // `#` opens a comment only where a word could begin. `printf %q`
    // leaves a mid-word `#` unescaped, so a shell that ended the word
    // there could not read back what it had just written -- which is
    // exactly what this one used to do.
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

    // Regression: the raw ${...} scan used to stop at the first literal
    // '}', even one reached only through a nested expansion inside a
    // double-quoted alternate value -- `${x:-"${x}"}` (the
    // `${VAR+"${VAR}"}` idiom mise's own activation script uses) would
    // mistake the inner ${x}'s own '}' for the outer terminator, leaving a
    // stray '"' and a real closing '}' for the *next* token to choke on.
    #[test]
    fn tokenize_handles_a_nested_quoted_expansion_inside_a_var_op_word() {
        assert!(Lexer::new(r#"echo ${x:-"${x}"}"#).tokenize().is_ok());
    }

    #[test]
    fn parse_expansion_word_still_expands_vars_around_literal_spaces() {
        assert_eq!(parse_expansion_word("hello $v"), vec![Chunk::Str("hello ".to_string()), Chunk::Var { name: "v".to_string(), quoted: false }]);
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

#[cfg(test)]
mod alias_tests {
    use super::*;

    // Round-trips through the lexer so a test reads as the shell line it
    // is, and so what is asserted is the *token stream* the parser will
    // actually see.
    fn expand(line: &str, aliases: &[(&str, &str)]) -> String {
        let toks = Lexer::new(line).tokenize().expect("lexes");
        let out = expand_aliases(toks, &|name| aliases.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string()));
        out.iter().map(|(t, _)| describe(t)).collect::<Vec<_>>().join(" ")
    }

    fn describe(t: &Tok) -> String {
        match t {
            // Both literal shapes, so a quoted word prints as its text
            // -- the point of the quoting tests is that it survived
            // unexpanded, not that it is unprintable.
            Tok::Word(chunks, _) => chunks
                .iter()
                .map(|c| match c {
                    Chunk::Str(s) | Chunk::LiteralStr(s) => s.clone(),
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
        // Not a *later* word: `ll` as an argument is an argument.
        assert_eq!(expand("echo ll", &[("ll", "ls -la")]), "echo ll");
    }

    // The reason this is done on tokens rather than on words: an alias
    // whose value is a pipeline has to *become* a pipeline.
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

    // A variable assignment sits in front of the command word without
    // being it.
    #[test]
    fn an_assignment_does_not_consume_command_position() {
        assert_eq!(expand("FOO=1 ll", &[("ll", "ls")]), "FOO=1 ls");
        assert_eq!(expand("FOO=1 BAR=2 ll", &[("ll", "ls")]), "FOO=1 BAR=2 ls");
        // ...but something that only looks like one does.
        assert_eq!(expand("1FOO=x ll", &[("ll", "ls")]), "1FOO=x ll");
    }

    #[test]
    fn quoting_or_escaping_the_word_runs_the_real_thing() {
        assert_eq!(expand("\\ll", &[("ll", "ls -la")]), "ll");
        assert_eq!(expand("'ll'", &[("ll", "ls -la")]), "ll");
        assert_eq!(expand("\"ll\"", &[("ll", "ls -la")]), "ll");
    }

    // `alias ls='ls -F'` is the single most common alias there is, and it
    // has to terminate.
    #[test]
    fn an_alias_cannot_re_trigger_itself() {
        assert_eq!(expand("ls", &[("ls", "ls -F")]), "ls -F");
        assert_eq!(expand("ls /tmp", &[("ls", "ls -F")]), "ls -F /tmp");
        // Two aliases naming each other cannot spin forever either.
        let mutual = &[("a", "b"), ("b", "a")][..];
        let out = expand("a", mutual);
        assert!(out == "a" || out == "b", "terminated with {out:?}");
    }

    // `alias sudo='sudo '` -- the trailing blank is what makes the *next*
    // word eligible, and the only reason anyone writes one.
    #[test]
    fn a_trailing_blank_makes_the_next_word_eligible() {
        let a = &[("sudo", "sudo "), ("ll", "ls -la")][..];
        assert_eq!(expand("sudo ll", a), "sudo ls -la");
        // Without the blank, the next word is just an argument.
        let b = &[("sudo", "sudo"), ("ll", "ls -la")][..];
        assert_eq!(expand("sudo ll", b), "sudo ll");
    }

    // Only the separators and block openers put us back in command
    // position: the word after `for` is a variable name, and the words
    // after `in` are a list.
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

// Up to `max` hexadecimal digits, as one value. Fewer is fine -- `\x7`
// is a complete escape -- which is why this peeks rather than demands.
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
