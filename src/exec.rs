use std::collections::HashMap;
use std::process::{Command, Stdio};

use crate::arith;
use crate::builtins;
use crate::glob;
use crate::lexer::{Chunk, VarOp};
use crate::parser::{
    self, AndOr, Combinator, ListItem, Pipeline, Program, Redirect, Sep, SimpleCommand, Word,
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
    // Indexed arrays (`arr=(...)`). Global only in v1 -- no `local` arrays,
    // no sparse/associative arrays, no `+=`/`arr[i]=` mutation yet.
    arrays: HashMap<String, Vec<String>>,
    // `trap CMD EXIT` handler. Only EXIT is implemented -- real signal traps
    // (INT/TERM/...) would need OS signal-handling machinery this
    // dependency-free, single-process design doesn't have; `trap` warns
    // rather than silently no-oping for those instead of pretending to
    // support them.
    exit_trap: Option<String>,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            last_status: 0,
            functions: HashMap::new(),
            arg_frames: vec![Vec::new()],
            var_scopes: Vec::new(),
            script_name: "ash".to_string(),
            arrays: HashMap::new(),
            exit_trap: None,
        }
    }

    pub fn run_exit_trap(&mut self) {
        if let Some(cmd) = self.exit_trap.take() {
            self.run_source_here(&cmd, "trap");
        }
    }

    // getopts optstring name [args...]. Options requiring an argument are
    // marked with a trailing ':' in optstring (e.g. "ab:c"); a leading ':'
    // switches to "silent" error mode (custom handling via OPTARG/'?'/':'
    // instead of a printed message), matching bash.
    fn run_getopts(&mut self, args: &[String]) -> ExecResult {
        let optstring = args.first().cloned().unwrap_or_default();
        let varname = match args.get(1) {
            Some(v) => v.clone(),
            None => {
                eprintln!("ash: getopts: usage: getopts optstring name [args]");
                return ExecResult::Status(2);
            }
        };
        let positional: Vec<String> =
            if args.len() > 2 { args[2..].to_vec() } else { self.arg_frames.last().cloned().unwrap_or_default() };

        let optind: usize = self.lookup_var("OPTIND").trim().parse().unwrap_or(1);
        let idx = optind.saturating_sub(1);

        if idx >= positional.len() {
            return ExecResult::Status(1);
        }
        let cur = positional[idx].clone();
        if !cur.starts_with('-') || cur == "-" {
            return ExecResult::Status(1);
        }
        if cur == "--" {
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(1);
        }

        let opt_char = cur.chars().nth(1).unwrap_or('?');
        let silent = optstring.starts_with(':');
        let spec = optstring.trim_start_matches(':');

        let Some(pos) = spec.find(opt_char) else {
            if silent {
                self.assign_var(&varname, "?".to_string());
                self.assign_var("OPTARG", opt_char.to_string());
            } else {
                eprintln!("ash: getopts: illegal option -- '{}'", opt_char);
                self.assign_var(&varname, "?".to_string());
            }
            self.assign_var("OPTIND", (optind + 1).to_string());
            return ExecResult::Status(0);
        };

        let needs_arg = spec.as_bytes().get(pos + 1) == Some(&b':');
        if needs_arg {
            let rest: String = cur.chars().skip(2).collect();
            if !rest.is_empty() {
                self.assign_var("OPTARG", rest);
                self.assign_var("OPTIND", (optind + 1).to_string());
            } else if idx + 1 < positional.len() {
                self.assign_var("OPTARG", positional[idx + 1].clone());
                self.assign_var("OPTIND", (optind + 2).to_string());
            } else {
                if silent {
                    self.assign_var(&varname, ":".to_string());
                    self.assign_var("OPTARG", opt_char.to_string());
                } else {
                    eprintln!("ash: getopts: option requires an argument -- '{}'", opt_char);
                    self.assign_var(&varname, "?".to_string());
                }
                self.assign_var("OPTIND", (optind + 1).to_string());
                return ExecResult::Status(0);
            }
        } else {
            self.assign_var("OPTIND", (optind + 1).to_string());
        }
        self.assign_var(&varname, opt_char.to_string());
        ExecResult::Status(0)
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
        ExecResult::Status(self.run_multi(&pipeline.commands, background))
    }

    fn run_command(&mut self, cmd: &parser::Command, background: bool) -> ExecResult {
        let redirects: &[Redirect] = match cmd {
            parser::Command::If { redirects, .. } => redirects,
            parser::Command::While { redirects, .. } => redirects,
            parser::Command::For { redirects, .. } => redirects,
            parser::Command::Case { redirects, .. } => redirects,
            parser::Command::Group(_, redirects) => redirects,
            _ => &[],
        };
        if !redirects.is_empty() {
            return self.run_compound_redirected(cmd, redirects, background);
        }
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
                Redirect::HereDoc(w) => {
                    let content = self.expand_word(w);
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

    // Functions and `local` variables live only in this process's memory, so
    // a self-exec'd child (see run_subshell/run_command_substitution below)
    // starts with no knowledge of them -- unlike a real fork, which
    // duplicates the whole process (this is exactly why e.g. `$(fib
    // $((n-1)))` works in bash even when `n` is a local var: the forked
    // child inherits it for free). Re-declaring visible locals and every
    // currently-known function at the top of the child's script closes that
    // gap without needing unsafe fork(2) or a separate IPC channel.
    fn functions_preamble(&self) -> String {
        let mut s = String::new();
        let mut flattened: HashMap<&str, &str> = HashMap::new();
        for scope in &self.var_scopes {
            for (k, v) in scope {
                flattened.insert(k.as_str(), v.as_str());
            }
        }
        for (k, v) in &flattened {
            s.push_str(k);
            s.push('=');
            s.push_str(&crate::serialize::quote_literal(v));
            s.push('\n');
        }
        for (name, items) in &self.arrays {
            s.push_str(name);
            s.push_str("=(");
            for item in items {
                s.push_str(&crate::serialize::quote_literal(item));
                s.push(' ');
            }
            s.push_str(")\n");
        }
        for (name, body) in &self.functions {
            let def = parser::Command::FuncDef { name: name.clone(), body: Box::new(body.clone()) };
            s.push_str(&crate::serialize::serialize_program(&[ListItem {
                and_or: AndOr {
                    first: Pipeline { commands: vec![def], negate: false },
                    rest: Vec::new(),
                },
                sep: Sep::Seq,
            }]));
        }
        s
    }

    // (...) subshells self-exec the ash binary on the raw captured source
    // (plus the function preamble), inheriting env but not sharing process
    // state -- real bash subshells are forked children too, so mutations
    // inside (cd, variables) must not leak back into the parent. Spawning a
    // real child process gets that isolation for free instead of a separate
    // snapshot/restore mechanism.
    fn run_subshell(&self, raw: &str) -> i32 {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ash: subshell: {}", e);
                return 1;
            }
        };
        let script = self.functions_preamble() + raw;
        match Command::new(exe).arg("-c").arg(script).status() {
            Ok(status) => status.code().unwrap_or(1),
            Err(e) => {
                eprintln!("ash: subshell: {}", e);
                1
            }
        }
    }

    // Compound commands (if/while/for/case/group) with a trailing redirect
    // (`{ ...; } > file`, `done < file`) self-exec too, same as pipeline
    // stages -- avoids needing unsafe fd-dup2 to redirect this process's own
    // stdio for a nested block. serialize_command drops the redirects when
    // reconstructing the child's script (they're applied here, at spawn
    // time, instead), so the child doesn't re-trigger this same path.
    // Trade-off: unlike real bash, state changes inside (cd, variables)
    // won't propagate back to the parent in this specific case.
    fn run_compound_redirected(&mut self, cmd: &parser::Command, redirects: &[Redirect], background: bool) -> ExecResult {
        let redirs = match self.resolve_redirect_list(redirects) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ash: {}", e);
                return ExecResult::Status(1);
            }
        };
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("ash: {}", e);
                return ExecResult::Status(1);
            }
        };
        let script = self.functions_preamble() + &crate::serialize::serialize_command(cmd);
        let mut command = Command::new(exe);
        command.arg("-c").arg(script);
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
                eprintln!("ash: {}", e);
                ExecResult::Status(127)
            }
        }
    }

    fn run_command_substitution(&self, raw: &str) -> String {
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return String::new(),
        };
        let script = self.functions_preamble() + raw;
        match Command::new(exe).arg("-c").arg(script).output() {
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
            for (name, items) in &cmd.array_assigns {
                let values: Vec<String> = items.iter().map(|w| self.expand_word(w)).collect();
                self.arrays.insert(name.clone(), values);
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            return ExecResult::Status(0);
        }

        let argv: Vec<String> = self.expand_words(&cmd.words);
        if argv.is_empty() {
            // Every word vanished (e.g. the command was just an unquoted
            // empty/unset variable) -- matches bash: nothing runs.
            return ExecResult::Status(0);
        }
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
                self.run_exit_trap();
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
            "trap" => {
                if argv.len() < 3 {
                    return ExecResult::Status(0);
                }
                let cmd_str = argv[1].clone();
                for sig in &argv[2..] {
                    if sig == "EXIT" {
                        self.exit_trap = Some(cmd_str.clone());
                    } else {
                        eprintln!(
                            "ash: trap: signal '{}' is not supported yet (only EXIT is honored)",
                            sig
                        );
                    }
                }
                return ExecResult::Status(0);
            }
            "getopts" => return self.run_getopts(&argv[1..]),
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

    // Every pipeline stage is a separate process by necessity (that's what
    // makes piping possible at all), so compound-command stages self-exec
    // just like Subshell already does -- this is actually the *correct*
    // bash semantics too: piped stages always fork, even in real bash.
    fn run_multi(&mut self, commands: &[parser::Command], background: bool) -> i32 {
        let n = commands.len();
        let mut children: Vec<std::process::Child> = Vec::with_capacity(n);
        let mut prev_stdout: Option<Stdio> = None;

        for (i, cmd) in commands.iter().enumerate() {
            let is_last = i == n - 1;
            let default_stdin = prev_stdout.take().unwrap_or_else(Stdio::inherit);
            let default_stdout = if is_last { Stdio::inherit() } else { Stdio::piped() };

            let mut command = match cmd {
                parser::Command::Simple(sc) => {
                    if sc.words.is_empty() {
                        eprintln!("ash: syntax error in pipeline");
                        kill_all(children);
                        return 1;
                    }
                    let argv: Vec<String> = self.expand_words(&sc.words);
                    if argv.is_empty() {
                        continue;
                    }
                    let redirs = match self.resolve_redirects(sc) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("ash: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    let mut command = Command::new(&argv[0]);
                    command.args(&argv[1..]);
                    for (k, val) in &sc.assigns {
                        command.env(k, self.expand_word(val));
                    }
                    command.stdin(redirs.stdin.unwrap_or(default_stdin));
                    command.stdout(redirs.stdout.unwrap_or(default_stdout));
                    command.stderr(redirs.stderr.unwrap_or_else(Stdio::inherit));
                    command
                }
                other => {
                    let exe = match std::env::current_exe() {
                        Ok(p) => p,
                        Err(e) => {
                            eprintln!("ash: {}", e);
                            kill_all(children);
                            return 1;
                        }
                    };
                    let script = self.functions_preamble() + &crate::serialize::serialize_command(other);
                    let mut command = Command::new(exe);
                    command.arg("-c").arg(script);
                    command.stdin(default_stdin);
                    command.stdout(default_stdout);
                    command.stderr(Stdio::inherit());
                    command
                }
            };

            match command.spawn() {
                Ok(mut child) => {
                    if !is_last {
                        prev_stdout = child.stdout.take().map(Stdio::from);
                    }
                    children.push(child);
                }
                Err(e) => {
                    eprintln!("ash: {}", e);
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
                Chunk::Var { name, .. } => s.push_str(&self.lookup_var(name)),
                Chunk::Sub { raw, .. } => s.push_str(&self.run_command_substitution(raw)),
                Chunk::Arith { raw, .. } => match arith::eval(raw, self) {
                    Ok(v) => s.push_str(&v.to_string()),
                    Err(e) => eprintln!("ash: (({})): {}", raw, e),
                },
                Chunk::VarExpand { name, op, .. } => {
                    let name = name.clone();
                    let op = op.clone();
                    s.push_str(&self.eval_var_op(&name, &op));
                }
                Chunk::ArrayVar { name, index, .. } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_element(&name, &index));
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    s.push_str(&self.array_length(&name, &index).to_string());
                }
            }
        }
        s
    }

    // index "@"/"*" joins all elements with a space (used outside the
    // splitting-aware path, where "@" vs "*" can't be distinguished anyway);
    // any other index is evaluated as an arithmetic expression (so
    // `${arr[i+1]}` works) and looked up 0-based.
    fn array_element(&mut self, name: &str, index: &str) -> String {
        if index == "@" || index == "*" {
            return self.arrays.get(name).cloned().unwrap_or_default().join(" ");
        }
        match arith::eval(index, self) {
            Ok(i) if i >= 0 => self
                .arrays
                .get(name)
                .and_then(|v| v.get(i as usize))
                .cloned()
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn array_all(&self, name: &str) -> Vec<String> {
        self.arrays.get(name).cloned().unwrap_or_default()
    }

    fn array_length(&mut self, name: &str, index: &str) -> usize {
        if index == "@" || index == "*" {
            return self.arrays.get(name).map(|v| v.len()).unwrap_or(0);
        }
        match arith::eval(index, self) {
            Ok(i) if i >= 0 => self
                .arrays
                .get(name)
                .and_then(|v| v.get(i as usize))
                .map(|s| s.chars().count())
                .unwrap_or(0),
            _ => 0,
        }
    }

    // Bash word-splitting: unquoted expansion results are split on
    // whitespace (IFS, hardcoded to the default here) into separate fields;
    // literal text (whether from quotes or plain source) never splits, since
    // unquoted literal whitespace would already have ended the word at the
    // lexer level. Only used where splitting actually applies (command
    // argv, `for` word-lists) -- assignment RHS, case words, redirect
    // targets, etc. still go through plain expand_word.
    fn expand_word_split(&mut self, w: &Word) -> Vec<String> {
        let mut fields: Vec<String> = Vec::new();
        let mut current: Option<String> = None;
        for c in &w.chunks {
            match c {
                Chunk::Str(t) => current.get_or_insert_with(String::new).push_str(t),
                Chunk::Var { name, quoted } => {
                    // "$@" is a special case even when quoted: it expands
                    // to one field per positional parameter (as if each
                    // were individually double-quoted), not one joined
                    // string -- unlike "$*", which does join. Unquoted $@
                    // falls through to the normal joined-then-split path,
                    // matching bash (both $@ and $* behave the same
                    // unquoted).
                    if name == "@" && *quoted {
                        let parts = self.arg_frames.last().cloned().unwrap_or_default();
                        append_parts(&mut fields, &mut current, &parts);
                    } else {
                        let v = self.lookup_var(name);
                        append_splittable(&mut fields, &mut current, &v, *quoted);
                    }
                }
                Chunk::Sub { raw, quoted } => {
                    let v = self.run_command_substitution(raw);
                    append_splittable(&mut fields, &mut current, &v, *quoted);
                }
                Chunk::Arith { raw, quoted } => {
                    let v = match arith::eval(raw, self) {
                        Ok(n) => n.to_string(),
                        Err(e) => {
                            eprintln!("ash: (({})): {}", raw, e);
                            String::new()
                        }
                    };
                    append_splittable(&mut fields, &mut current, &v, *quoted);
                }
                Chunk::VarExpand { name, op, quoted } => {
                    let name = name.clone();
                    let op = op.clone();
                    let v = self.eval_var_op(&name, &op);
                    append_splittable(&mut fields, &mut current, &v, *quoted);
                }
                Chunk::ArrayVar { name, index, quoted } => {
                    // "${arr[@]}" is the array analog of "$@": one field per
                    // element even though it's quoted. "${arr[*]}" (quoted
                    // or not) and unquoted "${arr[@]}" join with a space
                    // first, like $*.
                    if index == "@" && *quoted {
                        let items = self.array_all(name);
                        append_parts(&mut fields, &mut current, &items);
                    } else if index == "@" || index == "*" {
                        let joined = self.array_all(name).join(" ");
                        append_splittable(&mut fields, &mut current, &joined, *quoted);
                    } else {
                        let name = name.clone();
                        let index = index.clone();
                        let v = self.array_element(&name, &index);
                        append_splittable(&mut fields, &mut current, &v, *quoted);
                    }
                }
                Chunk::ArrayLength { name, index } => {
                    let name = name.clone();
                    let index = index.clone();
                    let v = self.array_length(&name, &index).to_string();
                    append_splittable(&mut fields, &mut current, &v, true);
                }
            }
        }
        if let Some(c) = current {
            fields.push(c);
        }
        fields
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
            if w.globbable {
                // globbable implies no quoting/expansion at all in the word
                // (see Word::globbable), so splitting can't apply here --
                // glob-check the single literal value as before.
                let s = self.expand_word(w);
                if let Some(matches) = glob::expand(&s) {
                    out.extend(matches);
                    continue;
                }
                out.push(s);
            } else {
                out.extend(self.expand_word_split(w));
            }
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
        self.resolve_redirect_list(&cmd.redirects)
    }

    fn resolve_redirect_list(&mut self, redirects: &[Redirect]) -> Result<ResolvedRedirs, String> {
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        let mut stdin_path: Option<String> = None;
        let mut here_string: Option<String> = None;
        let mut dup_err_to_out = false;

        for r in redirects {
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
                Redirect::HereDoc(w) => {
                    // Body already ends in '\n' from capture_heredoc_body;
                    // reuses the same temp-file Stdio plumbing as <<<.
                    here_string = Some(self.expand_word(w));
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

// Appends an expansion's value `v` to the in-progress word-split state.
// Quoted values are never split (appended verbatim, like literal text).
// Unquoted values are split on whitespace: interior separators end the
// current field and start a new one; leading/trailing whitespace in `v`
// forces a boundary even when the adjacent side has zero-or-one resulting
// parts (e.g. `pre$x` where x=" y " must split into "pre" and "y", not
// "prey"). A purely empty-or-whitespace unquoted value contributes nothing
// (matching bash: an unquoted unset/empty variable standing alone
// contributes zero arguments, not one empty one).
fn append_splittable(fields: &mut Vec<String>, current: &mut Option<String>, v: &str, quoted: bool) {
    if quoted {
        current.get_or_insert_with(String::new).push_str(v);
        return;
    }
    let leading_ws = v.starts_with(char::is_whitespace);
    let trailing_ws = v.ends_with(char::is_whitespace);
    let parts: Vec<&str> = v.split_whitespace().collect();
    if parts.is_empty() {
        if !v.is_empty() {
            if let Some(c) = current.take() {
                fields.push(c);
            }
        }
        return;
    }
    if leading_ws {
        if let Some(c) = current.take() {
            fields.push(c);
        }
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
    if trailing_ws {
        fields.push(current.take().unwrap_or_default());
    }
}

// Like append_splittable, but for "$@": the parts are already well-defined
// (one per positional parameter, never re-split even if a param contains
// whitespace) rather than derived by splitting a joined string.
fn append_parts(fields: &mut Vec<String>, current: &mut Option<String>, parts: &[String]) {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            fields.push(current.take().unwrap_or_default());
        }
        current.get_or_insert_with(String::new).push_str(part);
    }
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
