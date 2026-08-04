use std::collections::HashMap;
use std::process::{Command, Stdio};

use crate::arith;
use crate::builtins;
use crate::glob;
use crate::lexer::{Chunk, VarOp};
use crate::parser::{
    self, AndOr, Combinator, Pipeline, Program, Redirect, Sep, SimpleCommand, Word,
};

#[derive(Debug, Clone, Copy)]
pub enum ExecResult {
    Status(i32),
    Break(u32),
    Continue(u32),
    Return(i32),
}

impl ExecResult {
    fn status(self) -> i32 {
        match self {
            ExecResult::Status(s) => s,
            ExecResult::Return(s) => s,
            ExecResult::Break(_) | ExecResult::Continue(_) => 0,
        }
    }

    fn is_signal(self) -> bool {
        matches!(self, ExecResult::Break(_) | ExecResult::Continue(_) | ExecResult::Return(_))
    }
}

pub struct Shell {
    pub last_status: i32,
    functions: HashMap<String, parser::Command>,
    // Stack of positional-parameter frames; last() is the current scope
    // ($0 is tracked separately since it's never shifted/reassigned by calls).
    arg_frames: Vec<Vec<String>>,
    // Stack of `local` overlays; empty unless we're inside a function call.
    // A name only lives here if `local` explicitly declared it -- plain
    // assignment still targets the global (process-env) variable unless it
    // matches an existing local of the same name, matching bash semantics.
    var_scopes: Vec<HashMap<String, String>>,
    script_name: String,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            last_status: 0,
            functions: HashMap::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "ash".to_string(),
        }
    }

    pub fn set_script_args(&mut self, name: String, args: Vec<String>) {
        self.script_name = name;
        self.arg_frames = vec![args];
    }

    pub fn run_program(&mut self, prog: &Program) -> ExecResult {
        let mut result = ExecResult::Status(self.last_status);
        for item in prog {
            let background = matches!(item.sep, Sep::Background);
            result = self.run_and_or(&item.and_or, background);
            self.last_status = result.status();
            if result.is_signal() {
                return result;
            }
        }
        result
    }

    fn run_and_or(&mut self, and_or: &AndOr, background: bool) -> ExecResult {
        let mut result = self.run_pipeline(&and_or.first, background);
        self.last_status = result.status();
        if result.is_signal() {
            return result;
        }
        let mut status = result.status();
        for (comb, pipeline) in &and_or.rest {
            let should_run = match comb {
                Combinator::And => status == 0,
                Combinator::Or => status != 0,
            };
            if should_run {
                result = self.run_pipeline(pipeline, background);
                self.last_status = result.status();
                if result.is_signal() {
                    return result;
                }
                status = result.status();
            }
        }
        ExecResult::Status(status)
    }

    fn run_pipeline(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        let result = self.run_pipeline_inner(pipeline, background);
        if pipeline.negate {
            return match result {
                ExecResult::Status(s) => ExecResult::Status(if s == 0 { 1 } else { 0 }),
                signal => signal,
            };
        }
        result
    }

    fn run_pipeline_inner(&mut self, pipeline: &Pipeline, background: bool) -> ExecResult {
        if pipeline.commands.len() == 1 {
            return self.run_command(&pipeline.commands[0], background);
        }
        if pipeline.commands.iter().any(|c| !matches!(c, parser::Command::Simple(_))) {
            eprintln!("ash: piping compound commands is not supported yet");
            return ExecResult::Status(1);
        }
        let simples: Vec<SimpleCommand> = pipeline
            .commands
            .iter()
            .map(|c| match c {
                parser::Command::Simple(sc) => sc.clone(),
                _ => unreachable!(),
            })
            .collect();
        ExecResult::Status(self.run_multi(&simples, background))
    }

    fn run_command(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        match cmd {
            parser::Command::Simple(sc) => self.run_single(sc, background),
            parser::Command::If { branches, else_branch, .. } => self.run_if(branches, else_branch),
            parser::Command::While { cond, body, until, .. } => self.run_while(cond, body, *until),
            parser::Command::For { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_for(&var, &words, body)
            }
            parser::Command::Case { word, arms, .. } => self.run_case(word, arms),
            parser::Command::Group(prog, _redirects) => self.run_program(prog),
            parser::Command::FuncDef { name, body } => {
                self.functions.insert(name.clone(), (**body).clone());
                ExecResult::Status(0)
            }
            parser::Command::Subshell(raw, _redirects) => ExecResult::Status(self.run_subshell(raw)),
            parser::Command::Arith(raw, _redirects) => match arith::eval(raw, self) {
                Ok(v) => ExecResult::Status(if v != 0 { 0 } else { 1 }),
                Err(e) => {
                    eprintln!("ash: (({})): {}", raw, e);
                    ExecResult::Status(1)
                }
            },
        }
    }

    // `read` is a builtin, so it can't go through the normal Stdio-based
    // redirect machinery (that's built for handing stdio to a *child*
    // process, not reading in-process). Special-cased here since `read x
    // <<< "..."` is too common a pattern to leave broken.
    fn read_input_source(&mut self, cmd: &SimpleCommand) -> Box<dyn std::io::BufRead> {
        for r in cmd.redirects.iter().rev() {
            match r {
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    return Box::new(std::io::Cursor::new(content.into_bytes()));
                }
                Redirect::In(w) => {
                    let p = self.expand_word(w);
                    return match std::fs::File::open(&p) {
                        Ok(f) => Box::new(std::io::BufReader::new(f)),
                        Err(e) => {
                            eprintln!("ash: {}: {}", p, e);
                            Box::new(std::io::Cursor::new(Vec::new()))
                        }
                    };
                }
                _ => continue,
            }
        }
        Box::new(std::io::BufReader::new(std::io::stdin()))
    }

    // Shared by `eval` and `source`/`.` -- both run source text in the
    // CURRENT shell (unlike command substitution/subshells, which self-exec
    // a child process), so `eval`/sourced scripts can set variables,
    // functions, or cwd in the calling shell.
    fn run_source_here(&mut self, src: &str, label: &str) -> ExecResult {
        match crate::lexer::Lexer::new(src).tokenize() {
            Ok(toks) => match crate::parser::Parser::new(toks).parse_program() {
                Ok(prog) => self.run_program(&prog),
                Err(e) => {
                    eprintln!("ash: {}: syntax error: {}", label, e);
                    ExecResult::Status(2)
                }
            },
            Err(e) => {
                eprintln!("ash: {}: syntax error: {}", label, e);
                ExecResult::Status(2)
            }
        }
    }

    // (...) subshells self-exec the ash binary on the raw captured source,
    // inheriting env but not sharing process state -- real bash subshells
    // are forked children too, so mutations inside (cd, variables) must not
    // leak back into the parent. Spawning a real child process gets that
    // isolation for free instead of a separate snapshot/restore mechanism.
    fn run_subshell(&self, raw: &str) -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ash: subshell: {}", e);
                return 1;
            }
        };
        match Command::new(exe).arg("-c").arg(raw).status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("ash: subshell: {}", e);
                1
            }
        }
    }

    fn run_command_substitution(&self, raw: &str) -> String {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return String::new(),
        };
        match Command::new(exe).arg("-c").arg(raw).output() {
            Ok(out) => {
                let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
                while s.ends_with('\n') {
                    s.pop();
                }
                s
            }
            Err(_) => String::new(),
        }
    }

    fn call_function(&mut self, body: &parser::Command, call_args: Vec<String>) -> ExecResult {
        self.arg_frames.push(call_args);
        self.var_scopes.push(HashMap::new());
        let result = self.run_command(body, false);
        self.var_scopes.pop();
        self.arg_frames.pop();
        match result {
            ExecResult::Return(code) => ExecResult::Status(code),
            other => other,
        }
    }

    fn run_if(&mut self, branches: &[(Program, Program)], else_branch: &Option<Program>) -> ExecResult {
        for (cond, body) in branches {
            let cond_result = self.run_program(cond);
            if cond_result.is_signal() {
                return cond_result;
            }
            if cond_result.status() == 0 {
                return self.run_program(body);
            }
        }
        if let Some(else_body) = else_branch {
            return self.run_program(else_body);
        }
        ExecResult::Status(0)
    }

    fn run_while(&mut self, cond: &Program, body: &Program, until: bool) -> ExecResult {
        loop {
            let cond_result = self.run_program(cond);
            if cond_result.is_signal() {
                return cond_result;
            }
            let keep_going = if until { cond_result.status() != 0 } else { cond_result.status() == 0 };
            if !keep_going {
                break;
            }
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ ExecResult::Return(_) => return ret,
            }
        }
        ExecResult::Status(self.last_status)
    }

    fn run_for(&mut self, var: &str, words: &[Word], body: &Program) -> ExecResult {
        let values = self.expand_words(words);
        for val in values {
            self.assign_var(var, val);
            match self.run_program(body) {
                ExecResult::Break(n) => {
                    if n > 1 {
                        return ExecResult::Break(n - 1);
                    }
                    break;
                }
                ExecResult::Continue(n) => {
                    if n > 1 {
                        return ExecResult::Continue(n - 1);
                    }
                    continue;
                }
                ExecResult::Status(s) => self.last_status = s,
                ret @ ExecResult::Return(_) => return ret,
            }
        }
        ExecResult::Status(self.last_status)
    }

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, Program)]) -> ExecResult {
        let val = self.expand_word(word);
        for (patterns, body) in arms {
            for p in patterns {
                let pat = self.expand_word(p);
                if glob::matches(&pat, &val) {
                    return self.run_program(body);
                }
            }
        }
        ExecResult::Status(0)
    }

    fn run_single(&mut self, cmd: &SimpleCommand, background: bool) -> ExecResult {
        if cmd.words.is_empty() {
            for (name, val) in &cmd.assigns {
                let v = self.expand_word(val);
                self.assign_var(name, v);
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            return ExecResult::Status(0);
        }

        let argv: Vec<String> = self.expand_words(&cmd.words);
        let name = argv[0].clone();

        // Builtins ignore per-command redirects for now: their output goes
        // straight to the shell's own stdio.
        match name.as_str() {
            "cd" => return ExecResult::Status(builtins::cd(&argv[1..])),
            "export" => return ExecResult::Status(builtins::export(&argv[1..])),
            "let" => {
                let mut last = 0i64;
                for a in &argv[1..] {
                    match arith::eval(a, self) {
                        Ok(v) => last = v,
                        Err(e) => {
                            eprintln!("ash: let: {}", e);
                            return ExecResult::Status(2);
                        }
                    }
                }
                return ExecResult::Status(if last != 0 { 0 } else { 1 });
            }
            "break" => return builtins::break_loop(&argv[1..]),
            "continue" => return builtins::continue_loop(&argv[1..]),
            "test" => return ExecResult::Status(builtins::test(&argv[1..], false)),
            "[" | "[[" => {
                let closer = if name == "[" { "]" } else { "]]" };
                let mut a = argv[1..].to_vec();
                if a.last().map(|s| s.as_str()) == Some(closer) {
                    a.pop();
                } else {
                    eprintln!("ash: {}: missing closing {}", name, closer);
                    return ExecResult::Status(2);
                }
                return ExecResult::Status(builtins::test(&a, name == "[["));
            }
            "return" => {
                let code = argv.get(1).and_then(|s| s.parse::<i32>().ok()).unwrap_or(self.last_status);
                if self.var_scopes.is_empty() {
                    eprintln!("ash: return: can only 'return' from a function");
                    return ExecResult::Status(code);
                }
                return ExecResult::Return(code);
            }
            "shift" => {
                let n = argv.get(1).and_then(|s| s.parse::<usize>().ok()).unwrap_or(1);
                if let Some(frame) = self.arg_frames.last_mut() {
                    let drain = n.min(frame.len());
                    frame.drain(0..drain);
                }
                return ExecResult::Status(0);
            }
            "local" => {
                if self.var_scopes.is_empty() {
                    eprintln!("ash: local: can only be used inside a function");
                    return ExecResult::Status(1);
                }
                for a in &argv[1..] {
                    let (n, v) = match a.find('=') {
                        Some(eq) => (a[..eq].to_string(), a[eq + 1..].to_string()),
                        None => (a.clone(), String::new()),
                    };
                    self.var_scopes.last_mut().unwrap().insert(n, v);
                }
                return ExecResult::Status(0);
            }
            "exit" => {
                let code = argv
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
                std::process::exit(code);
            }
            "read" => {
                let mut reader = self.read_input_source(cmd);
                let mut line = String::new();
                match std::io::BufRead::read_line(&mut *reader, &mut line) {
                    Ok(0) => return ExecResult::Status(1),
                    Ok(_) => {
                        let line = line.trim_end_matches(['\n', '\r']);
                        let names = &argv[1..];
                        if names.is_empty() {
                            self.assign_var("REPLY", line.to_string());
                        } else {
                            let mut rest = line.trim_start();
                            for (i, n) in names.iter().enumerate() {
                                if i == names.len() - 1 {
                                    self.assign_var(n, rest.trim_end().to_string());
                                } else {
                                    match rest.find(char::is_whitespace) {
                                        Some(pos) => {
                                            self.assign_var(n, rest[..pos].to_string());
                                            rest = rest[pos..].trim_start();
                                        }
                                        None => {
                                            self.assign_var(n, rest.to_string());
                                            rest = "";
                                        }
                                    }
                                }
                            }
                        }
                        return ExecResult::Status(0);
                    }
                    Err(_) => return ExecResult::Status(1),
                }
            }
            "eval" => {
                let src = argv[1..].join(" ");
                return self.run_source_here(&src, "eval");
            }
            "source" | "." => {
                let path = match argv.get(1) {
                    Some(p) => p.clone(),
                    None => {
                        eprintln!("ash: {}: filename argument required", name);
                        return ExecResult::Status(2);
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(src) => return self.run_source_here(&src, &path),
                    Err(e) => {
                        eprintln!("ash: {}: {}", path, e);
                        return ExecResult::Status(1);
                    }
                }
            }
            _ => {}
        }

        if let Some(body) = self.functions.get(&name).cloned() {
            return self.call_function(&body, argv[1..].to_vec());
        }

        let redirs = match self.resolve_redirects(cmd) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ash: {}", e);
                return ExecResult::Status(1);
            }
        };

        let mut command = Command::new(&name);
        command.args(&argv[1..]);
        for (k, val) in &cmd.assigns {
            command.env(k, self.expand_word(val));
        }
        command.stdin(redirs.stdin.unwrap_or_else(Stdio::inherit));
        command.stdout(redirs.stdout.unwrap_or_else(Stdio::inherit));
        command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));

        match command.spawn() {
            Ok(mut child) => {
                if background {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    ExecResult::Status(0)
                } else {
                    match child.wait() {
                        Ok(status) => ExecResult::Status(status.code().unwrap_or(1)),
                        Err(e) => {
                            eprintln!("ash: {}", e);
                            ExecResult::Status(1)
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("ash: {}: {}", name, e);
                ExecResult::Status(127)
            }
        }
    }

    fn run_multi(&mut self, commands: &[SimpleCommand], background: bool) -> i32 {
        let n = commands.len();
        let mut children: Vec<std::process::Child> = Vec::with_capacity(n);
        let mut prev_stdout: Option<Stdio> = None;

        for (i, cmd) in commands.iter().enumerate() {
            if cmd.words.is_empty() {
                eprintln!("ash: syntax error in pipeline");
                kill_all(children);
                return 1;
            }
            let argv: Vec<String> = self.expand_words(&cmd.words);

            let redirs = match self.resolve_redirects(cmd) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("ash: {}", e);
                    kill_all(children);
                    return 1;
                }
            };

            let is_last = i == n - 1;
            let stdin = redirs.stdin.or(prev_stdout.take()).unwrap_or_else(Stdio::inherit);
            let stdout = redirs
                .stdout
                .unwrap_or_else(|| if is_last { Stdio::inherit() } else { Stdio::piped() });
            let stderr = redirs.stderr.unwrap_or_else(Stdio::inherit);

            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            for (k, val) in &cmd.assigns {
                command.env(k, self.expand_word(val));
            }
            command.stdin(stdin);
            command.stdout(stdout);
            command.stderr(stderr);

            match command.spawn() {
                Ok(mut child) => {
                    if !is_last {
                        prev_stdout = child.stdout.take().map(Stdio::from);
                    }
                    children.push(child);
                }
                Err(e) => {
                    eprintln!("ash: {}: {}", argv[0], e);
                    kill_all(children);
                    return 127;
                }
            }
        }

        if background {
            std::thread::spawn(move || {
                for mut c in children {
                    let _ = c.wait();
                }
            });
            return 0;
        }

        let mut status = 0;
        for mut c in children {
            match c.wait() {
                Ok(s) => status = s.code().unwrap_or(1),
                Err(e) => {
                    eprintln!("ash: {}", e);
                    status = 1;
                }
            }
        }
        status
    }

    fn expand_word(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Str(t) => s.push_str(t),
                Chunk::Var(name) => s.push_str(&self.lookup_var(name)),
                Chunk::Sub(raw) => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith(raw) => match arith::eval(raw, self) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => eprintln!("ash: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op } => {
                    let name = name.clone();
                    let op = op.clone();
                    s.push_str(&self.eval_var_op(&name, &op));
                }
            }
        }
        s
    }

    // Re-lexes and expands a captured raw operand (the "word"/"pattern"
    // half of a ${...} expansion), so it can itself contain further $
    // expansions. Multiple resulting words are concatenated with no
    // separator, approximating "the operand as a single expandable value".
    fn expand_raw(&mut self, raw: &str) -> String {
        match crate::lexer::Lexer::new(raw).tokenize() {
            Ok(toks) => {
                let mut s = String::new();
                for t in toks {
                    if let crate::lexer::Tok::Word(chunks, _) = t {
                        s.push_str(&self.expand_word(&Word { chunks, globbable: false }));
                    }
                }
                s
            }
            Err(_) => raw.to_string(),
        }
    }

    fn eval_var_op(&mut self, name: &str, op: &VarOp) -> String {
        let cur = self.lookup_var(name);
        match op {
            VarOp::Length => cur.chars().count().to_string(),
            VarOp::Default { word, .. } => {
                if cur.is_empty() {
                    self.expand_raw(word)
                } else {
                    cur
                }
            }
            VarOp::AssignDefault { word, .. } => {
                if cur.is_empty() {
                    let v = self.expand_raw(word);
                    self.assign_var(name, v.clone());
                    v
                } else {
                    cur
                }
            }
            VarOp::ErrorIfUnset { word, .. } => {
                if cur.is_empty() {
                    let msg = self.expand_raw(word);
                    eprintln!("ash: {}: {}", name, msg);
                    String::new()
                } else {
                    cur
                }
            }
            VarOp::AltIfSet { word, .. } => {
                if !cur.is_empty() {
                    self.expand_raw(word)
                } else {
                    String::new()
                }
            }
            VarOp::RemovePrefix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_prefix_glob(&cur, &pattern, *longest)
            }
            VarOp::RemoveSuffix { pattern, longest } => {
                let pattern = self.expand_raw(pattern);
                strip_suffix_glob(&cur, &pattern, *longest)
            }
        }
    }

    // Expands a simple-command's words into argv, applying filesystem
    // pathname (glob) expansion to any word that's both glob-eligible
    // (no quoting/escaping/expansion at all -- see Word::globbable) and
    // actually contains metacharacters. A pattern with no filesystem
    // matches is kept as its literal text, matching bash's default
    // (nullglob-off) behavior.
    fn expand_words(&mut self, words: &[Word]) -> Vec<String> {
        let mut out = Vec::new();
        for w in words {
            let s = self.expand_word(w);
            if w.globbable {
                if let Some(matches) = glob::expand(&s) {
                    out.extend(matches);
                    continue;
                }
            }
            out.push(s);
        }
        out
    }

    fn lookup_var(&self, name: &str) -> String {
        match name {
            "?" => self.last_status.to_string(),
            "0" => self.script_name.clone(),
            "#" => self.arg_frames.last().map(|a| a.len()).unwrap_or(0).to_string(),
            "@" | "*" => self.arg_frames.last().map(|a| a.join(" ")).unwrap_or_default(),
            _ if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) => {
                let idx: usize = name.parse().unwrap_or(0);
                idx.checked_sub(1)
                    .and_then(|i| self.arg_frames.last().and_then(|a| a.get(i)))
                    .cloned()
                    .unwrap_or_default()
            }
            _ => {
                for scope in self.var_scopes.iter().rev() {
                    if let Some(v) = scope.get(name) {
                        return v.clone();
                    }
                }
                std::env::var(name).unwrap_or_default()
            }
        }
    }

    // Plain assignment targets the global (process-env) variable, unless it
    // shadows an existing `local` of the same name in the current function
    // scope -- matching bash, where functions don't auto-localize vars.
    fn assign_var(&mut self, name: &str, value: String) {
        for scope in self.var_scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return;
            }
        }
        unsafe {
            std::env::set_var(name, value);
        }
    }

    fn resolve_redirects(&mut self, cmd: &SimpleCommand) -> Result<ResolvedRedirs, String> {
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        let mut stdin_path: Option<String> = None;
        let mut here_string: Option<String> = None;
        let mut dup_err_to_out = false;

        for r in &cmd.redirects {
            match r {
                Redirect::In(w) => {
                    stdin_path = Some(self.expand_word(w));
                    here_string = None;
                }
                Redirect::HereString(w) => {
                    let mut content = self.expand_word(w);
                    content.push('\n');
                    here_string = Some(content);
                    stdin_path = None;
                }
                Redirect::Out { word, append } => {
                    stdout_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Err { word, append } => {
                    stderr_target = Some((self.expand_word(word), *append));
                    dup_err_to_out = false;
                }
                Redirect::Both { word, append } => {
                    let p = self.expand_word(word);
                    stdout_target = Some((p.clone(), *append));
                    stderr_target = Some((p, *append));
                    dup_err_to_out = false;
                }
                Redirect::DupErrToOut => dup_err_to_out = true,
            }
        }

        let stdin = if let Some(content) = here_string {
            Some(Stdio::from(here_string_file(&content)?))
        } else {
            match stdin_path {
                Some(p) => Some(Stdio::from(
                    std::fs::File::open(&p).map_err(|e| format!("{}: {}", p, e))?,
                )),
                None => None,
            }
        };
        let stdout = match &stdout_target {
            Some((p, append)) => Some(Stdio::from(open_out(p, *append)?)),
            None => None,
        };
        let stderr = if dup_err_to_out {
            // True fd-dup onto a pipe destination isn't modeled yet; only
            // the common `> file 2>&1` shape is honored in v1.
            match &stdout_target {
                Some((p, append)) => Some(Stdio::from(open_out(p, *append)?)),
                None => None,
            }
        } else {
            match &stderr_target {
                Some((p, append)) => Some(Stdio::from(open_out(p, *append)?)),
                None => None,
            }
        };

        Ok(ResolvedRedirs { stdin, stdout, stderr })
    }
}

