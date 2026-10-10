// One debug adapter as a subprocess, and the session with it: the half of
// DAP that owns a process, where `dap.rs` is the wire format (as with
// `lsp.rs` and `lspclient.rs`).
//
// Non-blocking pipes, no threads, for lspclient.rs's reason: it's polled
// from the loop drawing the editor. `service` drains what arrived, pushes
// what's queued, and advances the session; nothing waits.
//
// The launch handshake isn't the obvious order, and getting it wrong
// deadlocks:
//
//   1. `initialize` request.
//   2. Its response, with the adapter's capabilities.
//   3. `launch` (or `attach`) request, whose response doesn't come yet.
//   4. The `initialized` event: ready to be configured.
//   5. `setBreakpoints` for every file, then `configurationDone`.
//   6. Now the `launch` response arrives, and the program runs.
//
// Waiting for the launch response before configuring waits forever. This
// is what gdb 17 does, which is why the tests drive a real adapter.
#![allow(dead_code)]

use crate::dap::{self, Message};
use crate::json::{self, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

/// How far the handshake is. Every state but `Exited` moves on by itself
/// as the adapter answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// `initialize` sent, waiting for its answer.
    Initializing,
    /// Launched, awaiting the `initialized` event that says breakpoints can
    /// be sent.
    Launching,
    /// Breakpoints and `configurationDone` sent: the program runs or is about
    /// to.
    Configured,
    /// The adapter said the program ended, or the process died.
    Exited,
}

/// Whether the program is running or sitting at a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Execution {
    Running,
    /// Stopped, with everything the adapter said about why.
    Stopped(dap::Stopped),
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    decoder: dap::Decoder,
    /// What couldn't be written yet because the pipe was full, rather than a
    /// writer thread, as in lspclient.rs.
    outgoing: VecDeque<u8>,
    next_seq: i64,
    /// Which command each in-flight request was, so a response is routed by
    /// what it answers.
    pending: HashMap<i64, String>,
    /// Answers nobody has collected yet, by request seq.
    responses: HashMap<i64, Result<Value, String>>,

    state: State,
    execution: Execution,
    capabilities: Value,
    /// What to send once the adapter says it is ready for it.
    launch_arguments: Value,
    /// Breakpoint lines per file, as asked for, resent on every change since
    /// `setBreakpoints` replaces a file's whole set.
    wanted: HashMap<PathBuf, Vec<usize>>,
    /// What the adapter made of them, keyed the same way.
    placed: HashMap<PathBuf, Vec<dap::Breakpoint>>,
    /// The program's own output, and the adapter's, kept apart.
    output: Vec<dap::Output>,
    exit_code: Option<i64>,
    /// Everything that went wrong, for a user asking why.
    log: Vec<String>,
    stdout_eof: bool,
    stderr_partial: String,
}

impl Session {
    /// Starts `command` as a debug adapter and begins the handshake.
    /// `launch_arguments` go to the adapter unchanged: they're
    /// adapter-specific (gdb wants `program`, debugpy `module` or
    /// `program`, delve `mode`).
    pub fn start(command: &[String], cwd: &Path, launch_arguments: Value) -> Result<Session, String> {
        if !cfg!(feature = "dap") {
            return Err("debug adapters are not part of this build".to_string());
        }
        let Some((program, args)) = command.split_first() else {
            return Err("no command to run".to_string());
        };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped: an adapter that fails to start explains itself on
            // stderr. It's drained every tick, so a chatty adapter can't
            // fill the pipe and block.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;
        // The pipes only have descriptors where a child can be spawned;
        // elsewhere the spawn already failed.
        #[cfg(unix)]
        {
            crate::pty::set_nonblocking(stdout.as_raw_fd());
            crate::pty::set_nonblocking(stderr.as_raw_fd());
            crate::pty::set_nonblocking(stdin.as_raw_fd());
        }

        let mut session = Session {
            child,
            stdin,
            stdout,
            stderr,
            decoder: dap::Decoder::new(),
            outgoing: VecDeque::new(),
            next_seq: 1,
            pending: HashMap::new(),
            responses: HashMap::new(),
            state: State::Initializing,
            execution: Execution::Running,
            capabilities: Value::Null,
            launch_arguments,
            wanted: HashMap::new(),
            placed: HashMap::new(),
            output: Vec::new(),
            exit_code: None,
            log: Vec::new(),
            stdout_eof: false,
            stderr_partial: String::new(),
        };
        session.request(
            "initialize",
            object(vec![
                ("clientID", Value::Str("bish".to_string())),
                ("clientName", Value::Str("bish".to_string())),
                ("adapterID", Value::Str(program.rsplit('/').next().unwrap_or(program).to_string())),
                // One-based on both axes, as the editor's line numbers and
                // `debugger.rs` count.
                ("linesStartAt1", Value::Bool(true)),
                ("columnsStartAt1", Value::Bool(true)),
                ("pathFormat", Value::Str("path".to_string())),
                // Not claimed, because not implemented: an adapter that
                // believed it would ask bish to run the program in a
                // terminal and wait forever.
                ("supportsRunInTerminalRequest", Value::Bool(false)),
                ("supportsProgressReporting", Value::Bool(false)),
            ]),
        );
        Ok(session)
    }

