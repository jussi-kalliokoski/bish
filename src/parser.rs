use crate::lexer::{Chunk, Lexer, Quoting, Tok, VarFdKind, keyword_text};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct Word {
    pub chunks: Vec<Chunk>,
    pub globbable: bool,
}

#[derive(Debug, Clone)]
pub enum Redirect {
    In(Word),
    // `<>`: open the target for reading and writing on fd 0, mostly for
    // `/dev/tcp/HOST/PORT` (see lexer.rs's `Tok::RedirInOut`).
    InOut(Word),
    Out { word: Word, append: bool, clobber: bool },
    Err { word: Word, append: bool, clobber: bool },
    Both { word: Word, append: bool },
    DupErrToOut,
    HereString(Word),
    HereDoc(Word, crate::lexer::HereDocSpelling),
    // `N<<<word` and `N<<DELIM`: those two on another descriptor
    // (`{ read l <&3; } 3<<<text`); with fd 0 they're the plain forms.
    FdHereString { fd: u32, word: Word },
    FdHereDoc { fd: u32, body: Word, spelling: crate::lexer::HereDocSpelling },
    // Any-fd forms: `N>file`, `N>>file`, `N<file`, `N>&M`, `N<&M`.
    FdOut { fd: u32, word: Word, append: bool, clobber: bool },
    // `{name}>file`: the shell picks a free descriptor, opens `word` on it,
    // and assigns its number to `name`, so it can't collide with a
    // hardcoded fd.
    VarFd { var: String, kind: VarFdKind, word: Word },
    FdIn { fd: u32, word: Word },
    // `N<>file`: `InOut` on an explicit fd, the usual shape
    // (`exec 3<>/dev/tcp/host/80`, then `>&3` and `<&3`).
    FdInOut { fd: u32, word: Word },
    // `input`: written `<&` rather than `>&`, the same dup2, kept to write it
    // back as written.
    FdDup { fd: u32, target: u32, input: bool },
    // `[N]>&WORD` / `[N]<&WORD` with a non-literal target (a variable holding
    // an fd, like a coproc's): expanded and parsed when the redirect is
    // resolved.
    FdDupWord { fd: u32, word: Word, input: bool },
    // `[N]>&-` / `[N]<&-`: closes fd N.
    FdClose { fd: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AssignMode {
    Set,
    Append,
}

// One element of an array literal (`arr=(1 [5]=x 2)`, `assoc=([k]=v)`).
// `Keyed`'s index is raw: arithmetic for an indexed array, any string for
// an associative one; `apply_array_literal` decides which, as
// `array_set_index` does. A `Positional` continues from a running index
// a `Keyed` before it moves, as in bash (`(1 [5]=x 2)` is 0, 5, 6).
#[derive(Debug, Clone)]
pub enum ArrayLiteralItem {
    Positional(Word),
    Keyed(String, Word),
}

#[derive(Debug, Clone)]
pub struct SimpleCommand {
    pub assigns: Vec<(String, AssignMode, Word)>,
    // `name=(...)` and `name+=(...)` array literals in the leading
    // assignment position (`arr=(1 2 3) cmd`), apart from `assigns`, since
    // arrays don't fit the scalar `Word`. See `array_word_assigns` for the
    // other place.
    pub array_assigns: Vec<(String, AssignMode, Vec<ArrayLiteralItem>)>,
    // Array literals as later words of declare, local, export, readonly
    // or typeset (`declare -A m=([a]=1 [b]=2)`), which bash recognizes
    // anywhere in those builtins' arguments. The `usize` is the position in
    // `words` it sits at, with no placeholder there, so `run_single`
    // interleaves them back: order matters, since a flag like `-A` before it
    // changes what it means.
    pub array_word_assigns: Vec<(usize, String, AssignMode, Vec<ArrayLiteralItem>)>,
    // `name[index]=value`: `index` is raw arithmetic, evaluated at
    // assignment, and the target one element, hence apart.
    pub index_assigns: Vec<(String, String, AssignMode, Word)>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    // `$BASH_COMMAND`'s text for this command, computed once and kept.
    // bash's answer is the command written back from its tree, not a slice
    // of source (`echo    a     b` reads back as `echo a b`, continuations
    // gone, quoting kept), which serialize.rs produces; a loop body is the
    // same node each time round, so once is enough.
    pub rendered: std::cell::OnceCell<Rc<str>>,
}

// A compound command's trailing redirects (`done < file`,
// `{ ...; } > file`), parsed after its closing keyword, applied through
// `command_own_redirects` and written back by serialize.rs.
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
        // `None`: no `in` clause, which iterates `"$@"`; `Some` is an
        // explicit list, possibly empty (`for x in; do` runs zero times).
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
        // `None` is `"$@"`, as in `For::words`.
        words: Option<Vec<Word>>,
        body: Program,
        redirects: Vec<Redirect>,
    },
    // `coproc [NAME] command`. NAME is recognized only as
    // `coproc NAME { ...; }`, unambiguous since `{` couldn't start a command
    // NAME names; `coproc NAME simple_command` needs the braces. `name: None`
    // is the unnamed form, which sets `COPROC`.
    Coproc {
        name: Option<String>,
        body: Box<Command>,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, Program, CaseTerm)>,
        redirects: Vec<Redirect>,
    },
    Group(Program, Vec<Redirect>),
    FuncDef {
        name: String,
        body: Box<Command>,
    },
    // The raw text of a `( )` subshell, parsed when it runs (see exec.rs).
    Subshell(String, Vec<Redirect>),
    // Standalone ((expr)) arithmetic command.
    Arith(String, Vec<Redirect>),
    // `[[ expr ]]`, as a flat token stream: exec.rs's evaluator handles
    // precedence (`!`, binary tests, `&&`, `||`), as `Subshell` and `Arith`
    // defer theirs.
    Test(Vec<TestAtom>, Vec<Redirect>),
}

