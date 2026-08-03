use std::process::{Command, Stdio};

use crate::builtins;
use crate::lexer::Chunk;
use crate::parser::{
    self, AndOr, Combinator, Pipeline, Program, Redirect, Sep, SimpleCommand, Word,
};

#[derive(Debug, Clone, Copy)]
pub enum ExecResult {
    Status(i32),
    Break(u32),
    Continue(u32),
}

impl ExecResult {
    fn status(self) -> i32 {
        match self {
            ExecResult::Status(s) => s,
            ExecResult::Break(_) | ExecResult::Continue(_) => 0,
        }
    }

    fn is_signal(self) -> bool {
        matches!(self, ExecResult::Break(_) | ExecResult::Continue(_))
    }
}

pub struct Shell {
    pub last_status: i32,
}

impl Shell {
    pub fn new() -> Self {
        Shell { last_status: 0 }
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
            parser::Command::Simple(sc) => ExecResult::Status(self.run_single(sc, background)),
            parser::Command::If { branches, else_branch, .. } => self.run_if(branches, else_branch),
            parser::Command::While { cond, body, until, .. } => self.run_while(cond, body, *until),
            parser::Command::For { var, words, body, .. } => {
                let var = var.clone();
                let words = words.clone();
                self.run_for(&var, &words, body)
            }
            parser::Command::Case { word, arms, .. } => self.run_case(word, arms),
            parser::Command::Group(prog, _redirects) => self.run_program(prog),
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
            }
        }
        ExecResult::Status(self.last_status)
    }

    fn run_for(&mut self, var: &str, words: &[Word], body: &Program) -> ExecResult {
        for w in words {
            let val = self.expand_word(w);
            unsafe {
                std::env::set_var(var, val);
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
            }
        }
        ExecResult::Status(self.last_status)
    }

    fn run_case(&mut self, word: &Word, arms: &[(Vec<Word>, Program)]) -> ExecResult {
        let val = self.expand_word(word);
        for (patterns, body) in arms {
            for p in patterns {
                let pat = self.expand_word(p);
                if pat == "*" || pat == val {
                    return self.run_program(body);
                }
            }
        }
        ExecResult::Status(0)
    }

    fn run_single(&mut self, cmd: &SimpleCommand, background: bool) -> i32 {
        if cmd.words.is_empty() {
            for (name, val) in &cmd.assigns {
                let v = self.expand_word(val);
                unsafe {
                    std::env::set_var(name, v);
                }
            }
            if !cmd.redirects.is_empty() {
                // side effect only: create/truncate/append the target files
                let _ = self.resolve_redirects(cmd);
            }
            return 0;
        }

        let argv: Vec<String> = cmd.words.iter().map(|w| self.expand_word(w)).collect();
        let name = argv[0].clone();

        // Builtins ignore per-command redirects for now: their output goes
        // straight to the shell's own stdio.
        match name.as_str() {
            "cd" => return builtins::cd(&argv[1..]),
            "export" => return builtins::export(&argv[1..]),
            "exit" => {
                let code = argv
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .unwrap_or(self.last_status);
                std::process::exit(code);
            }
            _ => {}
        }

        let redirs = match self.resolve_redirects(cmd) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ash: {}", e);
                return 1;
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
                    0
                } else {
                    match child.wait() {
                        Ok(status) => status.code().unwrap_or(1),
                        Err(e) => {
                            eprintln!("ash: {}", e);
                            1
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("ash: {}: {}", name, e);
                127
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
            let argv: Vec<String> = cmd.words.iter().map(|w| self.expand_word(w)).collect();

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

    fn expand_word(&self, w: &Word) -> String {
        let mut s = String::new();
        for c in &w.chunks {
            match c {
                Chunk::Str(t) => s.push_str(t),
                Chunk::Var(name) => {
                    if name == "?" {
                        s.push_str(&self.last_status.to_string());
                    } else if let Ok(v) = std::env::var(name) {
                        s.push_str(&v);
                    }
                }
            }
        }
        s
    }

    fn resolve_redirects(&self, cmd: &SimpleCommand) -> Result<ResolvedRedirs, String> {
        let mut stdout_target: Option<(String, bool)> = None;
        let mut stderr_target: Option<(String, bool)> = None;
        let mut stdin_path: Option<String> = None;
        let mut dup_err_to_out = false;

        for r in &cmd.redirects {
            match r {
                Redirect::In(w) => stdin_path = Some(self.expand_word(w)),
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

        let stdin = match stdin_path {
            Some(p) => Some(Stdio::from(
                std::fs::File::open(&p).map_err(|e| format!("{}: {}", p, e))?,
            )),
            None => None,
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

struct ResolvedRedirs {
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
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