impl arith::VarContext for Shell {
    fn get(&self, name: &str) -> i64 {
        self.lookup_var(name).trim().parse().unwrap_or(0)
    }

    fn set(&mut self, name: &str, value: i64) {
        self.assign_var(name, value.to_string());
    }
}

struct ResolvedRedirs {
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
}

// Here-strings need a real Stdio for the child process; simplest portable
// way to hand it literal content is a temp file, unlinked immediately after
// opening (the open fd keeps the data alive on unix even once unlinked).
fn here_string_file(content: &str) -> Result<std::fs::File, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("ash-herestring-{}-{}", std::process::id(), n));
    std::fs::write(&path, content).map_err(|e| format!("here-string: {}", e))?;
    let f = std::fs::File::open(&path).map_err(|e| format!("here-string: {}", e))?;
    let _ = std::fs::remove_file(&path);
    Ok(f)
}

fn strip_prefix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let lens: Vec<usize> = if longest { (0..=chars.len()).rev().collect() } else { (0..=chars.len()).collect() };
    for len in lens {
        let candidate: String = chars[..len].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[len..].iter().collect();
        }
    }
    s.to_string()
}

fn strip_suffix_glob(s: &str, pattern: &str, longest: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let lens: Vec<usize> = if longest { (0..=n).rev().collect() } else { (0..=n).collect() };
    for len in lens {
        let candidate: String = chars[n - len..].iter().collect();
        if glob::matches(pattern, &candidate) {
            return chars[..n - len].iter().collect();
        }
    }
    s.to_string()
}

fn open_out(path: &str, append: bool) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(|e| format!("{}: {}", path, e))
}

fn kill_all(children: Vec<std::process::Child>) {
    for mut c in children {
        let _ = c.kill();
        let _ = c.wait();
    }
}