// After a case arm's body: `;;` stops, `;&` runs the next arm's body
// without testing, `;;&` goes on testing the following patterns.
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
    /// `time` before the pipeline: a reserved word, not a builtin, so it
    /// times all of it, where a builtin would see one command.
    pub timed: Option<TimeStyle>,
}

/// Which shape `time` reports: `-p` is POSIX's three plain lines; the
/// default is bash's `0m0.000s`.
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
    // The 1-based line the statement started on, captured where the parser
    // begins this `ListItem` (`parse_list_until`), for the debugger's
    // breakpoints and current line. `0` for a `ListItem` built rather than
    // parsed (`declare -f`, `functions_preamble`).
    pub line: usize,
    // The line the statement ended on, its and-or list's last token.
    // Whether a newline separates it from the next decides when a new alias
    // applies: bash reads a whole statement before running it, so
    // `alias hi=...; hi` doesn't expand and `alias hi=...<newline>hi` does.
    // Start lines can't tell (`for ... done; hi`). See `run_program`.
    pub end_line: usize,
}

pub type Program = Vec<ListItem>;

pub struct Parser {
    toks: Vec<Tok>,
    // Parallel to `toks`: `lines[i]` is where `toks[i]` started. See
    // `ListItem::line`.
    lines: Vec<usize>,
    pos: usize,
    // Whether the list ended with a real `;`, newline or `&` rather than its
    // stop token: `{ : }` is a syntax error in bash, `{ :; }` isn't.
    list_ended_with_separator: bool,
}

impl Parser {
    pub fn new(toks: Vec<(Tok, usize)>) -> Self {
        let (toks, lines) = toks.into_iter().unzip();
        Parser { toks, lines, pos: 0, list_ended_with_separator: false }
    }

