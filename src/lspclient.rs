// One running language server: the process, its pipes, and the
// handshake that has to complete before it will answer anything.
//
// The other half of `lsp.rs`, which is the wire format and nothing else.
// This module knows about processes and file descriptors; it still knows
// nothing about the editor. It touches no `Shell`, no `SessionState`, no
// `Rc<RefCell<_>>` -- which is what makes it safe to drive from
// `service_background_jobs`, the single-threaded idle callback every
// blocking loop in repl.rs already calls.
//
// **No threads.** The detachable-sessions work needed one for its pty
// bridge, but only because a blocking write to a buffer nobody was
// draining could deadlock, and the daemon had no choice about writing.
// Here bish owns the write side: both pipes are non-blocking, outgoing
// bytes sit in a queue that drains a bit at a time, and a busy server
// that has stopped reading its stdin costs a queue that grows rather
// than a loop that stops. That keeps this out of the `Arc<Mutex<_>>`
// refactor a threaded design would force on everything it touched.
//
// Not yet here, and deliberately: nothing about documents. This module
// can start a server, agree on a protocol version and a position
// encoding, and be told to stop. Telling it about a *file* is the next
// piece of work.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};

use crate::json::{self, Value};
use crate::lsp::{self, Id, Message, PositionEncoding, ResponseError};
use crate::url;

// Bounded, like `drive_fg_job`'s own read loop and for the same reason:
// a server producing output faster than we consume it must not be able
// to hold this tick forever, because every other pane's redraw is
// waiting behind it.
const MAX_READS_PER_TICK: u32 = 16;

// How much of a server's stderr to keep. Servers log freely there, and
// none of it is worth unbounded memory -- but the last screenful is
// often the only explanation of why one failed to start, which is
// exactly the moment a user needs it.
const LOG_LINES: usize = 200;

/// Where a server is in its lifecycle.
///
/// `Initializing` is a real state rather than an implementation detail:
/// the protocol says a server may reject anything but `initialize`
/// before the handshake completes, so requests made during it are held
/// rather than sent and lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Initializing,
    Ready,
    /// With the reason, which is what `::bish lsp status` shows. A dead
    /// server stays dead: nothing here restarts one on a timer, because
    /// a server that fails at startup fails the same way every time and
    /// a retry loop would just be a slower way to log the same error
    /// repeatedly.
    Dead(String),
}

impl State {
    pub fn describe(&self) -> String {
        match self {
            State::Initializing => "initializing".to_string(),
            State::Ready => "ready".to_string(),
            State::Dead(why) => format!("dead: {why}"),
        }
    }
}

/// What a server said about how it wants documents synchronized, from
/// its `textDocumentSync` capability.
///
/// Worth honouring rather than assuming: a server that says
/// `openClose: false` is telling us it does not track documents at all,
/// and sending it a `didOpen` for every file the user visits is pure
/// noise on a pipe that has real work to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    pub open_close: bool,
    pub change: bool,
    pub save: bool,
}

impl Default for Sync {
    // What to assume when the server said nothing at all. The spec's
    // own default is `None` -- no sync whatsoever -- which would mean a
    // server that simply forgot to describe itself gets told nothing and
    // appears broken in a way that is very hard to see from the outside.
    // Assuming full sync instead fails the other way: a server that
    // really wanted nothing receives notifications it can ignore.
    fn default() -> Sync {
        Sync { open_close: true, change: true, save: false }
    }
}

impl Sync {
    /// `textDocumentSync` is either a bare kind number or an options
    /// object, and both forms are common.
    fn parse(capabilities: &Value) -> Sync {
        match json::query(capabilities, ".textDocumentSync") {
            // The number form names only the change kind; a server using
            // it still expects open/close (there would be nothing for a
            // change to be relative to otherwise).
            Ok(Value::Number(kind)) => Sync { open_close: true, change: *kind > 0.0, save: false },
            Ok(value @ Value::Object(_)) => Sync {
                // The object form's fields really do all default to
                // false, per the spec -- a server choosing this form is
                // describing itself exactly.
                open_close: matches!(json::query(value, ".openClose"), Ok(Value::Bool(true))),
                change: matches!(json::query(value, ".change"), Ok(Value::Number(k)) if *k > 0.0),
                // Either `true` or `{ includeText: bool }`; both mean
                // "tell me about saves".
                save: matches!(json::query(value, ".save"), Ok(Value::Bool(true)) | Ok(Value::Object(_))),
            },
            _ => Sync::default(),
        }
    }
}

// One document this server has been told about.
struct Document {
    uri: String,
    // The buffer revision last actually sent. Not the buffer's current
    // one -- the gap between them is precisely what `needs_change` is
    // asking about.
    version: u64,
    // When this document first differed from what the server has been
    // told, or `None` when it doesn't. The debounce is measured from
    // here rather than from the most recent keystroke, so a burst of
    // typing produces one `didChange` at a bounded delay instead of
    // never producing one at all while the typing continues.
    pending_since: Option<Instant>,
}

pub struct Server {
    /// The `exec::LspServer` declaration this was started from.
    pub id: u64,
    /// For display: the command line, as registered.
    pub command: String,
    /// The project root it was started for. Together with `command`
    /// this identifies the server -- one per (command, root), shared by
    /// every pane editing that project.
    pub root: PathBuf,

    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,

    decoder: lsp::Decoder,
    // Bytes owed to the server's stdin. A non-blocking pipe accepts what
    // it has room for and no more, so this drains across as many ticks
    // as it takes rather than blocking until it all fits.
    outgoing: VecDeque<u8>,
    // Messages that must not be sent until the handshake finishes.
    queued: Vec<Message>,

    log: VecDeque<String>,
    stderr_partial: String,