    /// Where a breakpoint should be, before the adapter has its say.
    /// Recorded rather than sent: an adapter takes breakpoints only after
    /// `initialized`. `service` sends them then, and again on every change.
    pub fn set_breakpoints(&mut self, path: &Path, lines: &[usize]) {
        let mut lines = lines.to_vec();
        crate::sort::sort(&mut lines);
        lines.dedup();
        self.wanted.insert(path.to_path_buf(), lines);
        if matches!(self.state, State::Configured) {
            self.send_breakpoints(path);
        }
    }

    /// What the adapter made of one file's breakpoints; empty until it
    /// answers. An unverified one won't be hit, so it's shown differently.
    pub fn breakpoints(&self, path: &Path) -> &[dap::Breakpoint] {
        self.placed.get(path).map_or(&[], |v| v.as_slice())
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn execution(&self) -> &Execution {
        &self.execution
    }

    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    pub fn supports(&self, capability: &str) -> bool {
        dap::supports(&self.capabilities, capability)
    }

    pub fn exit_code(&self) -> Option<i64> {
        self.exit_code
    }

    pub fn log(&self) -> &[String] {
        &self.log
    }

    /// Everything the program and the adapter printed, oldest first.
    pub fn output(&self) -> &[dap::Output] {
        &self.output
    }

    pub fn take_output(&mut self) -> Vec<dap::Output> {
        std::mem::take(&mut self.output)
    }

    // Driving the program

    /// Resume. `thread` is which one, for adapters that stop threads
    /// separately; all accept the stopped thread's id.
    pub fn resume(&mut self, thread: i64) -> i64 {
        self.execution = Execution::Running;
        self.request("continue", object(vec![("threadId", Value::Number(thread as f64))]))
    }

    /// Over the next line, into the next call, or out of this one.
    pub fn step_over(&mut self, thread: i64) -> i64 {
        self.step("next", thread)
    }

    pub fn step_into(&mut self, thread: i64) -> i64 {
        self.step("stepIn", thread)
    }

    pub fn step_out(&mut self, thread: i64) -> i64 {
        self.step("stepOut", thread)
    }

    fn step(&mut self, command: &str, thread: i64) -> i64 {
        self.execution = Execution::Running;
        self.request(command, object(vec![("threadId", Value::Number(thread as f64))]))
    }

    pub fn pause(&mut self, thread: i64) -> i64 {
        self.request("pause", object(vec![("threadId", Value::Number(thread as f64))]))
    }

    pub fn threads(&mut self) -> i64 {
        self.request("threads", Value::Null)
    }

    pub fn stack_trace(&mut self, thread: i64) -> i64 {
        self.request("stackTrace", object(vec![("threadId", Value::Number(thread as f64))]))
    }

    pub fn scopes(&mut self, frame: i64) -> i64 {
        self.request("scopes", object(vec![("frameId", Value::Number(frame as f64))]))
    }

    pub fn variables(&mut self, reference: i64) -> i64 {
        self.request("variables", object(vec![("variablesReference", Value::Number(reference as f64))]))
    }

    /// A watch expression, or a hover. `context` (`hover`, `watch`, `repl`)
    /// is a hint some adapters answer differently for.
    pub fn evaluate(&mut self, expression: &str, frame: Option<i64>, context: &str) -> i64 {
        let mut args = vec![("expression", Value::Str(expression.to_string())), ("context", Value::Str(context.to_string()))];
        if let Some(frame) = frame {
            args.push(("frameId", Value::Number(frame as f64)));
        }
        self.request("evaluate", object(args))
    }

    /// Ends the session, politely if the adapter supports it. Not waited for:
    /// `is_dead` and `service` notice the exit, and the redraw loop mustn't
    /// block on a process that won't leave.
    pub fn terminate(&mut self) {
        match self.supports("supportsTerminateRequest") {
            true => {
                self.request("terminate", object(vec![("restart", Value::Bool(false))]));
            }
            false => {
                self.request("disconnect", object(vec![("terminateDebuggee", Value::Bool(true))]));
            }
        }
    }

    /// A request's answer once it's back. Taken, not borrowed, so a stale
    /// answer can't be read as fresh.
    pub fn take_response(&mut self, seq: i64) -> Option<Result<Value, String>> {
        self.responses.remove(&seq)
    }

    pub fn is_dead(&mut self) -> bool {
        matches!(self.state, State::Exited) || self.child.try_wait().ok().flatten().is_some()
    }

    // The engine

    /// One tick: write what's queued, read what arrived, advance the
    /// session. Returns the events a caller may react to; the rest is
    /// absorbed into this session's state.
    pub fn service(&mut self) -> Vec<Message> {
        self.flush();
        self.drain_stderr();
        let mut buf = [0u8; 8192];
        loop {
            match self.stdout.read(&mut buf) {
                Ok(0) => {
                    self.stdout_eof = true;
                    break;
                }
                Ok(n) => self.decoder.feed(&buf[..n]),
                // Nothing to read now: the ordinary case.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.fail(&format!("reading from the adapter: {e}"));
                    break;
                }
            }
        }
        let mut out = Vec::new();
        while let Some(message) = self.decoder.take_message() {
            match message {
                Ok(message) => {
                    self.absorb(&message);
                    out.push(message);
                }
                Err(e) => {
                    self.log.push(e);
                    if self.decoder.is_failed() {
                        self.fail("the adapter's output stopped making sense");
                        break;
                    }
                }
            }
        }
        // An adapter whose stdout closed is gone, whatever it said last.
        if self.stdout_eof && !matches!(self.state, State::Exited) {
            self.state = State::Exited;
        }
        out
    }

