// One debug adapter, running as a subprocess, and the state of the
// session with it. The half of the Debug Adapter Protocol that owns a
// process -- `dap.rs` is the wire format and knows nothing about one,
// the same split `lsp.rs`/`lspclient.rs` already has.
//
// Non-blocking pipes and no threads, for the reason lspclient.rs gives
// at length: this is polled from the same loop that is drawing an
// editor, and a read that blocks is a keystroke that does not land.
// `service` is the whole engine -- it drains whatever has arrived,
// pushes whatever is queued, and returns having advanced the session as
// far as the bytes allowed. Nothing here ever waits.
//
// **The launch handshake is not the obvious order**, and getting it
// wrong deadlocks the session. It is:
//
//   1. `initialize` request.
//   2. Its response arrives, carrying the adapter's capabilities.
//   3. `launch` (or `attach`) request -- whose response does *not*
//      arrive yet.
//   4. `initialized` *event*, which is the adapter saying it is ready
//      to be configured.
//   5. `setBreakpoints` for every file, then `configurationDone`.
//   6. *Now* the `launch` response arrives, and the program runs.
//
// A client that waits for the launch response before configuring waits
// for ever. That is not a reading of the specification -- it is what
// gdb 17 does, observed, and it is why the tests here drive a real
// adapter rather than a mock written from the same assumption as the
// code.
#![allow(dead_code)]

use crate::dap::{self, Message};
use crate::json::{self, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

/// How far along the handshake is. Every state but `Exited` is
/// transient and moves on by itself as the adapter answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// `initialize` sent, waiting for its answer.
    Initializing,
    /// Launched, waiting for the `initialized` event that says the
    /// adapter will take breakpoints.
    Launching,
    /// Breakpoints sent, `configurationDone` sent -- the program is
    /// running or about to be.
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
    /// Whatever could not be written yet, because a non-blocking pipe
    /// was full. Same reason lspclient.rs has one: a writer thread is
    /// the alternative, and this is not worth a thread.
    outgoing: VecDeque<u8>,
    next_seq: i64,
    /// Which command each in-flight request was, so a response can be
    /// routed by what it answers rather than only by its number.
    pending: HashMap<i64, String>,
    /// Answers nobody has collected yet, by request seq.
    responses: HashMap<i64, Result<Value, String>>,

    state: State,
    execution: Execution,
    capabilities: Value,
    /// What to send once the adapter says it is ready for it.
    launch_arguments: Value,
    /// Breakpoint lines per file, as asked for -- resent on every
    /// change, because `setBreakpoints` replaces a file's whole set
    /// rather than adding to it.
    wanted: HashMap<PathBuf, Vec<usize>>,
    /// What the adapter made of them, keyed the same way.
    placed: HashMap<PathBuf, Vec<dap::Breakpoint>>,
    /// The program's own output, and the adapter's, kept apart.
    output: Vec<dap::Output>,
    exit_code: Option<i64>,
    /// Everything that went wrong, for a user asking why the session is
    /// not doing what they expected.
    log: Vec<String>,
    stdout_eof: bool,
    stderr_partial: String,
}