    // The source line of the current token, or `0` (as for unparsed
    // `ListItem`s) once every token is consumed.
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
            other => Err(format!("expected `{}', found `{}'", describe_token(&want), describe_next(other.as_ref()))),
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
            return Err(format!("unexpected `{}'", describe_token(other)));
        }
        Ok(prog)
    }

    // `parse_list_until` where the grammar needs at least one command:
    // conditions and loop and `if` bodies. bash rejects `if true; then fi`,
    // `while true; do done` and `while; do :; done`.
    fn parse_required_list_until(&mut self, stops: &[Tok]) -> Result<Program, String> {
        let list = self.parse_list_until(stops)?;
        if list.is_empty() {
            return Err(format!("near unexpected token `{}'", self.offending_token_text()));
        }
        Ok(list)
    }

    // ListItems until the next token, past separators, is one of `stops`
    // (or the end of input when it's empty). The stop isn't consumed.
    fn parse_list_until(&mut self, stops: &[Tok]) -> Result<Program, String> {
        let mut items = Vec::new();
        // Newlines only: a `;` where a command is expected is a syntax
        // error, not an empty statement (bash rejects `; echo a`,
        // `echo a; ; echo b`, `{ ; echo a; }`). Blank lines really are
        // skippable.
        self.skip_newlines();
        while !self.at_any(stops) {
            if self.peek().is_none() {
                if stops.is_empty() {
                    break;
                }
                let expected: Vec<String> = stops.iter().map(|t| format!("`{}'", describe_token(t))).collect();
                return Err(format!("unexpected end of input, expected one of {}", expected.join(", ")));
            }
            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            // Read before the separator is consumed: the statement's last
            // token.
            let end_line = self.lines.get(self.pos.wrapping_sub(1)).copied().unwrap_or(line);
            self.list_ended_with_separator = true;
            let sep = match self.peek() {
                Some(Tok::Amp) => {
                    self.advance();
                    Sep::Background
                }
                Some(Tok::Semi) | Some(Tok::Newline) => {
                    self.advance();
                    Sep::Seq
                }
                // Nothing separated this command from the next. A simple
                // command takes its own arguments, so this follows a
                // compound one: `f() { :; } f() { :; }` is an error.
                _ if !self.at_any(stops) && self.peek().is_some() => {
                    return Err(format!("near unexpected token `{}'", self.offending_token_text()));
                }
                _ => {
                    self.list_ended_with_separator = false;
                    Sep::Seq
                }
            };
            items.push(ListItem { and_or, sep, line, end_line });
            self.skip_newlines();
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
        // `time [-p]` and `!` in either order: the grammar puts `time` first,
        // but bash takes `! time true`. Recognized by text, since both are
        // keywords only where a command starts (`echo time` is a word).
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

    // The token a syntax error names, as bash does: the keyword or word, not
    // its Debug spelling.
    fn offending_token_text(&self) -> String {
        match self.peek() {
            Some(tok) => describe_token(tok),
            None => "end of input".to_string(),
        }
    }

    fn parse_command(&mut self) -> Result<Command, String> {
        // Every nesting level of a compound command (an `if` or loop body,
        // a group, a function) comes through here, so this is where a deep
        // script (a few thousand `if`s) would overflow the stack; bash runs
        // it. The guard bites at two thirds of the stack (stackguard),
        // leaving room to unwind and drop the partial tree, whose drop
        // frames are smaller. `( )` and `$(( ))` don't recurse here: the
        // lexer captures their text, and their depth is checked when they
        // run.
        if crate::stackguard::nearly_exhausted() {
            return Err("too deeply nested".to_string());
        }
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

    // `[[ expr ]]`: a flat `TestAtom` stream up to the matching `]]`, with
    // `&&`, `||` and a bare `!` as test operators rather than the shell's,
    // so `[[ a && b ]]` isn't split into two commands.
    fn parse_double_bracket(&mut self) -> Result<Command, String> {
        self.advance(); // KwLBracket2
        let atoms = self.parse_test_atoms()?;
        match self.advance() {
            Some(Tok::KwRBracket2) => {}
            other => return Err(format!("expected `]]', found `{}'", describe_next(other.as_ref()))),
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
                Some(Tok::Word(chunks, plain)) if *plain && matches!(chunks.as_slice(), [Chunk::Str(s)] if s == "!") => {
                    self.advance();
                    atoms.push(TestAtom::Not);
                }
                Some(Tok::Word(_, _)) => {
                    let w = self.expect_word()?;
                    atoms.push(TestAtom::Word(w));
                }
                // A keyword-shaped operand (`[[ $x == function ]]`): `]]`
                // has its own arm above, so it's an ordinary word. See
                // `expect_word`.
                Some(tok) if keyword_text(tok).is_some() => {
                    let w = self.expect_word()?;
                    atoms.push(TestAtom::Word(w));
                }
                // `<` and `>` in `[[ ]]` are string comparisons, but the
                // lexer, which doesn't know it's inside `[[ ]]`, makes them
                // redirect tokens, so they're turned back into words here.
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
                other => return Err(format!("unexpected `{}' in `[[ ]]'", describe_next(other))),
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

    // A function body is a compound command: a group, subshell, loop, `if`,
    // `case`, `(( ))` or `[[ ]]`. bash rejects `f() echo hi`.
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
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks).ok_or_else(|| "expected a plain function name".to_string())?,
            other => return Err(format!("expected a function name, found `{}'", describe_next(other.as_ref()))),
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
                other => return Err(format!("expected `elif', `else' or `fi', found `{}'", describe_next(other))),
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
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks).ok_or_else(|| "expected a plain variable name after 'for'".to_string())?,
            other => return Err(format!("expected a variable name after `for', found `{}'", describe_next(other.as_ref()))),
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
        // No `in`: bash iterates `"$@"` (`words` stays `None`).
        self.skip_terminators();
        self.expect(Tok::KwDo)?;
        let body = self.parse_required_list_until(&[Tok::KwDone])?;
        self.expect(Tok::KwDone)?;
        let redirects = self.parse_trailing_redirects()?;
        Ok(Command::For { var, words, body, redirects })
    }

    // `select var [in word...]; do body; done`: `for`'s grammar, with a
    // numbered menu and read loop at run time.
    fn parse_select(&mut self) -> Result<Command, String> {
        self.advance(); // KwSelect
        let var = match self.advance() {
            Some(Tok::Word(chunks, _)) => word_to_plain_name(&chunks).ok_or_else(|| "expected a plain variable name after 'select'".to_string())?,
            other => return Err(format!("expected a variable name after `select', found `{}'", describe_next(other.as_ref()))),
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

    // `for` and `select`'s `in word...` list, keyword-shaped words included,
    // as in bash (`for x in if while do done; ...`): none can open a
    // construct here, and the list ends at `;` or a newline, checked by the
    // caller. See `keyword_text`.
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

    // `coproc [NAME] command`. NAME only when `{` follows it (see
    // `Command::Coproc`); otherwise the word starts the command, as in the
    // unnamed form.
    fn parse_coproc(&mut self) -> Result<Command, String> {
        self.advance(); // KwCoproc
        let name = if let (Some(Tok::Word(chunks, true)), Some(Tok::LBrace)) = (self.toks.get(self.pos), self.toks.get(self.pos + 1)) {
            let name = word_to_plain_name(chunks).ok_or_else(|| "expected a plain name after 'coproc'".to_string())?;
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
            // POSIX allows an arm to open with `(`, which scripts use so a
            // `case` in `$( )` balances its parens. The lexer has already
            // folded `(a*|b*)` into one `Subshell` token, so the patterns
            // come from lexing its text, and there's no `)` left to expect.
            let patterns = match self.peek() {
                Some(Tok::Subshell { .. }) => {
                    let Some(Tok::Subshell { raw, .. }) = self.advance() else { unreachable!("just peeked") };
                    parse_case_patterns(&raw)?
                }
                _ => {
                    let mut patterns = vec![self.expect_word()?];
                    while matches!(self.peek(), Some(Tok::Pipe)) {
                        self.advance();
                        patterns.push(self.expect_word()?);
                    }
                    self.expect(Tok::RParen)?;
                    patterns
                }
            };
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
                // No terminator before `esac` (the last arm may omit `;;`):
                // `;;`.
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
        let body = self.parse_required_list_until(&[Tok::RBrace])?;
        // The last command in a brace group must be terminated: `{ : }` is a
        // syntax error, and `{ echo }` doesn't print a brace.
        if !self.list_ended_with_separator {
            return Err("near unexpected token `}'".to_string());
        }
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
                Some(Tok::RedirVarFd { var, kind }) => {
                    let (var, kind) = (var.clone(), kind.clone());
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::VarFd { var, kind, word });
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
                Some(Tok::RedirFdDup { fd, target, input }) => {
                    let (fd, target, input) = (*fd, *target, *input);
                    self.advance();
                    redirects.push(Redirect::FdDup { fd, target, input });
                }
                Some(Tok::RedirDupWord { fd, input }) => {
                    let (fd, input) = (*fd, *input);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdDupWord { fd, word, input });
                }
                Some(Tok::RedirFdClose { fd }) => {
                    let fd = *fd;
                    self.advance();
                    redirects.push(Redirect::FdClose { fd });
                }
                // `N<<<` / `N<<`: the descriptor, then its operator (see
                // `Tok::RedirFdHere`).
                Some(Tok::RedirFdHere { fd }) => {
                    let fd = *fd;
                    self.advance();
                    match self.advance() {
                        Some(Tok::HereString) => {
                            let word = self.expect_word()?;
                            redirects.push(if fd == 0 { Redirect::HereString(word) } else { Redirect::FdHereString { fd, word } });
                        }
                        Some(Tok::HereDoc(chunks, spelling)) => {
                            let body = Word { chunks, globbable: false };
                            redirects.push(if fd == 0 { Redirect::HereDoc(body, spelling) } else { Redirect::FdHereDoc { fd, body, spelling } });
                        }
                        _ => return Err(format!("expected `<<<' or `<<' after `{fd}<'")),
                    }
                }
                Some(Tok::HereString) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::HereString(word));
                }
                Some(Tok::HereDoc(..)) => {
                    let (chunks, spelling) = match self.advance() {
                        Some(Tok::HereDoc(c, spelling)) => (c, spelling),
                        _ => unreachable!(),
                    };
                    redirects.push(Redirect::HereDoc(Word { chunks, globbable: false }, spelling));
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
                        if let Some((name, index, mode, val)) = word_as_index_assignment(&w) {
                            index_assigns.push((name, index, mode, val));
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
                        // `declare -A m=([a]=1 [b]=2)`: bash takes
                        // `NAME=(...)` as an array literal anywhere in
                        // these five builtins' arguments (see
                        // `array_word_assigns`). In an ordinary command
                        // (`echo bar=(1 2 3)`) bash calls it a syntax
                        // error, which here falls through as a word.
                        if let Some((name, mode, items)) = self.try_array_literal_assignment(&w)? {
                            array_word_assigns.push((words.len(), name, mode, items));
                            continue;
                        }
                    }
                    words.push(w);
                }
                // `x=1 (echo y)`, `arr= (a b)`: a subshell can't be an
                // argument, and after an assignment prefix there's no
                // command for it to be: a syntax error, as in bash.
                Some(Tok::Subshell { .. }) if !assigns.is_empty() || !array_assigns.is_empty() || !index_assigns.is_empty() || !words.is_empty() => {
                    return Err("near unexpected token `('".to_string());
                }
                // A keyword-shaped argument (`echo function`), or the word
                // after an assignment prefix (`FOO=bar if` runs a command
                // named `if`, as in bash), is an ordinary word. Only not as
                // the very first token, where a stray `then` stays a syntax
                // error. Never an assignment itself, since no keyword
                // contains `=`. See `expect_word`.
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
                Some(Tok::RedirVarFd { var, kind }) => {
                    let (var, kind) = (var.clone(), kind.clone());
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::VarFd { var, kind, word });
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
                Some(Tok::RedirFdDup { fd, target, input }) => {
                    let (fd, target, input) = (*fd, *target, *input);
                    self.advance();
                    redirects.push(Redirect::FdDup { fd, target, input });
                }
                Some(Tok::RedirDupWord { fd, input }) => {
                    let (fd, input) = (*fd, *input);
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::FdDupWord { fd, word, input });
                }
                Some(Tok::RedirFdClose { fd }) => {
                    let fd = *fd;
                    self.advance();
                    redirects.push(Redirect::FdClose { fd });
                }
                // `N<<<` / `N<<`: the descriptor, then its operator (see
                // `Tok::RedirFdHere`).
                Some(Tok::RedirFdHere { fd }) => {
                    let fd = *fd;
                    self.advance();
                    match self.advance() {
                        Some(Tok::HereString) => {
                            let word = self.expect_word()?;
                            redirects.push(if fd == 0 { Redirect::HereString(word) } else { Redirect::FdHereString { fd, word } });
                        }
                        Some(Tok::HereDoc(chunks, spelling)) => {
                            let body = Word { chunks, globbable: false };
                            redirects.push(if fd == 0 { Redirect::HereDoc(body, spelling) } else { Redirect::FdHereDoc { fd, body, spelling } });
                        }
                        _ => return Err(format!("expected `<<<' or `<<' after `{fd}<'")),
                    }
                }
                Some(Tok::HereString) => {
                    self.advance();
                    let word = self.expect_word()?;
                    redirects.push(Redirect::HereString(word));
                }
                Some(Tok::HereDoc(..)) => {
                    let (chunks, spelling) = match self.advance() {
                        Some(Tok::HereDoc(c, spelling)) => (c, spelling),
                        _ => unreachable!(),
                    };
                    redirects.push(Redirect::HereDoc(Word { chunks, globbable: false }, spelling));
                }
                _ => break,
            }
        }

        if assigns.is_empty() && array_assigns.is_empty() && index_assigns.is_empty() && words.is_empty() && redirects.is_empty() {
            return Err("expected command".to_string());
        }
        Ok(SimpleCommand { assigns, array_assigns, array_word_assigns, index_assigns, words, redirects, rendered: std::cell::OnceCell::new() })
    }

    // For the prefix-assignment and declare-family paths: if `w` is
    // `name=` or `name+=` directly followed by a `Tok::Subshell` (no space
    // between, as bash requires), consumes it and returns the parsed
    // literal; else `None`, consuming nothing past `w`.
    fn try_array_literal_assignment(&mut self, w: &Word) -> Result<Option<(String, AssignMode, Vec<ArrayLiteralItem>)>, String> {
        let Some((name, mode, val)) = word_as_assignment(w) else { return Ok(None) };
        // `attached`: `arr=(a b)` is an array assignment, `arr= (a b)` a
        // syntax error in bash; the space is the difference.
        if !is_empty_word(&val) || !matches!(self.peek(), Some(Tok::Subshell { attached: true, .. })) {
            return Ok(None);
        }
        let raw = match self.advance() {
            Some(Tok::Subshell { raw: r, .. }) => r,
            _ => unreachable!(),
        };
        Ok(Some((name, mode, split_array_literal_words(&raw)?)))
    }

    // Also takes a keyword token as the word it stands for (see
    // `keyword_text`): every caller is past where a new command could start
    // (a redirect target, a case pattern or subject, a `[[ ]]` operand), so
    // a keyword here is an over-eager lexer match.
    fn expect_word(&mut self) -> Result<Word, String> {
        match self.advance() {
            Some(Tok::Word(chunks, globbable)) => Ok(Word { chunks, globbable }),
            Some(tok) => match keyword_text(&tok) {
                Some(s) => Ok(Word { chunks: vec![Chunk::Str(s.to_string())], globbable: true }),
                None => Err(format!("expected a word, found `{}'", describe_token(&tok))),
            },
            None => Err("expected a word, found the end of the input".to_string()),
        }
    }
}