    /// Everything this session learns from a message, in one place.
    fn absorb(&mut self, message: &Message) {
        match message {
            Message::Response { request_seq, command, result } => {
                let command = match self.pending.remove(request_seq) {
                    Some(known) => known,
                    None => command.clone(),
                };
                match (command.as_str(), result) {
                    ("initialize", Ok(body)) => {
                        self.capabilities = body.clone();
                        // The launch goes out now and its answer comes much
                        // later; see the module header.
                        let arguments = self.launch_arguments.clone();
                        self.request("launch", arguments);
                        self.state = State::Launching;
                    }
                    ("setBreakpoints", Ok(body)) => {
                        // The reply doesn't say which file it answers, so
                        // that's remembered from the request (see
                        // `send_breakpoints`).
                        if let Some(path) = self.pending.remove(&-request_seq).map(PathBuf::from) {
                            self.placed.insert(path, dap::breakpoints(body));
                        }
                    }
                    (_, Err(why)) => self.log.push(format!("{command} failed: {why}")),
                    _ => {}
                }
                self.responses.insert(*request_seq, result.clone());
            }
            Message::Event { event, body } => match event.as_str() {
                // Ready to be configured: breakpoints can be sent now,
                // which is why they're recorded until then.
                "initialized" => {
                    for path in self.wanted.keys().cloned().collect::<Vec<_>>() {
                        self.send_breakpoints(&path);
                    }
                    self.request("configurationDone", Value::Null);
                    self.state = State::Configured;
                }
                "stopped" => self.execution = Execution::Stopped(dap::stopped(body)),
                "continued" => self.execution = Execution::Running,
                // A revised breakpoint. gdb sends one for every breakpoint
                // once the program loads, the only way any becomes
                // verified.
                "breakpoint" => {
                    if let Some(updated) = dap::breakpoint_event(body) {
                        self.update_breakpoint(updated);
                    }
                }
                "output" => self.output.push(dap::output(body)),
                "exited" => {
                    self.exit_code = match json::query(body, ".exitCode") {
                        Ok(Value::Number(n)) => Some(*n as i64),
                        _ => None,
                    };
                }
                "terminated" => self.state = State::Exited,
                _ => {}
            },
            // An adapter may ask us things; `runInTerminal` matters, and
            // `initialize` said we can't. A plain refusal beats leaving it
            // waiting.
            Message::Request { seq, command, .. } => {
                let refusal =
                    Message::Response { request_seq: *seq, command: command.clone(), result: Err(format!("bish does not implement {command}")) };
                self.write(&refusal);
            }
        }
    }

