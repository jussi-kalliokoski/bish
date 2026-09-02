use crate::lexer::{keyword_text, Chunk, Lexer, Tok};

#[derive(Debug, Clone)]
pub struct Word {
    pub chunks: Vec<Chunk>,
    pub globbable: bool,
}

#[derive(Debug, Clone)]
pub enum Redirect {
    In(Word),
    // `<>`: opens the target for both reading and writing, on fd 0 --
    // see lexer.rs's own Tok::RedirInOut doc comment for what this is
    // actually for (almost entirely /dev/tcp/HOST/PORT).
    InOut(Word),
    Out { word: Word, append: bool, clobber: bool },
    Err { word: Word, append: bool, clobber: bool },
    Both { word: Word, append: bool },
    DupErrToOut,
    HereString(Word),
    HereDoc(Word),
    // Arbitrary-fd forms: `N>file`/`N>>file`/`N<file` and `N>&M`/`N<&M`.
    // Only per-command (not the persistent shell-level `exec N>file` form,
    // which would need fds kept open for the rest of the shell's life --
    // a documented, separate gap).
    FdOut { fd: u32, word: Word, append: bool, clobber: bool },
    FdIn { fd: u32, word: Word },
    // `N<>file`: same as InOut above, just on an explicit fd instead of
    // the implicit 0 -- the far more common real shape in practice
    // (`exec 3<>/dev/tcp/host/80`, then `>&3`/`<&3` to actually use it),
    // since a bare, unnumbered `<>` only ever gives you fd 0 back, which
    // can't be both a request-writer and a response-reader at once in
    // any useful way without a second fd to reach it by.
    FdInOut { fd: u32, word: Word },
    FdDup { fd: u32, target: u32 },
    // `[N]>&WORD` / `[N]<&WORD` with a non-literal target (e.g. a variable
    // holding an fd number, like a coproc's array entries) -- word is
    // expanded and parsed as the target fd at redirect-resolution time.
    FdDupWord { fd: u32, word: Word },
    // `[N]>&-` / `[N]<&-`: closes fd N.
    FdClose { fd: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignMode {
    Set,
    Append,
}

// One element of an array literal's parenthesized body (`arr=(1 [5]=x
// 2)`, `assoc=([k]=v)`). `Keyed`'s index is raw, unevaluated text (an
// arithmetic expression for a plain/indexed array, an arbitrary string
// for an associative one -- exec.rs's apply_array_literal decides which
// the same way array_set_index already does, by whether the name is
// already known-associative). A plain `Positional` element continues
// from a running "next index" counter that a `Keyed` element ahead of
// it in the same literal bumps forward, matching bash (`(1 [5]=x 2)` ->
// indices 0, 5, 6).
#[derive(Debug, Clone)]
pub enum ArrayLiteralItem {
    Positional(Word),
    Keyed(String, Word),
}

#[derive(Debug, Clone)]
pub struct SimpleCommand {
    pub assigns: Vec<(String, AssignMode, Word)>,
    // `name=(word word ...)` / `name+=(word word ...)` array literals --
    // kept separate from `assigns` since array values don't fit the scalar
    // Word model. Only ever populated while still in the simple command's
    // own leading assignment-prefix position (`arr=(1 2 3) cmd` -- though
    // see array_word_assigns below for the other place one of these can
    // appear).
    pub array_assigns: Vec<(String, AssignMode, Vec<ArrayLiteralItem>)>,
    // `name=(...)` array literals appearing as a *later* word of a
    // declare/local/export/readonly/typeset command (`declare -A
    // m=([a]=1 [b]=2)`) -- real bash gives these builtins' own grammar
    // special recognition for this syntax anywhere in their word list,
    // not just in prefix-assignment position. The `usize` is the index
    // into `words` (see below) this literal conceptually sits at -- no
    // placeholder is pushed into `words` itself for it, so exec.rs's
    // run_single has to interleave the two back together in order when
    // building the final argument stream (a plain string arg vs. one of
    // these can flip which flags -- -A vs -a -- are in effect when it's
    // reached, so the original relative order matters).
    pub array_word_assigns: Vec<(usize, String, AssignMode, Vec<ArrayLiteralItem>)>,
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
    Select {
        var: String,
        // Same `None` == "$@" convention as For::words.
        words: Option<Vec<Word>>,
        body: Program,
        redirects: Vec<Redirect>,
    },
    // `coproc [NAME] command`. Scoped grammar (see parse_coproc): the named
    // form is only recognized when written as `coproc NAME { ...; }` --
    // unambiguous since `{` can't otherwise start what NAME would mean.
    // `coproc NAME simple_command` (no braces) isn't supported; write it
    // with braces instead. `name: None` means the unnamed form, which
    // populates the default `COPROC` array/PID vars.
    Coproc { name: Option<String>, body: Box<Command> },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, Program, CaseTerm)>,
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

// What happens after a case arm's body finishes: `;;` stops (the normal
// case), `;&` falls through into the *next* arm's body unconditionally
// (no pattern test), `;;&` keeps testing subsequent patterns from the top
// (doesn't stop, but doesn't skip the pattern match either).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaseTerm {
    Stop,
    FallThrough,
    Continue,
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
    /// `time` in front of the pipeline. A reserved word rather than a
    /// builtin -- which is the whole reason it lives on the pipeline
    /// and not in an argv: it times *all* of it, and a builtin could
    /// only ever see one command's worth.
    pub timed: Option<TimeStyle>,
}

/// Which shape `time` reports in. `-p` is POSIX's, plain seconds on
/// three unadorned lines; the default is bash's own `0m0.000s`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeStyle {
    Shell,
    Posix,
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
    // 1-based source line the statement started on -- captured once,
    // where the *real* Parser first begins parsing this ListItem
    // (parse_list_until), unlike the separate tokenize_spanned/lint/
    // format/highlight span machinery, which never fed into this AST at
    // all. Used by the debugger (exec.rs's run_program) to match against
    // breakpoints and report the currently-executing line. `0` for a
    // synthetically re-serialized ListItem (declare -f's print_functions,
    // functions_preamble) that was never really parsed from source.
    pub line: usize,
}