impl Session {
    /// Starts `command` as a debug adapter and begins the handshake.
    ///
    /// `launch_arguments` is passed through to the adapter untouched:
    /// what a `launch` request needs is adapter-specific (gdb wants
    /// `program`, debugpy wants `module` or `program`, delve wants
    /// `mode`), and inventing a common shape over them would only be a
    /// second thing to get wrong.
    pub fn start(command: &[String], cwd: &Path, launch_arguments: Value) -> Result<Session, String> {
        let Some((program, args)) = command.split_first() else {
            return Err("no command to run".to_string());
        };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, not null: an adapter that fails to start explains
            // itself on stderr and then exits, and discarding that
            // leaves a dead session with no reason. Never reading it
            // would be worse -- a chatty adapter fills the pipe and
            // blocks -- so it is drained every tick.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;
        crate::pty::set_nonblocking(stdout.as_raw_fd());
        crate::pty::set_nonblocking(stderr.as_raw_fd());
        crate::pty::set_nonblocking(stdin.as_raw_fd());

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
                // One-based on both axes, which is what an editor
                // showing line numbers already counts in -- and what
                // `debugger.rs` uses for its own breakpoints.
                ("linesStartAt1", Value::Bool(true)),
                ("columnsStartAt1", Value::Bool(true)),
                ("pathFormat", Value::Str("path".to_string())),
                // Not claimed, because it is not implemented: an
                // adapter that believes this will ask bish to run the
                // program in a terminal and wait for an answer that
                // never comes.
                ("supportsRunInTerminalRequest", Value::Bool(false)),
                ("supportsProgressReporting", Value::Bool(false)),
            ]),
        );
        Ok(session)
    }

    /// Where a breakpoint should be, before the adapter has had a say.
    ///
    /// Recorded rather than sent immediately: an adapter will not take
    /// breakpoints until it has sent `initialized`, and a session that
    /// is still starting has nowhere to put them yet. `service` sends
    /// them when the moment comes, and again whenever this changes.
    pub fn set_breakpoints(&mut self, path: &Path, lines: &[usize]) {
        let mut lines = lines.to_vec();
        lines.sort_unstable();
        lines.dedup();
        self.wanted.insert(path.to_path_buf(), lines);
        if matches!(self.state, State::Configured) {
            self.send_breakpoints(path);
        }
    }

    /// What the adapter made of the breakpoints in one file. Empty
    /// until it has answered -- and an entry that is `verified: false`
    /// is a breakpoint that will not be hit, which is worth showing
    /// differently rather than pretending.
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

    /// Everything the program (and the adapter) has printed, oldest
    /// first.
    pub fn output(&self) -> &[dap::Output] {
        &self.output
    }

    pub fn take_output(&mut self) -> Vec<dap::Output> {
        std::mem::take(&mut self.output)
    }

    // -----------------------------------------------------------------
    // Driving the program
    // -----------------------------------------------------------------

    /// Resume. `thread` is which one, for an adapter that stops them
    /// individually; every adapter accepts the id of the thread that
    /// stopped.
    pub fn resume(&mut self, thread: i64) -> i64 {
        self.execution = Execution::Running;
        self.request("continue", object(vec![("threadId", Value::Number(thread as f64))]))
    }

    /// Over the next line, into the next call, or out of this one --
    /// the three steps every debugger has and every adapter names the
    /// same way.
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

    /// A watch expression, or what a hover should say. `context` is the
    /// adapter's own hint -- `hover`, `watch`, `repl` -- and some
    /// adapters answer differently for each.
    pub fn evaluate(&mut self, expression: &str, frame: Option<i64>, context: &str) -> i64 {
        let mut args = vec![("expression", Value::Str(expression.to_string())), ("context", Value::Str(context.to_string()))];
        if let Some(frame) = frame {
            args.push(("frameId", Value::Number(frame as f64)));
        }
        self.request("evaluate", object(args))
    }

    /// Ends the session, politely if the adapter said it can be. The
    /// process is not waited for here -- `is_dead` and `service` notice
    /// it going, and blocking on a subprocess that has decided not to
    /// leave is exactly what an editor's redraw loop must not do.
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

    /// The answer to a request, once it has come back. Taken rather
    /// than borrowed: a caller asks once and gets it once, which is
    /// what stops a stale answer being read as a fresh one.
    pub fn take_response(&mut self, seq: i64) -> Option<Result<Value, String>> {
        self.responses.remove(&seq)
    }

    pub fn is_dead(&mut self) -> bool {
        matches!(self.state, State::Exited) || self.child.try_wait().ok().flatten().is_some()
    }

    // -----------------------------------------------------------------
    // The engine
    // -----------------------------------------------------------------

    /// One tick: write what is queued, read what has arrived, and
    /// advance the session by whatever it said. Returns the events that
    /// the caller may want to react to -- everything else is absorbed
    /// into this session's own state.
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
                // Nothing to read right now, which is the ordinary case
                // and not a failure.
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
        // An adapter whose stdout has closed is an adapter that has
        // gone, whatever it said last.
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
                        // The launch goes out now and its answer comes
                        // back much later -- see this module's own
                        // header for why that is not a bug.
                        let arguments = self.launch_arguments.clone();
                        self.request("launch", arguments);
                        self.state = State::Launching;
                    }
                    ("setBreakpoints", Ok(body)) => {
                        // Which file this answers is not in the reply,
                        // so it is remembered from the request -- see
                        // `send_breakpoints`.
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
                // The adapter is ready to be configured. This is the
                // moment breakpoints become sendable, and the reason
                // they are recorded before it rather than sent.
                "initialized" => {
                    for path in self.wanted.keys().cloned().collect::<Vec<_>>() {
                        self.send_breakpoints(&path);
                    }
                    self.request("configurationDone", Value::Null);
                    self.state = State::Configured;
                }
                "stopped" => self.execution = Execution::Stopped(dap::stopped(body)),
                "continued" => self.execution = Execution::Running,
                // An adapter revising a breakpoint it could not place
                // earlier. gdb sends one of these for every breakpoint
                // once the program loads, which is the only way any of
                // them ever becomes verified.
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
            // An adapter may ask *us* things -- `runInTerminal` is the
            // one that matters, and `initialize` said we cannot do it.
            // Refusing plainly is better than silence, which leaves the
            // adapter waiting.
            Message::Request { seq, command, .. } => {
                let refusal =
                    Message::Response { request_seq: *seq, command: command.clone(), result: Err(format!("bish does not implement {command}")) };
                self.write(&refusal);
            }
        }
    }

    /// Slots a revised breakpoint into whichever file it belongs to.
    /// Matched by id where the adapter gave one, since the line it
    /// reports may have moved from the line that was asked for.
    fn update_breakpoint(&mut self, updated: dap::Breakpoint) {
        for (path, placed) in self.placed.iter_mut() {
            if let Some(existing) = placed.iter_mut().find(|b| b.id.is_some() && b.id == updated.id) {
                *existing = updated;
                return;
            }
            // No id to match on, so fall back to the file it names.
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
        // The reply says nothing about which file it is for, so the
        // path rides along under the negated seq -- one map, and no way
        // for the two halves to drift apart.
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

    /// Pushes as much of the queue as the pipe will take. What is left
    /// stays queued for the next tick, which is what makes a full pipe
    /// a delay rather than a block.
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

    /// An adapter's stderr is where it explains itself when it cannot
    /// start. Read every tick whether anyone is looking or not, because
    /// a pipe nobody empties eventually stops the writer.
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
    /// A debug adapter left running holds the debuggee with it -- a
    /// stopped process nobody can reach, and on some systems a
    /// terminal that never comes back. So the child is killed rather
    /// than orphaned.
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

    /// A debug session needs three things this machine may not have: a
    /// C compiler, a debugger that speaks DAP, and permission to
    /// ptrace. Every test here skips rather than fails without them --
    /// the same courtesy the bash corpus extends to a machine with no
    /// bash.
    ///
    /// gdb learned `-i=dap` in version 14. Older ones fail the
    /// handshake, which this notices as a session that never leaves
    /// `Initializing`.
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

    /// A small program with a function worth stepping into and a local
    /// worth reading, compiled with debug info and no optimisation --
    /// an optimised build has no `total` to look at, which would make
    /// the test about the compiler rather than about this client.
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

    /// Polls until `done` says so, or gives up. Every wait in a real
    /// debug session is like this: the adapter answers when it answers,
    /// and how quickly is a fact about the machine.
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

    /// The whole point of building this against gdb rather than a mock:
    /// a real adapter, a real program, a real breakpoint. Everything
    /// the handshake gets wrong shows up here as a session that never
    /// stops.
    #[test]
    fn a_real_adapter_runs_to_a_real_breakpoint() {
        let (Some(command), Some(fx)) = (adapter(), fixture("breakpoint")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).expect("gdb starts");
        // Line 2 is `add`'s body, reached only by being called.
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

        // The breakpoint that came back unverified from the reply has
        // been upgraded by the event -- which is the only way it ever
        // becomes verified, and the thing a client reading only the
        // reply would get wrong for ever.
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
        // Stopped inside `add`, called from `main` -- the call stack is
        // the thing a debugger exists to show.
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
        // First call: add(0, 1).
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

        // A refusal is an answer too, and it carries the debugger's own
        // words rather than a shrug.
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

        // `add` is called three times -- add(0,1), add(1,2), add(3,3)
        // -- so the second argument counts up as the loop goes round,
        // and the third resume runs off the end.
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
        // And the program's own output came back through the adapter,
        // kept apart from gdb's own chatter.
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

    // The capabilities really were negotiated, over a pipe, with a
    // process that had its own opinion about what it can do.
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

    // A breakpoint on a line with no code is not a breakpoint, and an
    // adapter that says so is telling the truth the user needs. gdb
    // moves this one to the next statement rather than refusing it,
    // which is worth knowing rather than assuming either way.
    #[test]
    fn a_breakpoint_the_adapter_moves_reports_where_it_landed() {
        let (Some(command), Some(fx)) = (adapter(), fixture("moved")) else { return };
        let mut session =
            Session::start(&command, &fx.dir, object(vec![("program", Value::Str(fx.program.to_string_lossy().into_owned()))])).unwrap();
        // Line 3 is `int main(void) {`, whose code is really line 4.
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