    /// Slots a revised breakpoint into its file, matched by id where given,
    /// since the reported line may differ from the one asked for.
    fn update_breakpoint(&mut self, updated: dap::Breakpoint) {
        for (path, placed) in self.placed.iter_mut() {
            if let Some(existing) = placed.iter_mut().find(|b| b.id.is_some() && b.id == updated.id) {
                *existing = updated;
                return;
            }
            // No id, so match on the file it names.
            if updated.source_path.as_deref() == path.to_str()
                && let Some(existing) = placed.iter_mut().find(|b| b.line == updated.line)
            {
                *existing = updated;
                return;
            }
        }
    }

    fn send_breakpoints(&mut self, path: &Path) {
        let lines: Vec<Value> =
            self.wanted.get(path).map(|lines| lines.iter().map(|l| object(vec![("line", Value::Number(*l as f64))])).collect()).unwrap_or_default();
        let seq = self.request(
            "setBreakpoints",
            object(vec![("source", object(vec![("path", Value::Str(path.to_string_lossy().into_owned()))])), ("breakpoints", Value::Array(lines))]),
        );
        // The reply doesn't name the file, so the path is stored under the
        // negated seq: one map, so the halves can't drift.
        self.pending.insert(-seq, path.to_string_lossy().into_owned());
    }

    fn request(&mut self, command: &str, arguments: Value) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(seq, command.to_string());
        self.write(&Message::Request { seq, command: command.to_string(), arguments });
        seq
    }

    fn write(&mut self, message: &Message) {
        self.outgoing.extend(dap::encode(message));
        self.flush();
    }

    /// Pushes as much of the queue as the pipe takes; the rest waits for the
    /// next tick, so a full pipe delays rather than blocks.
    fn flush(&mut self) {
        while !self.outgoing.is_empty() {
            let (front, _) = self.outgoing.as_slices();
            let chunk: Vec<u8> = front.to_vec();
            match self.stdin.write(&chunk) {
                Ok(0) => break,
                Ok(n) => {
                    self.outgoing.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.fail(&format!("writing to the adapter: {e}"));
                    break;
                }
            }
        }
    }

    /// An adapter's stderr explains why it couldn't start. Drained every tick
    /// regardless, since an unemptied pipe eventually stops the writer.
    fn drain_stderr(&mut self) {
        let mut buf = [0u8; 4096];
        loop {
            match self.stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.stderr_partial.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        while let Some(at) = self.stderr_partial.find('\n') {
            let line: String = self.stderr_partial.drain(..=at).collect();
            let line = line.trim_end().to_string();
            if !line.is_empty() {
                self.log.push(line);
            }
        }
    }

    fn fail(&mut self, why: &str) {
        self.log.push(why.to_string());
        self.state = State::Exited;
    }
}

impl Drop for Session {
    /// A running adapter holds the debuggee: a stopped process nobody can
    /// reach, and sometimes a terminal that never comes back. So the child is
    /// killed, not orphaned.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A debug session needs a C compiler, a DAP-speaking debugger and
    /// permission to ptrace; without them the tests skip. gdb has had
    /// `-i=dap` since version 14; older ones never leave `Initializing`.
    fn adapter() -> Option<Vec<String>> {
        let ok = Command::new("gdb").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?.success();
        ok.then(|| vec!["gdb".to_string(), "-i=dap".to_string()])
    }