/// `describe_token` for a token that may be missing, as at the end of
/// input.
fn describe_next(tok: Option<&Tok>) -> String {
    match tok {
        Some(tok) => describe_token(tok),
        None => "the end of the input".to_string(),
    }
}

// A token as a user should see it, not its Rust shape (`{:?}` would
// show `Subshell { raw: "a*", attached: false }`).
fn describe_token(tok: &Tok) -> String {
    match tok {
        Tok::Word(chunks, _) => word_to_plain_name(chunks).unwrap_or_else(|| "word".to_string()),
        Tok::Subshell { raw, .. } => format!("({raw})"),
        // The operators, which `keyword_text` rightly doesn't cover, since
        // `;;` is never a word; an error message still spells them ("found
        // `;;'" in `case a in ;; esac`).
        Tok::DSemi => ";;".to_string(),
        Tok::SemiAmp => ";&".to_string(),
        Tok::DSemiAmp => ";;&".to_string(),
        Tok::Semi => ";".to_string(),
        Tok::Pipe => "|".to_string(),
        Tok::Or => "||".to_string(),
        Tok::And => "&&".to_string(),
        Tok::Amp => "&".to_string(),
        Tok::RParen => ")".to_string(),
        Tok::LBrace => "{".to_string(),
        Tok::RBrace => "}".to_string(),
        Tok::Newline => "newline".to_string(),
        // Redirect operators, spelled as written.
        Tok::RedirIn => "<".to_string(),
        Tok::RedirOut { append, .. } => if *append { ">>" } else { ">" }.to_string(),
        Tok::RedirInOut => "<>".to_string(),
        Tok::RedirErr { append, .. } => if *append { "2>>" } else { "2>" }.to_string(),
        Tok::RedirBoth { append } => if *append { "&>>" } else { "&>" }.to_string(),
        Tok::DupErrToOut => "2>&1".to_string(),
        Tok::RedirFdOut { fd, append, .. } => format!("{fd}{}", if *append { ">>" } else { ">" }),
        Tok::RedirFdIn { fd } => format!("{fd}<"),
        Tok::RedirFdInOut { fd } => format!("{fd}<>"),
        Tok::RedirFdDup { fd, target, input } => format!("{fd}{}&{target}", if *input { "<" } else { ">" }),
        Tok::RedirDupWord { fd, input } => format!("{fd}{}&", if *input { "<" } else { ">" }),
        Tok::RedirFdClose { fd } => format!("{fd}>&-"),
        Tok::HereString => "<<<".to_string(),
        Tok::HereDoc(..) => "<<".to_string(),
        Tok::RedirFdHere { fd } => format!("{fd}<"),
        other => keyword_text(other).unwrap_or("token").to_string(),
    }
}