    state: State,
    // Set when stdout hits EOF, which for a server that just exited
    // happens a moment *before* the exit is reapable. Counted rather
    // than acted on at once, so `check_alive` gets the chance to
    // report the real exit status -- "exited with status 3 (cannot
    // find configuration)" is a diagnosis, "closed its stdout" is a
    // symptom.
    stdout_eof: bool,
    eof_ticks: u32,
    next_id: i64,
    initialize_id: i64,
    encoding: PositionEncoding,
    capabilities: Value,
    sync: Sync,
    documents: Vec<Document>,
}

impl Server {
    /// Spawns `command` in `root` and sends `initialize`. Returns as
    /// soon as the process exists -- the handshake completes later, in
    /// `service`, which is why the returned server starts out
    /// `Initializing` rather than usable.
    pub fn start(id: u64, command: &[String], display: &str, root: &Path) -> Result<Server, String> {
        let Some((program, args)) = command.split_first() else {
            return Err("no command to run".to_string());
        };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, not null: a server that fails to start explains
            // itself on stderr and then exits, and discarding that means
            // the user sees a dead server with no reason. Piped-and-
            // never-read would be worse still -- a chatty server fills
            // the pipe and blocks -- so `drain_stderr` runs every tick
            // whether anyone is looking or not.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;
        // Every pipe non-blocking, including stdin: see this module's
        // own header for why that is what replaces a writer thread.
        crate::pty::set_nonblocking(stdout.as_raw_fd());
        crate::pty::set_nonblocking(stderr.as_raw_fd());
        crate::pty::set_nonblocking(stdin.as_raw_fd());

        let mut server = Server {
            id,
            command: display.to_string(),
            root: root.to_path_buf(),
            child,
            stdin,
            stdout,
            stderr,
            decoder: lsp::Decoder::new(),
            outgoing: VecDeque::new(),
            queued: Vec::new(),
            log: VecDeque::new(),
            stderr_partial: String::new(),
            state: State::Initializing,
            stdout_eof: false,
            eof_ticks: 0,
            next_id: 1,
            initialize_id: 0,
            // What the protocol says to assume when nothing has been
            // negotiated yet. Replaced by whatever `initialize` agrees
            // on, which is `utf-32` for any server new enough to offer
            // it.
            encoding: PositionEncoding::Utf16,
            capabilities: Value::Null,
            sync: Sync::default(),
            documents: Vec::new(),
        };
        let params = server.initialize_params();
        server.initialize_id = server.next_id;
        server.next_id += 1;
        let message = Message::Request { id: Id::Number(server.initialize_id), method: "initialize".to_string(), params };
        server.enqueue(&message);
        Ok(server)
    }

    fn initialize_params(&self) -> Value {
        let uri = url::from_file_path(&self.root);
        let encodings = PositionEncoding::PREFERRED.iter().map(|e| Value::Str(e.wire_name().to_string())).collect();
        Value::Object(vec![
            ("processId".to_string(), Value::Number(std::process::id() as f64)),
            (
                "clientInfo".to_string(),
                Value::Object(vec![("name".to_string(), Value::Str("bish".to_string())), ("version".to_string(), Value::Str(env!("CARGO_PKG_VERSION").to_string()))]),
            ),
            // `rootUri` is deprecated in favour of `workspaceFolders`,
            // but it is the one every server still honours, and bish has
            // no multi-root story to describe yet -- one root per server
            // is the whole model (see `exec::LspServer`).
            ("rootUri".to_string(), Value::Str(uri.clone())),
            ("rootPath".to_string(), Value::Str(self.root.to_string_lossy().into_owned())),
            (
                "capabilities".to_string(),
                Value::Object(vec![
                    // The one thing actually claimed so far, and the
                    // reason to claim it: `utf-32` is bish's own column
                    // counting exactly, so agreeing on it means no
                    // conversion and no chance of being off by one on
                    // any line containing an emoji.
                    ("general".to_string(), Value::Object(vec![("positionEncodings".to_string(), Value::Array(encodings))])),
                    (
                        "textDocument".to_string(),
                        Value::Object(vec![(
                            "synchronization".to_string(),
                            Value::Object(vec![
                                // No dynamic registration anywhere: a
                                // server that could ask to be told about
                                // things at run time would be asking a
                                // client that has no way to honour it.
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // `willSave` is declined deliberately.
                                // It only earns its place alongside
                                // `willSaveWaitUntil`, which lets a
                                // server rewrite the buffer before it
                                // hits disk -- and a save that blocks on
                                // a language server is a save that can
                                // hang, which `:w` must not.
                                ("willSave".to_string(), Value::Bool(false)),
                                ("willSaveWaitUntil".to_string(), Value::Bool(false)),
                                ("didSave".to_string(), Value::Bool(true)),
                            ]),
                        )]),
                    ),
                    ("workspace".to_string(), Value::Object(Vec::new())),
                ]),
            ),
        ])
    }

    // -----------------------------------------------------------------
    // The tick
    // -----------------------------------------------------------------