    struct Fixture {
        dir: PathBuf,
        source: PathBuf,
        program: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A small program with a function to step into and a local to read,
    /// compiled with debug info and no optimisation, which would remove
    /// `total`.
    fn fixture(name: &str) -> Option<Fixture> {
        let dir = std::env::temp_dir().join(format!("bish-dap-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let source = dir.join("prog.c");
        std::fs::write(
            &source,
            "#include <stdio.h>\nint add(int a, int b) { int s = a + b; return s; }\nint main(void) {\n    int total = 0;\n    for (int i = 1; i <= 3; i++) { total = add(total, i); }\n    printf(\"%d\\n\", total);\n    return 0;\n}\n",
        )
        .ok()?;
        let program = dir.join("prog");
        let built =
            Command::new("cc").args(["-g", "-O0", "-o"]).arg(&program).arg(&source).stdout(Stdio::null()).stderr(Stdio::null()).status().ok()?;
        built.success().then_some(Fixture { dir, source, program })
    }

    /// Polls until `done` holds, or gives up. The adapter answers when it
    /// answers; how quickly depends on the machine.
    fn until(session: &mut Session, seconds: u64, done: impl Fn(&Session) -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            session.service();
            if done(session) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    fn answer(session: &mut Session, seq: i64, seconds: u64) -> Value {
        assert!(until(session, seconds, |s| s.responses.contains_key(&seq)), "no answer to request {seq}: {:?}", session.log());
        session.take_response(seq).expect("checked").expect("the adapter answered")
    }

    fn stopped_thread(session: &Session) -> i64 {
        match session.execution() {
            Execution::Stopped(s) => s.thread_id.expect("gdb always says which thread"),
            Execution::Running => panic!("not stopped"),
        }
    }

    /// A real adapter, program and breakpoint: a handshake mistake shows up
    /// as a session that never stops.
    #[test]
    fn a_real_adapter_runs_to_a_real_breakpoint() {
        let (Some(command), Some(fx)) = (adapter(), fixture("breakpoint")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).expect("gdb starts");
        // Line 2 is `add`'s body, reached only by calling it.
        session.set_breakpoints(&fx.source, &[2]);

        assert!(
            until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))),
            "never stopped -- state {:?}, log {:?}",
            session.state(),
            session.log()
        );
        let Execution::Stopped(stop) = session.execution().clone() else { panic!("stopped") };
        assert_eq!(stop.reason, "breakpoint");
        assert!(!stop.hit_breakpoint_ids.is_empty(), "gdb says which breakpoint: {stop:?}");

        // The breakpoint the reply left unverified has been upgraded by the
        // event.
        let placed = session.breakpoints(&fx.source);
        assert_eq!(placed.len(), 1, "{placed:?}");
        assert!(placed[0].verified, "still unverified after the program loaded: {placed:?}");
        assert_eq!(placed[0].line, Some(2));
    }

    #[test]
    fn the_stack_the_scopes_and_the_variables_are_all_reachable_from_a_stop() {
        let (Some(command), Some(fx)) = (adapter(), fixture("inspect")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        session.set_breakpoints(&fx.source, &[2]);
        assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());
        let thread = stopped_thread(&session);

        let seq = session.stack_trace(thread);
        let frames = dap::stack_frames(&answer(&mut session, seq, 10));
        // Stopped inside `add`, called from `main`.
        assert_eq!(frames[0].name, "add", "{frames:?}");
        assert_eq!(frames[0].line, 2);
        assert!(frames.iter().any(|f| f.name == "main"), "{frames:?}");
        assert_eq!(frames[0].source_path.as_deref(), Some(fx.source.to_str().unwrap()));

        let seq = session.scopes(frames[0].id);
        let scopes = dap::scopes(&answer(&mut session, seq, 10));
        let arguments = scopes.iter().find(|s| s.name.contains("Argument")).expect("gdb names one of them Arguments");

        let seq = session.variables(arguments.variables_reference);
        let vars = dap::variables(&answer(&mut session, seq, 10));
        let names: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "{vars:?}");
        // First call: `add(0, 1)`.
        assert_eq!(vars[0].value, "0");
        assert_eq!(vars[1].value, "1");
    }

    #[test]
    fn an_expression_is_evaluated_in_the_frame_that_is_stopped() {
        let (Some(command), Some(fx)) = (adapter(), fixture("evaluate")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        session.set_breakpoints(&fx.source, &[2]);
        assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());
        let thread = stopped_thread(&session);
        let seq = session.stack_trace(thread);
        let frames = dap::stack_frames(&answer(&mut session, seq, 10));

        let seq = session.evaluate("a + b", Some(frames[0].id), "hover");
        assert_eq!(dap::evaluated(&answer(&mut session, seq, 10)).result, "1");

        // A refusal is an answer, in the debugger's own words.
        let seq = session.evaluate("no_such_symbol", Some(frames[0].id), "hover");
        assert!(until(&mut session, 10, |s| s.responses.contains_key(&seq)));
        let refusal = session.take_response(seq).expect("checked").expect_err("gdb has no such symbol");
        assert!(refusal.to_lowercase().contains("no symbol"), "{refusal}");
    }