/// The patterns of a `(a*|b*)` case arm, from the text between its
/// parens. Lexed rather than split on `|`: inside quotes or a bracket
/// expression (`[a|b]`) a `|` is a character.
fn parse_case_patterns(raw: &str) -> Result<Vec<Word>, String> {
    let toks = crate::lexer::Lexer::new(raw).tokenize()?;
    let mut patterns = Vec::new();
    let mut expecting = true;
    // The line each token came from isn't needed here.
    for (tok, _line) in toks {
        match tok {
            Tok::Word(chunks, globbable) if expecting => {
                patterns.push(Word { chunks, globbable });
                expecting = false;
            }
            Tok::Pipe if !expecting => expecting = true,
            // A newline inside the parens is layout: `(\n  a*\n)` is legal.
            Tok::Newline => {}
            other => match keyword_text(&other) {
                // `case x in (in) ...`: a keyword is an ordinary word in
                // pattern position.
                Some(text) if expecting => {
                    patterns.push(Word { chunks: vec![Chunk::Str(text.to_string())], globbable: true });
                    expecting = false;
                }
                _ => return Err(format!("unexpected `{}' in a case pattern", describe_token(&other))),
            },
        }
    }
    match patterns.is_empty() {
        true => Err("a case arm needs a pattern".to_string()),
        false => Ok(patterns),
    }
}