    /// Moves whatever bytes are ready in either direction, and returns
    /// the messages the caller has to deal with -- everything except the
    /// handshake, which is handled here.
    ///
    /// Non-blocking throughout. Called once per idle tick, from the same
    /// place every job pty is drained.
    pub fn service(&mut self) -> Vec<Message> {
        if matches!(self.state, State::Dead(_)) {
            return Vec::new();
        }
        // Reads first, then the liveness check: a server that just died
        // may have said why on the way out, and checking first would
        // throw that away.
        self.drain_stderr();
        self.drain_stdout();
        self.check_alive();
        // A server that closed stdout without exiting will never answer
        // anything again, so it is dead either way -- but only once the
        // exit it was probably about has had a tick to arrive.
        if self.stdout_eof && !matches!(self.state, State::Dead(_)) {
            self.eof_ticks += 1;
            if self.eof_ticks > 1 {
                self.die("server closed its stdout".to_string());
            }
        }
        self.flush_outgoing();

        let mut incoming = Vec::new();
        loop {
            match self.decoder.take_message() {
                None => break,
                Some(Err(why)) => {
                    self.note(format!("protocol error: {why}"));
                    // A body-level problem leaves the stream
                    // synchronized, so it is logged and skipped; a
                    // framing one has cost us our place in the byte
                    // stream, and there is nothing to do but give up.
                    if self.decoder.is_failed() {
                        self.die(why);
                        return incoming;
                    }
                }
                Some(Ok(message)) => {
                    if let Some(message) = self.handle(message) {
                        incoming.push(message);
                    }
                }
            }
        }
        // Again at the end: handling the `initialize` response releases
        // `initialized` and everything queued behind it, and waiting a
        // whole tick to send those would add latency for nothing.
        self.flush_outgoing();
        incoming
    }

    // Returns the message if the caller should see it. `None` for the
    // parts of the conversation that are this module's own business.
    fn handle(&mut self, message: Message) -> Option<Message> {
        match &message {
            Message::Response { id: Id::Number(id), result } if *id == self.initialize_id && self.state == State::Initializing => {
                match result {
                    Err(e) => self.die(format!("initialize failed: {} ({})", e.message, e.code)),
                    Ok(result) => {
                        self.capabilities = json::query(result, ".capabilities").cloned().unwrap_or(Value::Null);
                        self.encoding = match json::query(&self.capabilities, ".positionEncoding") {
                            Ok(Value::Str(name)) => PositionEncoding::from_wire_name(name).unwrap_or(PositionEncoding::Utf16),
                            // Silence means UTF-16, per the spec -- a
                            // server too old to know the negotiation
                            // exists is exactly the case this default
                            // is for.
                            _ => PositionEncoding::Utf16,
                        };
                        self.sync = Sync::parse(&self.capabilities);
                        self.state = State::Ready;
                        self.send_now(Message::Notification { method: "initialized".to_string(), params: Value::Object(Vec::new()) });
                        for message in std::mem::take(&mut self.queued) {
                            self.send_now(message);
                        }
                        self.note(format!("ready, position encoding {}", self.encoding.wire_name()));
                    }
                }
                None
            }
            // A server's own log lines belong with the ones it wrote to
            // stderr, not in the caller's lap.
            Message::Notification { method, params } if method == "window/logMessage" || method == "window/showMessage" => {
                if let Ok(Value::Str(text)) = json::query(params, ".message") {
                    self.note(format!("{method}: {text}"));
                }
                None
            }
            _ => Some(message),
        }
    }