    #[test]
    fn continuing_hits_the_same_breakpoint_again_and_then_the_program_ends() {
        let (Some(command), Some(fx)) = (adapter(), fixture("continue")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        session.set_breakpoints(&fx.source, &[2]);
        assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());

        // `add` runs three times, `add(0,1)`, `add(1,2)`, `add(3,3)`: the
        // second argument counts up, and the third resume runs off the end.
        for expected in ["2", "3"] {
            let thread = stopped_thread(&session);
            session.resume(thread);
            assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());
            let seq = session.stack_trace(stopped_thread(&session));
            let frames = dap::stack_frames(&answer(&mut session, seq, 10));
            let seq = session.scopes(frames[0].id);
            let scopes = dap::scopes(&answer(&mut session, seq, 10));
            let seq = session.variables(scopes[0].variables_reference);
            let vars = dap::variables(&answer(&mut session, seq, 10));
            assert_eq!(vars[1].value, expected, "the loop counter on this call: {vars:?}");
        }

        let thread = stopped_thread(&session);
        session.resume(thread);
        assert!(until(&mut session, 20, |s| s.exit_code().is_some() || matches!(s.state(), State::Exited)), "{:?}", session.log());
        // And the program's output came back, apart from gdb's.
        let printed: String = session.output().iter().filter(|o| o.category == "stdout").map(|o| o.output.clone()).collect();
        assert!(printed.contains('6'), "0+1+2+3 = 6, printed by the program: {:?}", session.output());
    }

    #[test]
    fn stepping_moves_one_line_at_a_time() {
        let (Some(command), Some(fx)) = (adapter(), fixture("step")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        // Line 4 is `int total = 0;` in `main`.
        session.set_breakpoints(&fx.source, &[4]);
        assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());
        let thread = stopped_thread(&session);
        let seq = session.stack_trace(thread);
        assert_eq!(dap::stack_frames(&answer(&mut session, seq, 10))[0].line, 4);

        session.step_over(thread);
        assert!(
            until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(st) if st.reason != "breakpoint")),
            "a step is not a breakpoint hit: {:?}",
            session.execution()
        );
        let seq = session.stack_trace(thread);
        let frames = dap::stack_frames(&answer(&mut session, seq, 10));
        assert_eq!(frames[0].line, 5, "one line on, still in main: {frames:?}");
        assert_eq!(frames[0].name, "main");
    }

    // The capabilities were negotiated with a real process.
    #[test]
    fn the_adapters_capabilities_are_read_from_the_adapter() {
        let (Some(command), Some(fx)) = (adapter(), fixture("caps")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        assert!(until(&mut session, 20, |s| !matches!(s.state(), State::Initializing)), "{:?}", session.log());
        assert!(session.supports("supportsTerminateRequest"), "gdb says it does: {:?}", session.capabilities());
        assert!(session.supports("supportsConditionalBreakpoints"));
        assert!(!session.supports("supportsSomethingNobodyHas"));
    }

    // A breakpoint on a line with no code: gdb moves it to the next statement
    // rather than refusing it.
    #[test]
    fn a_breakpoint_the_adapter_moves_reports_where_it_landed() {
        let (Some(command), Some(fx)) = (adapter(), fixture("moved")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        // Line 3 is `int main(void) {`, whose code starts on line 4.
        session.set_breakpoints(&fx.source, &[3]);
        assert!(until(&mut session, 20, |s| matches!(s.execution(), Execution::Stopped(_))), "{:?}", session.log());
        let placed = session.breakpoints(&fx.source);
        assert!(placed[0].verified, "{placed:?}");
        assert!(placed[0].line.is_some_and(|l| l >= 3), "wherever it went, it says so: {placed:?}");
    }

    #[test]
    fn an_adapter_that_does_not_exist_fails_at_the_start_rather_than_later() {
        let Err(e) = Session::start(&["no-such-debug-adapter".to_string()], Path::new("."), Value::Null) else {
            panic!("there is nothing to run");
        };
        assert!(e.contains("no-such-debug-adapter"), "{e}");
    }
}