pub(crate) fn word_as_assignment(w: &Word) -> Option<(String, AssignMode, Word)> {
    let first = w.chunks.first()?;
    if let Chunk::Str(s) = first {
        let eq = s.find('=')?;
        let (name, mode) = if let Some(n) = s[..eq].strip_suffix('+') { (n, AssignMode::Append) } else { (&s[..eq], AssignMode::Set) };
        if is_valid_ident(name) {
            let mut rest_chunks = vec![Chunk::Str(s[eq + 1..].to_string())];
            rest_chunks.extend(w.chunks[1..].iter().cloned());
            // An assignment's value is never glob-expanded in bash.
            return Some((name.to_string(), mode, Word { chunks: rest_chunks, globbable: false }));
        }
    }
    None
}

// `name[index]=value`. The `name[index]=` prefix must be literal text (a
// run of `Str`/`LiteralStr` chunks; `arr[$i]=x` falls through as a
// word), but may span chunks: a quoted index piece (`arr["a b"]=x`) is
// its own chunk, so the run is flattened to search, and the split after
// `=` mapped back onto chunks.
fn word_as_index_assignment(w: &Word) -> Option<(String, String, AssignMode, Word)> {
    let mut flat = String::new();
    let mut bounds: Vec<(usize, usize, bool)> = Vec::new(); // (start_in_flat, chunk_idx, is_literal)
    for (ci, c) in w.chunks.iter().enumerate() {
        let (s, is_lit) = match c {
            Chunk::Str(s) => (s, false),
            Chunk::LiteralStr(s, _) => (s, true),
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
    // The matching `]`, not the first: `a[b[0]]=9` is a subscript in a
    // subscript.
    let after_bracket = &flat[bracket + 1..];
    let mut depth = 1;
    let mut close_rel = None;
    for (i, c) in after_bracket.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close_rel = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close_rel = close_rel?;
    let index = after_bracket[..close_rel].to_string();
    let close = bracket + 1 + close_rel;
    // `m[k]+=v` appends to the element, as `x+=v` to a scalar.
    let after_close = &flat[close + 1..];
    let (mode, value_start) = match after_close.strip_prefix("+=") {
        Some(rest) => (AssignMode::Append, rest),
        None => (AssignMode::Set, after_close.strip_prefix('=')?),
    };
    let value_pos = flat.len() - value_start.len();

    let &(start, chunk_idx, is_lit) = bounds.iter().rfind(|&&(start, _, _)| start <= value_pos)?;
    // The tail of a split chunk keeps its spelling: half a `'...'` is still
    // single-quoted.
    let (chunk_text, quoting) = match &w.chunks[chunk_idx] {
        Chunk::Str(s) => (s, Quoting::Single),
        Chunk::LiteralStr(s, q) => (s, *q),
        _ => unreachable!(),
    };
    let remainder = chunk_text[value_pos - start..].to_string();
    let mut rest_chunks = Vec::new();
    if !remainder.is_empty() {
        rest_chunks.push(if is_lit { Chunk::LiteralStr(remainder, quoting) } else { Chunk::Str(remainder) });
    }
    rest_chunks.extend(w.chunks[chunk_idx + 1..].iter().cloned());
    if rest_chunks.is_empty() {
        rest_chunks.push(Chunk::Str(String::new()));
    }
    Some((name.to_string(), index, mode, Word { chunks: rest_chunks, globbable: false }))
}

fn is_empty_word(w: &Word) -> bool {
    matches!(w.chunks.as_slice(), [Chunk::Str(s)] if s.is_empty()) || w.chunks.is_empty()
}

// Re-lexes an array literal's `(...)` body as words, each positional or
// `[index]=value` (see `ArrayLiteralItem`), through the general
// tokenizer so quoting and expansions work as anywhere
// (`arr=("$x" 'lit' $(cmd))`).
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
            other => return Err(format!("unexpected `{}' in an array literal", describe_token(&other))),
        }
    }
    Ok(items)
}