    fn drain_stdout(&mut self) {
        let mut buf = [0u8; 8192];
        for _ in 0..MAX_READS_PER_TICK {
            match self.stdout.read(&mut buf) {
                Ok(0) => {
                    self.stdout_eof = true;
                    return;
                }
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.die(format!("reading stdout: {e}"));
                    return;
                }
            }
        }
    }

    fn drain_stderr(&mut self) {
        let mut buf = [0u8; 8192];
        for _ in 0..MAX_READS_PER_TICK {
            match self.stderr.read(&mut buf) {
                // Nothing to report: a server closing stderr while still
                // speaking on stdout is unusual but perfectly legal, and
                // it is not this half's job to declare it dead.
                Ok(0) => return,
                Ok(n) => {
                    self.stderr_partial.push_str(&String::from_utf8_lossy(&buf[..n]));
                    while let Some(newline) = self.stderr_partial.find('\n') {
                        let line: String = self.stderr_partial.drain(..=newline).collect();
                        self.note(line.trim_end().to_string());
                    }
                    // A server that logs one enormous line without ever
                    // ending it must not be able to grow this without
                    // bound.
                    if self.stderr_partial.len() > 64 * 1024 {
                        self.stderr_partial.clear();
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }

    fn flush_outgoing(&mut self) {
        while !self.outgoing.is_empty() {
            let (front, _) = self.outgoing.as_slices();
            let written = match self.stdin.write(front) {
                Ok(0) => return,
                Ok(n) => n,
                // The pipe is full because the server is busy. The bytes
                // stay queued and go out on a later tick -- this is the
                // case a writer thread would otherwise exist to handle.
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.die(format!("writing stdin: {e}"));
                    return;
                }
            };
            self.outgoing.drain(..written);
        }
    }

    fn check_alive(&mut self) {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let why = match status.code() {
                    Some(code) => format!("exited with status {code}"),
                    None => "killed by a signal".to_string(),
                };
                // The last thing it said is almost always the answer to
                // "why", so it goes in the reason rather than only in a
                // log the user has to think to look at.
                let why = match self.log.back() {
                    Some(last) if !last.is_empty() => format!("{why} ({last})"),
                    _ => why,
                };
                self.die(why);
            }
            Ok(None) => {}
            Err(e) => self.die(format!("cannot check on the process: {e}")),
        }
    }

    // -----------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------

    /// Fire-and-forget. Held until the handshake finishes if it hasn't.
    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(Message::Notification { method: method.to_string(), params });
    }

    /// Sends a request and returns the id its response will carry.
    pub fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(Message::Request { id: Id::Number(id), method: method.to_string(), params });
        id
    }

    /// Answers a request the *server* made. The id is echoed back
    /// exactly as it arrived, which is why `Id` keeps its string case.
    pub fn respond(&mut self, id: Id, result: Result<Value, ResponseError>) {
        self.send(Message::Response { id, result });
    }

    fn send(&mut self, message: Message) {
        match self.state {
            State::Dead(_) => {}
            State::Ready => self.send_now(message),
            // Sending this now would at best be ignored and at worst be
            // answered with an error, and either way the caller would
            // never learn its request went nowhere.
            State::Initializing => self.queued.push(message),
        }
    }

    fn send_now(&mut self, message: Message) {
        self.enqueue(&message);
    }

    fn enqueue(&mut self, message: &Message) {
        self.outgoing.extend(lsp::encode(message));
    }

    // -----------------------------------------------------------------
    // Documents
    // -----------------------------------------------------------------
    //
    // Full-document sync throughout: every change carries the whole
    // text. `buffer_text` already produces exactly that and is what
    // `:diag` has always used, whereas incremental sync would need the
    // buffer to *emit* its edits as ranges, which nothing in it does
    // today -- a real refactor of every mutating path for a performance
    // win nobody has measured. The declared capability says `Full`, and
    // the whole-document form (`[{ "text": ... }]`) is a legal change
    // event even for a server that asked for `Incremental`, so this
    // stays correct against either.

    /// Tells the server about a document it hasn't seen. A no-op if it
    /// already knows, or if it said it doesn't track documents.
    pub fn open_document(&mut self, uri: &str, language_id: &str, version: u64, text: &str) {
        if !self.sync.open_close || self.has_document(uri) {
            return;
        }
        // Recorded before the send, not after: `send` holds the
        // notification until the handshake finishes, and a second open
        // arriving in the meantime must not queue a duplicate.
        self.documents.push(Document { uri: uri.to_string(), version, pending_since: None });
        self.notify(
            "textDocument/didOpen",
            Value::Object(vec![(
                "textDocument".to_string(),
                Value::Object(vec![
                    ("uri".to_string(), Value::Str(uri.to_string())),
                    ("languageId".to_string(), Value::Str(language_id.to_string())),
                    ("version".to_string(), Value::Number(version as f64)),
                    ("text".to_string(), Value::Str(text.to_string())),
                ]),
            )]),
        );
    }

    /// Whether this document has changed since the server was last told,
    /// and has been settled long enough to be worth telling it.
    ///
    /// Asked on every idle tick, which is why it is separate from
    /// `change_document`: producing the text means walking the whole
    /// buffer, and doing that sixty times a second to discover nothing
    /// has changed would be the one genuinely wasteful thing in this
    /// path.
    pub fn needs_change(&mut self, uri: &str, version: u64, now: Instant, debounce: Duration) -> bool {
        if !self.sync.change {
            return false;
        }
        let Some(document) = self.documents.iter_mut().find(|d| d.uri == uri) else {
            return false;
        };
        if document.version == version {
            document.pending_since = None;
            return false;
        }
        let since = *document.pending_since.get_or_insert(now);
        now.duration_since(since) >= debounce
    }

    /// Sends the document's current text. A no-op when the server
    /// already has this version -- which is what makes it safe for
    /// `save_document` to flush unconditionally rather than having to
    /// ask first, and keeps a save that follows a settled edit from
    /// sending the same document twice.
    pub fn change_document(&mut self, uri: &str, version: u64, text: &str) {
        let Some(document) = self.documents.iter_mut().find(|d| d.uri == uri) else {
            return;
        };
        if document.version == version {
            document.pending_since = None;
            return;
        }
        document.version = version;
        document.pending_since = None;
        self.notify(
            "textDocument/didChange",
            Value::Object(vec![
                (
                    "textDocument".to_string(),
                    Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string())), ("version".to_string(), Value::Number(version as f64))]),
                ),
                ("contentChanges".to_string(), Value::Array(vec![Value::Object(vec![("text".to_string(), Value::Str(text.to_string()))])])),
            ]),
        );
    }

    pub fn save_document(&mut self, uri: &str, text: &str) {
        if !self.sync.save || !self.has_document(uri) {
            return;
        }
        // `text` is included unconditionally. A server that didn't ask
        // for it (`save: true` rather than `{ includeText: true }`) is
        // required to ignore the field, and sending it costs one copy of
        // a file someone just pressed `:w` on -- cheap, against the
        // alternative of a server that wanted it silently working from
        // whatever it last saw.
        self.notify(
            "textDocument/didSave",
            Value::Object(vec![
                ("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string()))])),
                ("text".to_string(), Value::Str(text.to_string())),
            ]),
        );
    }

    /// Tells the server the document is gone, and forgets it. After
    /// this the server's diagnostics for it are its own to withdraw --
    /// which is exactly why the editor sends this while the buffer still
    /// exists rather than after it is dropped.
    pub fn close_document(&mut self, uri: &str) {
        if !self.has_document(uri) {
            return;
        }
        self.documents.retain(|d| d.uri != uri);
        if !self.sync.open_close {
            return;
        }
        self.notify(
            "textDocument/didClose",
            Value::Object(vec![("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string()))]))]),
        );
    }

    // -----------------------------------------------------------------
    // Winding down
    // -----------------------------------------------------------------

    /// The protocol's own goodbye: `shutdown`, then `exit`. Best effort
    /// -- the bytes are queued and flushed once, and `Drop` is what
    /// actually guarantees the process is gone.
    pub fn shutdown(&mut self) {
        if matches!(self.state, State::Dead(_)) {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send_now(Message::Request { id: Id::Number(id), method: "shutdown".to_string(), params: Value::Null });
        self.send_now(Message::Notification { method: "exit".to_string(), params: Value::Null });
        self.flush_outgoing();
    }

    fn die(&mut self, why: String) {
        if matches!(self.state, State::Dead(_)) {
            return;
        }
        self.note(format!("dead: {why}"));
        self.state = State::Dead(why);
        self.outgoing.clear();
        self.queued.clear();
    }

    fn note(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    // -----------------------------------------------------------------
    // What the outside can ask
    // -----------------------------------------------------------------

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    /// How this server counts columns. Only meaningful once ready.
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// What the server said it can do, verbatim, for a caller to read
    /// with `json::query` -- there is no typed model of this on purpose
    /// (see lsp.rs's own header).
    pub fn capabilities(&self) -> &Value {
        &self.capabilities
    }

    pub fn open_documents(&self) -> usize {
        self.documents.len()
    }

    /// How this server wants documents synchronized.
    pub fn sync(&self) -> Sync {
        self.sync
    }

    pub fn has_document(&self, uri: &str) -> bool {
        self.documents.iter().any(|d| d.uri == uri)
    }

    /// The tail of what this server has written to stderr, plus this
    /// module's own notes about its lifecycle, oldest first.
    pub fn log(&self) -> impl Iterator<Item = &String> {
        self.log.iter()
    }
}

impl Drop for Server {
    // A language server outliving the editor that started it is a real
    // problem, not a tidiness one -- an orphaned rust-analyzer will
    // happily index a workspace forever. Dropping `stdin` closes it,
    // which is the signal most servers take as "we're done"; the kill is
    // what makes it certain, and the wait is what stops it becoming a
    // zombie.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every language server this bish has running.
///
/// Keyed by (command, root) rather than by buffer: one server per
/// project, shared by every pane editing it, which is what makes a
/// cross-file answer possible at all and is the only affordable
/// arrangement for anything that indexes a workspace.
///
/// Lives behind an `Rc<RefCell<_>>` on `Shell`, exactly as `jobs` does
/// and for the same reason -- it is one table the whole process shares,
/// and every session's shell is a `new_virtual_child` of the last, so
/// they all reach the same one. The alternative considered was a map
/// owned by `repl::run` beside `job_frames`, which would have meant
/// threading one more `&mut` parameter through eighteen call sites and
/// six already-oversized signatures for no gain; sharing it here also
/// means `::bish lsp status` reads the live table instead of a snapshot
/// that could be a tick stale.
#[derive(Default)]
pub struct Table {
    servers: Vec<Server>,
    failures: Vec<Failure>,
}

/// A server that could not be started at all -- no process, so nothing
/// for `Server` to represent, but exactly what someone running
/// `::bish lsp status` is trying to find out about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub id: u64,
    pub command: String,
    pub root: PathBuf,
    pub why: String,
}

impl Table {
    /// The server for this command and root, starting one if there
    /// isn't one yet.
    ///
    /// A start that failed is remembered and not retried. Opening files
    /// is a thing people do dozens of times a minute, and a command
    /// that isn't on `$PATH` will not be on `$PATH` the next time
    /// either -- retrying would turn one typo into a spawn attempt per
    /// keystroke's worth of navigation. A server that started and then
    /// died is likewise left dead, for the same reason.
    pub fn get_or_start(&mut self, id: u64, command: &[String], display: &str, root: &Path) -> Result<&mut Server, String> {
        if let Some(index) = self.servers.iter().position(|s| s.command == display && s.root == root) {
            return Ok(&mut self.servers[index]);
        }
        if let Some(failure) = self.failures.iter().find(|f| f.command == display && f.root == root) {
            return Err(failure.why.clone());
        }
        match Server::start(id, command, display, root) {
            Ok(server) => {
                self.servers.push(server);
                Ok(self.servers.last_mut().expect("just pushed"))
            }
            Err(why) => {
                self.failures.push(Failure { id, command: display.to_string(), root: root.to_path_buf(), why: why.clone() });
                Err(why)
            }
        }
    }

    /// Servers that never started. Reported alongside the running ones.
    pub fn failures(&self) -> &[Failure] {
        &self.failures
    }

    /// One tick for every server. Called from the idle callback every
    /// blocking loop in repl.rs shares.
    pub fn service_all(&mut self) {
        for server in &mut self.servers {
            for message in server.service() {
                match message {
                    // Nothing consumes server-initiated messages yet.
                    // A *request* still has to be answered, though: a
                    // server that asked something and never heard back
                    // waits forever, and "not implemented" is both true
                    // and the answer that unblocks it.
                    Message::Request { id, method, .. } => {
                        server.note(format!("declined {method}: not implemented"));
                        server.respond(id, Err(ResponseError { code: -32601, message: format!("bish does not implement {method}") }));
                    }
                    Message::Notification { .. } | Message::Response { .. } => {}
                }
            }
        }
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// The already-running server for this command and root, if there is
    /// one. Unlike `get_or_start`, this never spawns -- which is what
    /// makes it the right thing for the document events, none of which
    /// should bring a server into existence: only opening a file does
    /// that.
    pub fn running(&mut self, display: &str, root: &Path) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| s.command == display && s.root == root)
    }

    /// The protocol's goodbye to every server, for a clean bish exit.
    /// `Server`'s own `Drop` is what actually guarantees they are gone.
    pub fn shutdown_all(&mut self) {
        for server in &mut self.servers {
            server.shutdown();
        }
        self.servers.clear();
        // Forgotten too: whatever made a start fail may well have been
        // fixed by the time anything asks again.
        self.failures.clear();
    }
}

/// The directory to treat as the project root for `path`: the nearest
/// ancestor containing one of `markers`, tried marker by marker.
///
/// Marker order beats proximity, which is what `--root=Cargo.toml,.git`
/// is asking for -- a crate inside a git repository should be rooted at
/// its `Cargo.toml`, not at the repository, even though the repository
/// root may be closer to nothing in particular. Within one marker,
/// nearest wins.
///
/// `None` when nothing matches, which is the caller's cue not to start a
/// server at all rather than to guess: a server given the wrong root
/// indexes the wrong tree, and for a large one that is expensive enough
/// to notice.
pub fn root_for(path: &Path, markers: &[String]) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    for marker in markers {
        let mut dir = Some(start);
        while let Some(current) = dir {
            if current.join(marker).exists() {
                return Some(current.to_path_buf());
            }
            dir = current.parent();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-lspclient-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_table_starts_one_server_per_command_and_root_and_reuses_it() {
        let dir = temp_dir("table");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let command = vec!["cat".to_string()];
        let mut table = Table::default();

        let first = table.get_or_start(1, &command, "cat", &dir).map(|s| s.root.clone()).unwrap();
        assert_eq!(table.servers().len(), 1);
        // Same command, same root: the existing one, which is the whole
        // point -- every pane editing a project shares its server.
        table.get_or_start(1, &command, "cat", &dir).unwrap();
        assert_eq!(table.servers().len(), 1);
        // Same command, different root: a second server, because a
        // server is scoped to the project it was told about.
        table.get_or_start(1, &command, "cat", &other).unwrap();
        assert_eq!(table.servers().len(), 2);
        assert_eq!(first, dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Opening files is something people do constantly, so a command
    // that isn't there must cost one failed spawn, not one per open.
    #[test]
    fn a_failed_start_is_remembered_rather_than_retried() {
        let dir = temp_dir("failed");
        let command = vec!["bish-no-such-language-server".to_string()];
        let mut table = Table::default();
        for _ in 0..5 {
            assert!(table.get_or_start(7, &command, "nope", &dir).is_err());
        }
        assert!(table.servers().is_empty());
        assert_eq!(table.failures().len(), 1, "one record, however many times it was asked for");
        assert_eq!(table.failures()[0].id, 7);
        assert!(table.failures()[0].why.contains("bish-no-such-language-server"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_root_is_the_nearest_ancestor_holding_a_marker() {
        let dir = temp_dir("root");
        let repo = dir.join("repo");
        let crate_dir = repo.join("crates").join("inner");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "").unwrap();
        let file = crate_dir.join("src.rs");
        std::fs::write(&file, "").unwrap();

        // Marker order beats proximity in the sense that matters: with
        // Cargo.toml listed first, the crate wins over the repository
        // even though both are ancestors.
        assert_eq!(root_for(&file, &["Cargo.toml".to_string(), ".git".to_string()]), Some(crate_dir.clone()));
        // Listed the other way round, the repository wins.
        assert_eq!(root_for(&file, &[".git".to_string(), "Cargo.toml".to_string()]), Some(repo.clone()));
        // And with only the marker that isn't there, nothing does --
        // rather than falling back to a guess.
        assert_eq!(root_for(&file, &["go.mod".to_string()]), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Everything below drives a real child process. `cat` is the
    // simplest server-shaped thing on any system: it reads stdin and
    // writes stdout, so it exercises spawning, non-blocking pipes and
    // the queue without needing a language server installed.

    #[test]
    fn starting_a_server_sends_initialize_and_leaves_it_initializing() {
        let dir = temp_dir("start");
        let mut server = Server::start(1, &["cat".to_string()], "cat", &dir).unwrap();
        assert_eq!(*server.state(), State::Initializing);
        assert!(!server.is_ready());

        // `cat` echoes, so what comes back is exactly what was sent --
        // which means the framing that went out is decodable, end to
        // end, through a real pipe.
        let mut echoed = Vec::new();
        for _ in 0..200 {
            echoed.extend(server.service());
            if !echoed.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let Some(Message::Request { method, params, .. }) = echoed.first() else {
            panic!("expected the echoed initialize, got {echoed:?}");
        };
        assert_eq!(method, "initialize");
        assert_eq!(json::query(params, ".clientInfo.name"), Ok(&Value::Str("bish".to_string())));
        // The negotiation the whole position-encoding story rests on:
        // utf-32 offered first, because it is bish's own counting.
        let Ok(Value::Array(encodings)) = json::query(params, ".capabilities.general.positionEncodings") else {
            panic!("no positionEncodings in {params:?}");
        };
        assert_eq!(encodings[0], Value::Str("utf-32".to_string()));
        // The root is a real percent-encoded file URI, not a bare path.
        let Ok(Value::Str(uri)) = json::query(params, ".rootUri") else { panic!("no rootUri") };
        assert!(uri.starts_with("file:///"), "{uri}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_request_made_before_the_handshake_finishes_is_held_not_sent() {
        let dir = temp_dir("queue");
        let mut server = Server::start(1, &["cat".to_string()], "cat", &dir).unwrap();
        server.request("textDocument/hover", Value::Null);
        server.notify("textDocument/didOpen", Value::Null);
        assert_eq!(server.queued.len(), 2, "both should be waiting for `initialized`");

        // Only `initialize` is ever on the wire before the handshake
        // completes -- anything else would be answered with an error the
        // caller could not learn about.
        let mut seen = Vec::new();
        for _ in 0..200 {
            seen.extend(server.service());
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(seen.len(), 1, "{seen:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // A mock server that answers `initialize` and then stays alive.
    //
    // It doesn't parse anything: bish's `initialize` is always the first
    // request it sends, so its id is always 1, and a canned reply to
    // id 1 is a complete and honest handshake from the client's side.
    // A responder that reads its input properly is what `bish tool
    // lsp-mock` will be, once there is a conversation to script rather
    // than a single reply.
    fn mock_server(result: &str) -> Vec<String> {
        mock_server_script(result, "sleep 30")
    }

    // The same mock, but echoing its stdin back instead of idling --
    // which turns it into an observer of everything bish sends.
    //
    // Everything written to the server comes straight back and is
    // decoded by `lsp::Decoder`, the same decoder unit-tested on its own
    // above, so a test can assert on the exact messages, their order and
    // their versions without a log file, a temp path, or any worry about
    // when a pipe flushes. Document sync is all notifications, so
    // nothing here can start a reply loop.
    //
    // This stops being enough at the point where replies have to vary by
    // method (hover, definition), which is when a real scripted fixture
    // in `src/testdata/` earns its place.
    fn echo_server(result: &str) -> Vec<String> {
        mock_server_script(result, "exec cat")
    }

    fn mock_server_script(result: &str, then: &str) -> Vec<String> {
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#);
        let script = format!("printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{body}'; {then}", body.len());
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    // Services until `want` `textDocument/*` notifications have come
    // back, or time runs out. Anything else (the echoed `initialize`
    // request, the echoed `initialized`) is not what these tests are
    // about.
    fn document_notifications(server: &mut Server, want: usize) -> Vec<(String, Value)> {
        let mut seen = Vec::new();
        for _ in 0..400 {
            for message in server.service() {
                if let Message::Notification { method, params } = message
                    && method.starts_with("textDocument/")
                {
                    seen.push((method, params));
                }
            }
            if seen.len() >= want {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        seen
    }

    const FULL_SYNC: &str = r#"{"capabilities":{"positionEncoding":"utf-32","textDocumentSync":{"openClose":true,"change":1,"save":true}}}"#;

    fn ready_echo_server(dir: &Path) -> Server {
        let mut server = Server::start(1, &echo_server(FULL_SYNC), "mock", dir).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        server
    }

    #[test]
    fn opening_a_document_tells_the_server_its_language_version_and_text() {
        let dir = temp_dir("didopen");
        let mut server = ready_echo_server(&dir);
        assert_eq!(server.sync(), Sync { open_close: true, change: true, save: true });

        server.open_document("file:///p/x.sh", "shellscript", 3, "echo hi\n");
        let seen = document_notifications(&mut server, 1);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (method, params) = &seen[0];
        assert_eq!(method, "textDocument/didOpen");
        assert_eq!(json::query(params, ".textDocument.uri"), Ok(&Value::Str("file:///p/x.sh".to_string())));
        assert_eq!(json::query(params, ".textDocument.languageId"), Ok(&Value::Str("shellscript".to_string())));
        assert_eq!(json::query(params, ".textDocument.version"), Ok(&Value::Number(3.0)));
        assert_eq!(json::query(params, ".textDocument.text"), Ok(&Value::Str("echo hi\n".to_string())));
        assert_eq!(server.open_documents(), 1);

        // Opening the same document again says nothing: the server
        // already has it, and a second didOpen for one uri is a protocol
        // error, not a refresh.
        server.open_document("file:///p/x.sh", "shellscript", 4, "echo bye\n");
        assert_eq!(server.open_documents(), 1);
        assert!(document_notifications(&mut server, 2).len() < 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_change_waits_for_the_buffer_to_settle_and_then_sends_the_whole_document() {
        let dir = temp_dir("didchange");
        let mut server = ready_echo_server(&dir);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        document_notifications(&mut server, 1);

        let debounce = Duration::from_millis(150);
        let start = Instant::now();
        // Unchanged: nothing to say, and no clock started.
        assert!(!server.needs_change("file:///p/x.sh", 1, start, debounce));
        // Changed, but only just -- this call is what starts the clock.
        assert!(!server.needs_change("file:///p/x.sh", 2, start, debounce));
        // Still typing: measured from the *first* unsent change, not the
        // most recent one, so a burst still produces an update.
        assert!(!server.needs_change("file:///p/x.sh", 3, start + Duration::from_millis(100), debounce));
        assert!(server.needs_change("file:///p/x.sh", 3, start + debounce, debounce));

        server.change_document("file:///p/x.sh", 3, "one two three");
        let seen = document_notifications(&mut server, 1);
        let (method, params) = seen.last().expect("a didChange");
        assert_eq!(method, "textDocument/didChange");
        assert_eq!(json::query(params, ".textDocument.version"), Ok(&Value::Number(3.0)));
        // Full sync: one change event carrying the entire document, with
        // no range -- which is a legal change event for an incremental
        // server too.
        assert_eq!(json::query(params, ".contentChanges[0].text"), Ok(&Value::Str("one two three".to_string())));
        assert_eq!(json::query(params, ".contentChanges[0].range"), Ok(&Value::Null));

        // Sent: the clock resets, so an idle editor stops asking.
        assert!(!server.needs_change("file:///p/x.sh", 3, start + Duration::from_secs(10), debounce));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A save that reported a version the server had never been told
    // about would be describing a file it cannot reconstruct.
    #[test]
    fn saving_flushes_a_pending_change_before_announcing_the_save() {
        let dir = temp_dir("didsave");
        let mut server = ready_echo_server(&dir);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        server.change_document("file:///p/x.sh", 2, "one two");
        // The flush a save does is unconditional, so this second call
        // must not put the same document on the wire twice.
        server.change_document("file:///p/x.sh", 2, "one two");
        server.save_document("file:///p/x.sh", "one two");
        let seen = document_notifications(&mut server, 3);
        let methods: Vec<&str> = seen.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(methods, vec!["textDocument/didOpen", "textDocument/didChange", "textDocument/didSave"], "{methods:?}");
        assert_eq!(json::query(&seen[2].1, ".text"), Ok(&Value::Str("one two".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn closing_a_document_tells_the_server_and_forgets_it() {
        let dir = temp_dir("didclose");
        let mut server = ready_echo_server(&dir);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        server.close_document("file:///p/x.sh");
        assert_eq!(server.open_documents(), 0);
        assert!(!server.has_document("file:///p/x.sh"));
        let methods: Vec<String> = document_notifications(&mut server, 2).into_iter().map(|(m, _)| m).collect();
        assert_eq!(methods, vec!["textDocument/didOpen".to_string(), "textDocument/didClose".to_string()]);

        // Closing what isn't open says nothing at all.
        server.close_document("file:///p/other.sh");
        assert!(document_notifications(&mut server, 3).len() < 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server describing itself with the options object means exactly
    // what it says, and sending it notifications it declined is noise on
    // a pipe with real work to carry.
    #[test]
    fn a_server_that_wants_no_documents_is_told_about_none() {
        let dir = temp_dir("nosync");
        let capabilities = r#"{"capabilities":{"textDocumentSync":{"openClose":false,"change":0,"save":false}}}"#;
        let mut server = Server::start(1, &echo_server(capabilities), "mock", &dir).unwrap();
        run_until_ready(&mut server);
        assert_eq!(server.sync(), Sync { open_close: false, change: false, save: false });

        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        assert_eq!(server.open_documents(), 0);
        assert!(!server.needs_change("file:///p/x.sh", 2, Instant::now(), Duration::ZERO));
        server.save_document("file:///p/x.sh", "one");
        assert!(document_notifications(&mut server, 1).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_sync_capability_is_read_from_either_form_a_server_may_use() {
        // The bare-number form names only the change kind; a server
        // using it still expects open/close.
        let number = json::parse(r#"{"textDocumentSync":2}"#).unwrap();
        assert_eq!(Sync::parse(&number), Sync { open_close: true, change: true, save: false });
        let none = json::parse(r#"{"textDocumentSync":0}"#).unwrap();
        assert_eq!(Sync::parse(&none), Sync { open_close: true, change: false, save: false });
        // `save` may be a bool or an options object; both mean yes.
        let object = json::parse(r#"{"textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":true}}}"#).unwrap();
        assert_eq!(Sync::parse(&object), Sync { open_close: true, change: true, save: true });
        // Said nothing at all: assume full sync rather than the spec's
        // own `None` default, so a server that merely forgot to describe
        // itself still works. See `Sync::default`.
        assert_eq!(Sync::parse(&Value::Object(Vec::new())), Sync::default());
        assert!(Sync::default().open_close && Sync::default().change);
    }



    fn run_until_ready(server: &mut Server) {
        for _ in 0..400 {
            server.service();
            if server.is_ready() || matches!(server.state(), State::Dead(_)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn the_handshake_completes_and_agrees_on_the_encoding_the_server_named() {
        let dir = temp_dir("ready");
        let command = mock_server(r#"{"capabilities":{"positionEncoding":"utf-32","hoverProvider":true}}"#);
        let mut server = Server::start(1, &command, "mock", &dir).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        // utf-32 is bish's own column counting, so agreeing on it is the
        // difference between exact and approximated positions.
        assert_eq!(server.encoding(), PositionEncoding::Utf32);
        // Capabilities are kept verbatim for a caller to read with
        // `json::query` -- there is no typed model of them on purpose.
        assert_eq!(json::query(server.capabilities(), ".hoverProvider"), Ok(&Value::Bool(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Every server too old to know the negotiation exists lands here, so
    // it is the case that has to be right by default rather than by
    // configuration.
    #[test]
    fn a_server_that_names_no_encoding_is_assumed_to_mean_utf16() {
        let dir = temp_dir("utf16");
        let mut server = Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        assert_eq!(server.encoding(), PositionEncoding::Utf16);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn work_queued_during_the_handshake_goes_out_once_it_finishes() {
        let dir = temp_dir("release");
        let mut server = Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir).unwrap();
        server.request("textDocument/hover", Value::Null);
        assert_eq!(server.queued.len(), 1);
        run_until_ready(&mut server);
        assert!(server.queued.is_empty(), "the handshake should have released it");
        // ...and after that, sending goes straight out rather than
        // being held again.
        server.notify("textDocument/didSave", Value::Null);
        assert!(server.queued.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // The server rejecting the handshake is a different failure from the
    // process dying, and it has to be reported as what it is.
    #[test]
    fn an_initialize_that_is_answered_with_an_error_kills_the_server() {
        let dir = temp_dir("refused");
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unsupported client"}}"#;
        let script = format!("printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{body}'; sleep 30", body.len());
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), script], "mock", &dir).unwrap();
        run_until_ready(&mut server);
        let State::Dead(why) = server.state() else { panic!("still {:?}", server.state()) };
        assert!(why.contains("unsupported client"), "{why}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_command_that_does_not_exist_fails_to_start_rather_than_hanging() {
        let dir = temp_dir("missing");
        let Err(error) = Server::start(1, &["bish-no-such-language-server".to_string()], "x", &dir) else {
            panic!("a command that isn't there should not have started");
        };
        assert!(error.contains("bish-no-such-language-server"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server that exits immediately is the single most common real
    // failure (a typo'd command, a missing runtime), and the reason it
    // gave on the way out is the whole value of keeping stderr.
    #[test]
    fn a_server_that_exits_is_noticed_with_its_last_words() {
        let dir = temp_dir("dies");
        let script = "echo 'cannot find configuration' >&2; exit 3";
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), script.to_string()], "sh", &dir).unwrap();
        for _ in 0..400 {
            server.service();
            if matches!(server.state(), State::Dead(_)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let State::Dead(why) = server.state() else {
            panic!("still {:?}", server.state());
        };
        assert!(why.contains("status 3"), "{why}");
        assert!(why.contains("cannot find configuration"), "the last thing it said should be the reason: {why}");
        assert!(server.log().any(|l| l.contains("cannot find configuration")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_server_stops_accepting_work() {
        let dir = temp_dir("dead");
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), "exit 0".to_string()], "sh", &dir).unwrap();
        for _ in 0..400 {
            server.service();
            if matches!(server.state(), State::Dead(_)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(matches!(server.state(), State::Dead(_)));
        server.request("textDocument/hover", Value::Null);
        assert!(server.outgoing.is_empty() && server.queued.is_empty(), "nothing should be queued for a server that is gone");
        assert!(server.service().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