pub type Program = Vec<ListItem>;

pub struct Parser {
    toks: Vec<Tok>,
    // Parallel to `toks` -- lines[i] is the source line toks[i] started
    // on. See ListItem::line's own doc comment.
    lines: Vec<usize>,
    pos: usize,
}

impl Parser {
    pub fn new(toks: Vec<(Tok, usize)>) -> Self {
        let (toks, lines) = toks.into_iter().unzip();
        Parser { toks, lines, pos: 0 }
    }

    // The source line the token at the parser's current position started
    // on -- `0` (matching ListItem::line's own "not really parsed"
    // convention) once every token has been consumed.
    fn current_line(&self) -> usize {
        self.lines.get(self.pos).copied().unwrap_or(0)
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
    // `parse_list_until` for the places a shell grammar requires at
    // least one command: every condition, and every loop or `if` body.
    // bash rejects `if true; then fi` and `while true; do done`, and
    // `while; do :; done` -- an *empty condition*, which bish read as
    // "true" and then looped on forever.
    fn parse_required_list_until(&mut self, stops: &[Tok]) -> Result<Program, String> {
        let list = self.parse_list_until(stops)?;
        if list.is_empty() {
            return Err(format!("near unexpected token `{}'", self.offending_token_text()));
        }
        Ok(list)
    }

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
            let line = self.current_line();
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
            items.push(ListItem { and_or, sep, line });
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
        // `time [-p]` and `!`, in either order: the grammar puts `time`
        // first but bash takes `! time true` too. Recognized by their
        // text, like `!` always has been here, because both are only
        // keywords where a command could start -- `echo time` is a
        // word.
        let mut timed = None;
        let mut negate = false;
        loop {
            if timed.is_none() && self.word_is("time") {
                self.advance();
                timed = Some(TimeStyle::Shell);
                if self.word_is("-p") {
                    self.advance();
                    timed = Some(TimeStyle::Posix);
                }
                continue;
            }
            if !negate && self.word_is("!") {
                self.advance();
                negate = true;
                continue;
            }
            break;
        }
        let mut commands = vec![self.parse_command()?];
        while matches!(self.peek(), Some(Tok::Pipe)) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_command()?);
        }
        Ok(Pipeline { commands, negate, timed })
    }

    // Whether the next token is exactly this unquoted word.
    fn word_is(&self, text: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(chunks, _)) if matches!(chunks.as_slice(), [Chunk::Str(s)] if s == text))
    }

    // How to name the token a syntax error is about, the way bash does
    // -- the keyword or the word itself, not its Debug spelling.
    fn offending_token_text(&self) -> String {
        match self.peek() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(chunks).unwrap_or_else(|| "word".to_string()),
            Some(tok) => keyword_text(tok).unwrap_or("token").to_string(),
            None => "end of input".to_string(),
        }
    }

    fn parse_command(&mut self) -> Result<Command, String> {
        match self.peek() {
            Some(Tok::KwIf) => self.parse_if(),
            Some(Tok::KwWhile) => self.parse_while(false),
            Some(Tok::KwUntil) => self.parse_while(true),
            Some(Tok::KwFor) => self.parse_for(),
            Some(Tok::KwSelect) => self.parse_select(),
            Some(Tok::KwCoproc) => self.parse_coproc(),
            Some(Tok::KwCase) => self.parse_case(),
            Some(Tok::LBrace) => self.parse_group(),
            Some(Tok::KwFunction) => self.parse_function_kw(),
            Some(Tok::Word(_, _)) if self.looks_like_func_def() => self.parse_function_paren(),
            Some(Tok::Subshell { .. }) => {
                let raw = match self.advance() {
                    Some(Tok::Subshell { raw, .. }) => raw,
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
                Some(Tok::Subshell { .. }) => {
                    let raw = match self.advance() {
                        Some(Tok::Subshell { raw, .. }) => raw,
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
                // A keyword-shaped operand (e.g. `[[ $x == function ]]`) --
                // KwRBracket2 already has its own arm above, so anything
                // else keyword_text recognizes here is unambiguously an
                // ordinary operand word, not the test's own closing `]]`.
                // See expect_word's own doc comment.
                Some(tok) if keyword_text(tok).is_some() => {
                    let w = self.expect_word()?;
                    atoms.push(TestAtom::Word(w));
                }
                // `<`/`>` inside `[[ ]]` are bash's lexicographic string
                // comparison operators, not redirects -- but the lexer
                // still tokenizes them as Tok::RedirIn/RedirOut
                // unconditionally (it has no notion of "inside `[[ ]]`"),
                // so they're translated back into literal "<"/">" words
                // here instead.
                Some(Tok::RedirIn) => {
                    self.advance();
                    atoms.push(TestAtom::Word(Word { chunks: vec![Chunk::Str("<".to_string())], globbable: false }));
                }
                Some(Tok::RedirOut { append: false, .. }) => {
                    self.advance();
                    atoms.push(TestAtom::Word(Word { chunks: vec![Chunk::Str(">".to_string())], globbable: false }));
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
                return matches!(self.toks.get(self.pos + 1), Some(Tok::Subshell { raw, .. }) if raw.is_empty());
            }
        }
        false
    }

    // A function body is a *compound* command -- a brace group, a
    // subshell, a loop, an `if`, a `case`, `(( ))` or `[[ ]]`. bash
    // rejects `f() echo hi`, and so must this: read as a simple command
    // it defined a function whose body was `echo`, silently, with `hi`
    // vanishing.
    fn parse_function_body(&mut self) -> Result<Command, String> {
        let compound = matches!(
            self.peek(),
            Some(
                Tok::LBrace
                    | Tok::Subshell { .. }
                    | Tok::KwIf
                    | Tok::KwWhile
                    | Tok::KwUntil
                    | Tok::KwFor
                    | Tok::KwSelect
                    | Tok::KwCase
                    | Tok::Arith(_)
                    | Tok::KwLBracket2
            )
        );
        if !compound {
            return Err(format!("near unexpected token `{}'", self.offending_token_text()));
        }
        self.parse_command()
    }

    fn parse_function_paren(&mut self) -> Result<Command, String> {
        let name = match self.advance() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks).unwrap(),
            _ => unreachable!(),
        };
        self.advance(); // empty Subshell("") standing in for `()`
        self.skip_terminators();
        let body = self.parse_function_body()?;
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
        if matches!(self.peek(), Some(Tok::Subshell { raw, .. }) if raw.is_empty()) {
            self.advance();
        }
        self.skip_terminators();
        let body = self.parse_function_body()?;
        Ok(Command::FuncDef { name, body: Box::new(body) })
    }

    fn parse_if(&mut self) -> Result<Command, String> {
        self.advance(); // KwIf
        let mut branches = Vec::new();

        let cond = self.parse_required_list_until(&[Tok::KwThen])?;
        self.expect(Tok::KwThen)?;
        let body = self.parse_required_list_until(&[Tok::KwElif, Tok::KwElse, Tok::KwFi])?;
        branches.push((cond, body));

        loop {
            match self.peek() {
                Some(Tok::KwElif) => {
                    self.advance();
                    let c = self.parse_required_list_until(&[Tok::KwThen])?;
                    self.expect(Tok::KwThen)?;
                    let b = self.parse_required_list_until(&[Tok::KwElif, Tok::KwElse, Tok::KwFi])?;
                    branches.push((c, b));
                }
                Some(Tok::KwElse) => {
                    self.advance();
                    let else_body = self.parse_required_list_until(&[Tok::KwFi])?;
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
        let cond = self.parse_required_list_until(&[Tok::KwDo])?;
        self.expect(Tok::KwDo)?;
        let body = self.parse_required_list_until(&[Tok::KwDone])?;
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
            let body = self.parse_required_list_until(&[Tok::KwDone])?;
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
            let list = self.parse_word_list();
            if matches!(self.peek(), Some(Tok::Semi) | Some(Tok::Newline)) {
                self.advance();
            }
            words = Some(list);
        }
        // No "in ...": bash iterates "$@" here (words stays None).
        self.skip_terminators();
        self.expect(Tok::KwDo)?;
        let body = self.parse_required_list_until(&[Tok::KwDone])?;
        self.expect(Tok::KwDone)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::For { var, words, body, redirects })
    }

    // `select var [in word...]; do body; done` -- identical grammar to
    // `for`'s in-list form (see parse_for above), just a different keyword
    // and runtime behavior (a numbered menu + read loop instead of plain
    // iteration).
    fn parse_select(&mut self) -> Result<Command, String> {
        self.advance(); // KwSelect
        let var = match self.advance() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks)
                .ok_or_else(|| "expected a plain variable name after 'select'".to_string())?,
            other => return Err(format!("expected variable name after 'select', got {:?}", other)),
        };
        self.skip_terminators();

        let mut words = None;
        if matches!(self.peek(), Some(Tok::KwIn)) {
            self.advance();
            let list = self.parse_word_list();
            if matches!(self.peek(), Some(Tok::Semi) | Some(Tok::Newline)) {
                self.advance();
            }
            words = Some(list);
        }
        self.skip_terminators();
        self.expect(Tok::KwDo)?;
        let body = self.parse_required_list_until(&[Tok::KwDone])?;
        self.expect(Tok::KwDone)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::Select { var, words, body, redirects })
    }

    // `for`/`select`'s shared `in word...` wordlist -- keyword-shaped
    // items included unconditionally (no "first item" gating the way
    // parse_simple_command's own fix needs): confirmed against real bash,
    // `for x in if while do done; do echo "[$x]"; done` prints each of
    // those words verbatim, since none of them are in a position that
    // could plausibly open a *new* construct -- the wordlist only ever
    // ends at a `;`/newline (checked by the caller right after this
    // returns), never by this loop itself recognizing a keyword. See
    // keyword_text's own doc comment.
    fn parse_word_list(&mut self) -> Vec<Word> {
        let mut list = Vec::new();
        loop {
            match self.peek() {
                Some(Tok::Word(_, _)) => {
                    if let Some(Tok::Word(chunks, globbable)) = self.advance() {
                        list.push(Word { chunks, globbable });
                    }
                }
                Some(tok) if keyword_text(tok).is_some() => {
                    let s = keyword_text(tok).unwrap().to_string();
                    self.advance();
                    list.push(Word { chunks: vec![Chunk::Str(s)], globbable: true });
                }
                _ => break,
            }
        }
        list
    }

    // `coproc [NAME] command`. Only recognizes NAME when the token right
    // after it is `{` (see the Command::Coproc doc comment for why that's
    // the only unambiguous case handled) -- otherwise the word right after
    // `coproc` is the start of the command itself, same as the unnamed
    // form.
    fn parse_coproc(&mut self) -> Result<Command, String> {
        self.advance(); // KwCoproc
        let name = if let (Some(Tok::Word(chunks, true)), Some(Tok::LBrace)) =
            (self.toks.get(self.pos), self.toks.get(self.pos + 1))
        {
            let name = word_to_plain_name(chunks)
                .ok_or_else(|| "expected a plain name after 'coproc'".to_string())?;
            self.advance();
            Some(name)
        } else {
            None
        };
        let body = self.parse_command()?;
        Ok(Command::Coproc { name, body: Box::new(body) })
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
            let body = self.parse_list_until(&[Tok::DSemi, Tok::SemiAmp, Tok::DSemiAmp, Tok::KwEsac])?;
            let term = match self.peek() {
                Some(Tok::DSemi) => {
                    self.advance();
                    CaseTerm::Stop
                }
                Some(Tok::SemiAmp) => {
                    self.advance();
                    CaseTerm::FallThrough
                }
                Some(Tok::DSemiAmp) => {
                    self.advance();
                    CaseTerm::Continue
                }
                // No explicit terminator before `esac` (the last arm may
                // omit `;;` entirely) -- behaves like `;;`.
                _ => CaseTerm::Stop,
            };
            self.skip_terminators();
            arms.push((patterns, body, term));
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
                Some(Tok::RedirOut { append, clobber }) => {
                    let (append, clobber) = (*append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Out { word, append, clobber });
                }
                Some(Tok::RedirIn) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::In(word));
                }
                Some(Tok::RedirInOut) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::InOut(word));
                }
                Some(Tok::RedirErr { append, clobber }) => {
                    let (append, clobber) = (*append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Err { word, append, clobber });
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
                Some(Tok::RedirFdOut { fd, append, clobber }) => {
                    let (fd, append, clobber) = (*fd, *append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdOut { fd, word, append, clobber });
                }
                Some(Tok::RedirFdIn { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdIn { fd, word });
                }
                Some(Tok::RedirFdInOut { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdInOut { fd, word });
                }
                Some(Tok::RedirFdDup { fd, target }) => {
                    let (fd, target) = (*fd, *target);
                    self.advance();
                    redirects.push(Redirect::FdDup { fd, target });
                }
                Some(Tok::RedirDupWord { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdDupWord { fd, word });
                }
                Some(Tok::RedirFdClose { fd }) => {
                    let fd = *fd;
                    self.advance();
                    redirects.push(Redirect::FdClose { fd });
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
        let mut array_word_assigns = Vec::new();
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
                        if let Some((name, mode, items)) = self.try_array_literal_assignment(&w)? {
                            array_assigns.push((name, mode, items));
                            continue;
                        }
                        if let Some((name, mode, val)) = word_as_assignment(&w) {
                            assigns.push((name, mode, val));
                            continue;
                        }
                        in_assign_phase = false;
                    } else if is_declare_family_command(&words) {
                        // `declare -A m=([a]=1 [b]=2)` -- real bash's own
                        // grammar recognizes NAME=(...) as an array
                        // literal anywhere in one of these five builtins'
                        // word list, not just in leading-prefix-
                        // assignment position (see array_word_assigns's
                        // own doc comment on SimpleCommand). Confirmed
                        // against real bash that an ordinary command
                        // (`echo bar=(1 2 3)`) doesn't get this treatment
                        // at all -- it's a syntax error there, which this
                        // doesn't attempt to replicate (falls through to
                        // being read as a plain word here instead, a
                        // pre-existing, narrower divergence left alone).
                        if let Some((name, mode, items)) = self.try_array_literal_assignment(&w)? {
                            array_word_assigns.push((words.len(), name, mode, items));
                            continue;
                        }
                    }
                    words.push(w);
                }
                // A keyword-shaped argument word (e.g. `echo function`), or
                // one right after a leading assignment prefix (`FOO=bar
                // if` -- confirmed against real bash: this really does run
                // a command literally named "if", not open an if-block;
                // an assignment prefix suppresses reserved-word status for
                // the word after it) -- gated on "this isn't the very
                // first token of the whole simple command" so a genuinely
                // bare, unexpected `then`/`do`/`in`/... (no assignment, no
                // preceding word at all) still falls through to the
                // default arm below and errors, matching real bash's own
                // "unexpected token" syntax error there instead of
                // silently accepting it as a command named "then". By
                // construction this is never itself a NAME=value/
                // NAME[i]=value assignment (none of these fixed literals
                // contain '='), so there's no need to run it through
                // word_as_assignment/word_as_index_assignment at all --
                // just ends the assignment phase (if still active) and
                // becomes an ordinary argument word. See expect_word's own
                // doc comment on why the lexer produces a keyword token
                // here in the first place.
                // `x=1 (echo y)`, `arr= (a b)`: a subshell cannot be a
                // command's argument, and after an assignment prefix
                // there is no command for it to *be*. bash calls this a
                // syntax error; bish used to read `arr= (a b)` as the
                // array assignment `arr=(a b)`, which is the leniency
                // that let the formatter emit the broken spelling
                // unnoticed (see try_array_literal_assignment).
                Some(Tok::Subshell { .. })
                    if !assigns.is_empty()
                        || !array_assigns.is_empty()
                        || !index_assigns.is_empty()
                        || !words.is_empty() =>
                {
                    return Err("near unexpected token `('".to_string());
                }
                Some(tok)
                    if keyword_text(tok).is_some()
                        && (!assigns.is_empty() || !array_assigns.is_empty() || !index_assigns.is_empty() || !words.is_empty()) =>
                {
                    let s = keyword_text(tok).unwrap().to_string();
                    self.advance();
                    in_assign_phase = false;
                    words.push(Word { chunks: vec![Chunk::Str(s)], globbable: true });
                }
                Some(Tok::RedirOut { append, clobber }) => {
                    let (append, clobber) = (*append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Out { word, append, clobber });
                }
                Some(Tok::RedirIn) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::In(word));
                }
                Some(Tok::RedirInOut) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::InOut(word));
                }
                Some(Tok::RedirErr { append, clobber }) => {
                    let (append, clobber) = (*append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::Err { word, append, clobber });
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
                Some(Tok::RedirFdOut { fd, append, clobber }) => {
                    let (fd, append, clobber) = (*fd, *append, *clobber);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdOut { fd, word, append, clobber });
                }
                Some(Tok::RedirFdIn { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdIn { fd, word });
                }
                Some(Tok::RedirFdInOut { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdInOut { fd, word });
                }
                Some(Tok::RedirFdDup { fd, target }) => {
                    let (fd, target) = (*fd, *target);
                    self.advance();
                    redirects.push(Redirect::FdDup { fd, target });
                }
                Some(Tok::RedirDupWord { fd }) => {
                    let fd = *fd;
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdDupWord { fd, word });
                }
                Some(Tok::RedirFdClose { fd }) => {
                    let fd = *fd;
                    self.advance();
                    redirects.push(Redirect::FdClose { fd });
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
        Ok(SimpleCommand { assigns, array_assigns, array_word_assigns, index_assigns, words, redirects })
    }

    // Shared by parse_simple_command's prefix-assignment path and its
    // later-word declare-family path: if `w` is a `name=`/`name+=`-
    // shaped word immediately followed by a `Tok::Subshell` (the
    // lexer's raw capture of `(...)`, with no space allowed between
    // `=`/`+=` and `(`, matching bash's own syntax rule), consumes that
    // Subshell token and returns the parsed (name, mode, items);
    // otherwise returns None having consumed nothing beyond `w` itself
    // (which the caller already owns either way).
    fn try_array_literal_assignment(&mut self, w: &Word) -> Result<Option<(String, AssignMode, Vec<ArrayLiteralItem>)>, String> {
        let Some((name, mode, val)) = word_as_assignment(w) else { return Ok(None) };
        // `attached`: `arr=(a b)` is an array assignment, `arr= (a b)`
        // is a syntax error in bash -- the space is the whole
        // difference, and without checking it this shell did the
        // assignment either way. (That is what let the formatter emit
        // the broken spelling for months: bish's own round-trip could
        // not see the damage.)
        if !is_empty_word(&val) || !matches!(self.peek(), Some(Tok::Subshell { attached: true, .. })) {
            return Ok(None);
        }
        let raw = match self.advance() {
            Some(Tok::Subshell { raw: r, .. }) => r,
            _ => unreachable!(),
        };
        Ok(Some((name, mode, split_array_literal_words(&raw)?)))
    }

    // Also accepts a keyword token, converting it back into the literal
    // word it stands for (see keyword_text's own doc comment) -- every
    // call site here is already past the point where a bare word could
    // legitimately start a new command (a redirect target, a case
    // pattern/subject, an operand inside `[[ ]]`, ...), so a keyword token
    // showing up here unambiguously means the lexer over-eagerly
    // keyword-matched an ordinary literal.
    fn expect_word(&mut self) -> Result<Word, String> {
        match self.advance() {
            Some(Tok::Word(chunks, globbable)) => Ok(Word { chunks, globbable }),
            Some(tok) => match keyword_text(&tok) {
                Some(s) => Ok(Word { chunks: vec![Chunk::Str(s.to_string())], globbable: true }),
                None => Err(format!("expected word, got {:?}", Some(tok))),
            },
            None => Err("expected word, got None".to_string()),
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

// `name[index]=value` -- the whole `name[index]=` prefix must be made up of
// literal text (a run of leading Str/LiteralStr chunks; a `$`-containing
// index like `arr[$i]=x` isn't recognized here, falls through to being read
// as a plain word instead), but that text no longer has to land in a
// single chunk -- a quoted piece of the index (`arr["a b"]=x`) now lexes as
// its own LiteralStr chunk rather than merging into its neighbors, so the
// leading run is flattened into one string to search across, and the split
// point after `=` is mapped back onto real chunk boundaries for the value.
fn word_as_index_assignment(w: &Word) -> Option<(String, String, Word)> {
    let mut flat = String::new();
    let mut bounds: Vec<(usize, usize, bool)> = Vec::new(); // (start_in_flat, chunk_idx, is_literal)
    for (ci, c) in w.chunks.iter().enumerate() {
        let (s, is_lit) = match c {
            Chunk::Str(s) => (s, false),
            Chunk::LiteralStr(s) => (s, true),
            _ => break,
        };
        bounds.push((flat.len(), ci, is_lit));
        flat.push_str(s);
    }
    let bracket = flat.find('[')?;
    let name = &flat[..bracket];
    if !is_valid_ident(name) {
        return None;
    }
    let after_bracket = &flat[bracket + 1..];
    let close_rel = after_bracket.find(']')?;
    let index = after_bracket[..close_rel].to_string();
    let close = bracket + 1 + close_rel;
    let value_start = flat[close + 1..].strip_prefix('=')?;
    let value_pos = flat.len() - value_start.len();

    let &(start, chunk_idx, is_lit) = bounds.iter().rfind(|&&(start, _, _)| start <= value_pos)?;
    let chunk_text = match &w.chunks[chunk_idx] {
        Chunk::Str(s) | Chunk::LiteralStr(s) => s,
        _ => unreachable!(),
    };
    let remainder = chunk_text[value_pos - start..].to_string();
    let mut rest_chunks = Vec::new();
    if !remainder.is_empty() {
        rest_chunks.push(if is_lit { Chunk::LiteralStr(remainder) } else { Chunk::Str(remainder) });
    }
    rest_chunks.extend(w.chunks[chunk_idx + 1..].iter().cloned());
    if rest_chunks.is_empty() {
        rest_chunks.push(Chunk::Str(String::new()));
    }
    Some((name.to_string(), index, Word { chunks: rest_chunks, globbable: false }))
}

fn is_empty_word(w: &Word) -> bool {
    matches!(w.chunks.as_slice(), [Chunk::Str(s)] if s.is_empty()) || w.chunks.is_empty()
}

// Re-lexes the raw captured text of an array literal's `(...)` body as a
// plain whitespace-separated word list, classifying each one as a plain
// positional element or a `[index]=value` keyed one (see
// ArrayLiteralItem's own doc comment). Reuses the general word tokenizer
// (rather than a bespoke splitter) so quoting/expansions inside the literal
// (`arr=("$x" 'lit' $(cmd))`) work exactly like anywhere else.
fn split_array_literal_words(raw: &str) -> Result<Vec<ArrayLiteralItem>, String> {
    let toks = Lexer::new(raw).tokenize()?;
    let mut items = Vec::new();
    for (t, _line) in toks {
        match t {
            Tok::Word(chunks, globbable) => {
                let w = Word { chunks, globbable };
                items.push(match array_literal_item_as_index(&w) {
                    Some((index, value)) => ArrayLiteralItem::Keyed(index, value),
                    None => ArrayLiteralItem::Positional(w),
                });
            }
            Tok::Newline => {}
            other => return Err(format!("unexpected token in array literal: {:?}", other)),
        }
    }
    Ok(items)
}

// `[index]=value` -- one element of an array literal's body written with
// an explicit key (`arr=([2]=x [5]=y)`, `assoc=([k]=v)`). Mirrors
// word_as_index_assignment's own bracket/equals-splitting logic minus the
// leading name (an array literal's own elements have no name prefix,
// just the bracketed index itself) -- same literal-chunks-only
// restriction on the "[index]=" prefix (an index containing a bare `$`
// expansion, e.g. `[$i]=x`, isn't recognized here and is instead kept as
// a plain positional element -- rare enough in practice, and matches
// word_as_index_assignment's own precedent).
fn array_literal_item_as_index(w: &Word) -> Option<(String, Word)> {
    let mut flat = String::new();
    let mut bounds: Vec<(usize, usize, bool)> = Vec::new(); // (start_in_flat, chunk_idx, is_literal)
    for (ci, c) in w.chunks.iter().enumerate() {
        let (s, is_lit) = match c {
            Chunk::Str(s) => (s, false),
            Chunk::LiteralStr(s) => (s, true),
            _ => break,
        };
        bounds.push((flat.len(), ci, is_lit));
        flat.push_str(s);
    }
    let rest = flat.strip_prefix('[')?;
    let close_rel = rest.find(']')?;
    let index = rest[..close_rel].to_string();
    let close = 1 + close_rel;
    let value_start = flat[close + 1..].strip_prefix('=')?;
    let value_pos = flat.len() - value_start.len();

    let &(start, chunk_idx, is_lit) = bounds.iter().rfind(|&&(start, _, _)| start <= value_pos)?;
    let chunk_text = match &w.chunks[chunk_idx] {
        Chunk::Str(s) | Chunk::LiteralStr(s) => s,
        _ => unreachable!(),
    };
    let remainder = chunk_text[value_pos - start..].to_string();
    let mut rest_chunks = Vec::new();
    if !remainder.is_empty() {
        rest_chunks.push(if is_lit { Chunk::LiteralStr(remainder) } else { Chunk::Str(remainder) });
    }
    rest_chunks.extend(w.chunks[chunk_idx + 1..].iter().cloned());
    if rest_chunks.is_empty() {
        rest_chunks.push(Chunk::Str(String::new()));
    }
    Some((index, Word { chunks: rest_chunks, globbable: false }))
}

fn word_to_plain_name(chunks: &[Chunk]) -> Option<String> {
    if let [Chunk::Str(s)] = chunks {
        if is_valid_ident(s) {
            return Some(s.clone());
        }
    }
    None
}

// declare/local/export/readonly/typeset: the five builtins real bash's
// own grammar gives special recognition for a `NAME=(...)` array literal
// appearing as a later word, not just in leading-prefix-assignment
// position -- see array_word_assigns's own doc comment on SimpleCommand.
fn is_declare_family_command(words: &[Word]) -> bool {
    let Some(first) = words.first() else { return false };
    matches!(word_to_plain_name(&first.chunks).as_deref(), Some("declare" | "local" | "export" | "readonly" | "typeset"))
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

#[cfg(test)]
mod line_tracking_tests {
    use super::*;

    fn lines_of(src: &str) -> Vec<usize> {
        let toks = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let prog = Parser::new(toks).parse_program().expect("parse");
        prog.iter().map(|item| item.line).collect()
    }

    #[test]
    fn top_level_statements_get_their_own_starting_line() {
        assert_eq!(lines_of("echo a\necho b\necho c\n"), vec![1, 2, 3]);
    }

    #[test]
    fn blank_lines_and_comments_still_advance_the_count() {
        assert_eq!(lines_of("echo a\n\n# comment\necho b\n"), vec![1, 4]);
    }

    #[test]
    fn a_multi_line_compound_statement_reports_the_line_it_started_on() {
        // The whole `if ... fi` is one ListItem -- its line is where `if`
        // itself sits, not where the block ends.
        assert_eq!(lines_of("if true\nthen\n  echo yes\nfi\necho after\n"), vec![1, 5]);
    }

    #[test]
    fn two_statements_on_one_line_share_that_line() {
        assert_eq!(lines_of("echo a; echo b\necho c\n"), vec![1, 1, 2]);
    }
}