// `[index]=value`, an array literal element with a key
// (`arr=([2]=x [5]=y)`, `assoc=([k]=v)`). `word_as_index_assignment`'s
// splitting without the name, with the same literal-only prefix: `[$i]=x`
// stays a positional element.
fn array_literal_item_as_index(w: &Word) -> Option<(String, Word)> {
    let mut flat = String::new();
    let mut bounds: Vec<(usize, usize, bool)> = Vec::new(); // (start_in_flat, chunk_idx, is_literal)
    for (ci, c) in w.chunks.iter().enumerate() {
        let (s, is_lit) = match c {
            Chunk::Str(s) => (s, false),
            Chunk::LiteralStr(s, _) => (s, true),
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
    // The tail of a split chunk keeps its spelling.
    let (chunk_text, quoting) = match &w.chunks[chunk_idx] {
        Chunk::Str(s) => (s, Quoting::Single),
        Chunk::LiteralStr(s, q) => (s, *q),
        _ => unreachable!(),
    };
    let remainder = chunk_text[value_pos - start..].to_string();
    let mut rest_chunks = Vec::new();
    if !remainder.is_empty() {
        rest_chunks.push(if is_lit { Chunk::LiteralStr(remainder, quoting) } else { Chunk::Str(remainder) });
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

// declare, local, export, readonly, typeset: the builtins where bash
// takes `NAME=(...)` anywhere (see `array_word_assigns`).
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
        // The whole `if ... fi` is one `ListItem`, on the `if`'s line.
        assert_eq!(lines_of("if true\nthen\n  echo yes\nfi\necho after\n"), vec![1, 5]);
    }

    #[test]
    fn two_statements_on_one_line_share_that_line() {
        assert_eq!(lines_of("echo a; echo b\necho c\n"), vec![1, 1, 2]);
    }
    /// A word's text, quoting undone; `word_to_plain_name` answers whether
    /// it's an identifier, and `a*` isn't.
    fn literal_text(chunks: &[Chunk]) -> String {
        chunks
            .iter()
            .map(|c| match c {
                Chunk::Str(s) => s.clone(),
                Chunk::LiteralStr(s, _) => s.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// An arm's patterns as text, enough to see how `(a*|b*)` split.
    fn case_patterns(src: &str) -> Vec<Vec<String>> {
        let toks = crate::lexer::Lexer::new(src).tokenize().expect("lexes");
        let prog = Parser::new(toks).parse_program().expect("parses");
        prog.iter()
            .flat_map(|item| std::iter::once(&item.and_or.first).chain(item.and_or.rest.iter().map(|(_, p)| p)).flat_map(|p| p.commands.iter()))
            .filter_map(|cmd| match cmd {
                Command::Case { arms, .. } => Some(arms),
                _ => None,
            })
            .flat_map(|arms| {
                arms.iter().map(|(patterns, _, _)| patterns.iter().map(|w| literal_text(&w.chunks)).collect::<Vec<String>>()).collect::<Vec<_>>()
            })
            .collect()
    }

    // POSIX's optional `(` before a case pattern: the lexer folds `(a*)`
    // into a `Subshell`, so the arm puts it back, and an arm without the
    // paren parses as before.
    #[test]
    fn a_case_arm_may_open_with_a_paren() {
        assert_eq!(case_patterns("case x in (a*) :;; (b*) :;; esac"), vec![vec!["a*"], vec!["b*"]]);
        assert_eq!(case_patterns("case x in a*) :;; b*) :;; esac"), vec![vec!["a*"], vec!["b*"]]);
        // Both spellings in one statement, as a half-converted script has.
        assert_eq!(case_patterns("case x in (a*) :;; b*) :;; esac"), vec![vec!["a*"], vec!["b*"]]);
    }

    #[test]
    fn a_parenthesised_arm_splits_its_alternatives() {
        assert_eq!(case_patterns("case x in (a*|b*|c) :;; esac"), vec![vec!["a*", "b*", "c"]]);
    }

    // A `|` separates patterns only outside brackets and quotes, hence
    // lexing.
    #[test]
    fn a_pipe_that_is_not_a_separator_stays_in_its_pattern() {
        assert_eq!(case_patterns(r#"case x in ("a|b") :;; esac"#), vec![vec!["a|b"]]);
        assert_eq!(case_patterns(r#"case x in (a\|b) :;; esac"#), vec![vec!["a|b"]]);
    }

    // A keyword is a word in pattern position, inside the parens too.
    #[test]
    fn a_keyword_is_a_pattern_like_any_other_word() {
        assert_eq!(case_patterns("case x in (in) :;; (esac) :;; esac"), vec![vec!["in"], vec!["esac"]]);
    }

    #[test]
    fn an_arm_with_no_pattern_at_all_is_refused() {
        let toks = crate::lexer::Lexer::new("case x in () :;; esac").tokenize().expect("lexes");
        let e = Parser::new(toks).parse_program().expect_err("no pattern to match on");
        assert!(e.contains("needs a pattern"), "{e}");
    }

    // A parser error names what was written, not the token's Rust shape.
    #[test]
    fn a_syntax_error_does_not_print_rust_debug_output() {
        let toks = crate::lexer::Lexer::new("case a in ;; esac").tokenize().expect("lexes");
        let e = Parser::new(toks).parse_program().expect_err("`;;' is not a pattern");
        assert!(e.contains("`;;'"), "the message names the token as written: {e}");
        for leak in ["Subshell", "Tok::", "Some(", "chunks", "{ raw"] {
            assert!(!e.contains(leak), "{leak:?} leaked into a user-facing message: {e}");
        }
    }

    // Deep nesting is an error, not a stack overflow aborting the process:
    // an overflow would be a `SIGABRT` taking the test binary down, so
    // reaching the assertion proves the guard fired. `note_base` gives the
    // stack budget a starting point (a test binary has none); calling it
    // again only records an address.
    #[test]
    fn deeply_nested_input_errors_instead_of_overflowing_the_stack() {
        // On a thread with the stack `stackguard` assumes (the main
        // thread's `ulimit -s`, about 8 MiB; a test thread's default 2 MiB
        // would overflow before the guard). In bish the parser runs on the
        // main thread.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                crate::stackguard::note_base();
                // About 5000 levels of `if`, some 15 KiB: a hostile paste,
                // not anything written.
                let deep = format!("{}echo hi{}", "if true; then ".repeat(5000), " ;fi".repeat(5000));
                let toks = crate::lexer::Lexer::new(&deep).tokenize().expect("the lexer is iterative");
                let err = Parser::new(toks).parse_program().expect_err("too deep to parse");
                assert!(err.contains("too deeply nested"), "{err}");

                // Real code, nowhere near that deep, still parses.
                let ok = format!("{}echo hi{}", "if true; then ".repeat(50), " ;fi".repeat(50));
                let toks = crate::lexer::Lexer::new(&ok).tokenize().expect("lexes");
                assert!(Parser::new(toks).parse_program().is_ok(), "50-deep is well within budget");
            })
            .expect("spawn")
            .join()
            .expect("the guard fired rather than the stack overflowing");
    }
}
