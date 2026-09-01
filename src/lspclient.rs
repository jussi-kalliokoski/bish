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

// How many uncollected responses to keep. A request that timed out may
// still be answered long afterwards with nobody left to receive it, so
// this is a ceiling on that debris rather than a real capacity: an
// actual conversation has one or two replies in flight.
const MAX_PENDING_RESPONSES: usize = 32;

// How many bytes may be owed to a server's stdin before it is declared
// broken.
//
// Queueing instead of blocking is what lets this be single-threaded
// (see the header), but a queue with no ceiling is just a slower way to
// run out of memory: a server that has stopped reading its input will
// never start again, and every keystroke adds another whole document to
// the pile. Generous enough that a large file's `didOpen` followed by a
// burst of edits fits comfortably, small enough to notice a wedge long
// before it matters.
const MAX_OUTGOING_BYTES: usize = 8 * 1024 * 1024;

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
    /// Kind 2: the server wants only the parts that changed, as ranges,
    /// rather than the whole file every time.
    pub incremental: bool,
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
        // Full, not incremental: a server that did not describe itself
        // is unlikely to be one that wants ranges.
        Sync { open_close: true, change: true, incremental: false, save: false }
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
            Ok(Value::Number(kind)) => Sync { open_close: true, change: *kind > 0.0, incremental: *kind == 2.0, save: false },
            Ok(value @ Value::Object(_)) => Sync {
                // The object form's fields really do all default to
                // false, per the spec -- a server choosing this form is
                // describing itself exactly.
                open_close: matches!(json::query(value, ".openClose"), Ok(Value::Bool(true))),
                change: matches!(json::query(value, ".change"), Ok(Value::Number(k)) if *k > 0.0),
                incremental: matches!(json::query(value, ".change"), Ok(Value::Number(k)) if *k == 2.0),
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
    // The text the server was last told about, kept only for a server
    // that asked for incremental sync -- it is the thing the next
    // change is a *difference from*, and there is nowhere else to get
    // it: a `TextBuffer` knows what it says now, not what was sent.
    //
    // Empty (and never filled) under full sync, so the extra copy is
    // paid only where it buys something.
    sent_text: String,
    // The most recent `publishDiagnostics` for this document, and
    // whether anyone has applied it yet.
    //
    // Kept rather than handed straight to a caller, because
    // diagnostics arrive on the idle tick -- which has no buffer -- and
    // are applied by the editor loop, which does. Holding the newest
    // here means an update that arrives for a pane nobody is currently
    // driving is waiting when they come back to it, rather than lost.
    // Only the newest is worth keeping: a publication replaces the
    // document's findings outright, it does not add to them.
    published: Option<lsp::Publication>,
    unapplied: bool,
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
    // Work the server says it is currently doing, newest last, keyed by
    // the `$/progress` token that will end it.
    //
    // This is the answer to the risk this feature was planned around: a
    // server that has not finished indexing answers nothing, and every
    // `K` or `gd` against it costs a full timeout with no explanation.
    // A server that reports progress is *saying* so, and the only thing
    // missing was somewhere to put it.
    //
    // Bounded because a token is only removed by the `end` that names
    // it, and a server that never sends one -- or crashes mid-work --
    // must not grow this forever.
    progress: Vec<(String, Progress)>,
    // `window/showMessage` lines the user has not been shown yet.
    //
    // Separate from `log`, which is where `window/logMessage` and the
    // server's own stderr go: the two look alike on the wire and mean
    // opposite things. A log line is for someone who went looking; a
    // show line is the server saying "tell them this" -- most often
    // that it could not load a project, which is the difference between
    // "nothing works and I don't know why" and knowing.
    shown: VecDeque<String>,
    // What this server should be told its configuration is, already
    // nested (see `settings_tree`). `Value::Object(vec![])` for a
    // server with no `--setting` of its own, which is the ordinary
    // case and means "nothing set, use your defaults" everywhere it is
    // read.
    settings: Value,

    state: State,
    // Responses to requests bish made, waiting to be collected.
    //
    // A `Vec` rather than a map because it is also the place staleness
    // is bounded: a request that timed out may still be answered
    // eventually, and nothing will ever collect it, so the oldest are
    // dropped once there are more than a handful outstanding. A real
    // conversation never has more than one or two in flight.
    responses: Vec<(i64, Result<Value, ResponseError>)>,
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
    apply_edits: ApplyEdits,
    // Ids of `workspace/executeCommand` requests still waiting for a
    // response. Non-empty is what "a command the user chose is running"
    // means for `ApplyEdits::Scoped`.
    commands: Vec<i64>,
    // Server-requested edits accepted by policy and not yet applied.
    // Deliberately unanswered: the reply has to say whether the edit
    // landed, and only the editor knows that, so the response waits for
    // `take_apply_edit`'s caller to do the work.
    applies: Vec<(Id, Value)>,
}

/// How far a server may go in changing files without being asked.
///
/// `workspace/applyEdit` is a request the *server* makes, at whatever
/// moment it likes. That is how a command-style code action does its
/// work -- the client runs `workspace/executeCommand`, and the edit
/// comes back as a separate request rather than as the command's
/// result -- but nothing in the protocol confines it to that moment. A
/// server may ask to rewrite a file nobody was editing, at any point
/// after the handshake.
///
/// So this is a policy and not a fact, and it is per declared server:
/// a language server you wrote yesterday and a large one you installed
/// from a distribution do not deserve the same latitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApplyEdits {
    /// Accept only while a command the user chose is still running.
    ///
    /// The default, and what makes command-style code actions work
    /// without handing over a standing permission: the window is open
    /// from the moment `workspace/executeCommand` goes out until its
    /// response comes back, which is exactly the span a server needs to
    /// carry out the thing that was picked from a menu.
    #[default]
    Scoped,
    /// Refuse always. What bish did before this existed.
    Never,
    /// Accept whenever asked, which is what VS Code does.
    Always,
}

impl ApplyEdits {
    pub fn from_name(name: &str) -> ApplyEdits {
        match name {
            "never" => ApplyEdits::Never,
            "always" => ApplyEdits::Always,
            _ => ApplyEdits::Scoped,
        }
    }
}

/// The most server-requested edits held unanswered at once.
///
/// A server that asks faster than the editor applies is either broken
/// or hostile; either way the answer past this point is a refusal
/// rather than unbounded memory.
const MAX_PENDING_APPLIES: usize = 8;

/// Builds the configuration object a server should be told about, from
/// the flat `dotted.key=value` pairs `::bish lsp add --setting` took.
///
/// `rust-analyzer.check.command=clippy` becomes
/// `{"rust-analyzer":{"check":{"command":"clippy"}}}` -- which is the
/// shape both `workspace/configuration` and `didChangeConfiguration`
/// are defined in terms of, and not a shape anyone should have to type
/// through two levels of shell quoting.
///
/// A value is JSON when it parses as JSON and a string otherwise, so
/// `enable=true` is a boolean, `procMacro.attributes=["a"]` is an
/// array, and `path=/usr/bin/foo` is a string without ceremony. The
/// fallback is deliberate: far more settings are strings than are
/// anything else, and a bare word is not valid JSON.
///
/// Two keys where one is a prefix of the other describe incompatible
/// shapes -- `a` cannot be both a number and an object -- so the later
/// one wins, exactly as a repeated key does. Nothing is dropped
/// silently and the rule is the same one either way: the last thing you
/// said is what you meant.
pub fn settings_tree(pairs: &[(String, String)]) -> Value {
    let mut root = Value::Object(Vec::new());
    for (key, raw) in pairs {
        let value = json::parse(raw).unwrap_or_else(|_| Value::Str(raw.clone()));
        let path: Vec<&str> = key.split('.').filter(|p| !p.is_empty()).collect();
        if path.is_empty() {
            continue;
        }
        insert_at(&mut root, &path, value);
    }
    root
}

// The subtree a `workspace/configuration` item's `section` names, or
// `Null` for one nothing was set for -- which is the protocol's own way
// of saying "nothing set, use your defaults".
//
// Walked here rather than handed to `json::query`. That used to be
// forced: `query`'s dotted field names accepted only alphanumerics and
// `_`, so `rust-analyzer` -- the most common section name there is --
// stopped at the hyphen and answered `null` for everything. `query` now
// takes a bare name up to the next `.` or `[`, so it could do this.
//
// Kept as a walk anyway, because the two answer subtly different
// questions: a section is a list of literal keys split on `.`, while a
// path is an expression where `[` and a leading `.` mean something. A
// server asking for a section whose name contains a bracket would be
// mis-parsed by the path form and is handled correctly here.
fn section_of(settings: &Value, section: &str) -> Value {
    let mut node = settings;
    for part in section.split('.').filter(|p| !p.is_empty()) {
        let Value::Object(fields) = node else { return Value::Null };
        match fields.iter().find(|(name, _)| name == part) {
            Some((_, value)) => node = value,
            None => return Value::Null,
        }
    }
    node.clone()
}

// Walks `path`, creating the objects it names, and puts `value` at the
// end of it.
fn insert_at(node: &mut Value, path: &[&str], value: Value) {
    let Value::Object(fields) = node else {
        // A leaf where an object is needed. The later key wins (see
        // `settings_tree`), so this becomes an object and the scalar
        // that was here goes.
        *node = Value::Object(Vec::new());
        return insert_at(node, path, value);
    };
    let [head, tail @ ..] = path else { return };
    if tail.is_empty() {
        fields.retain(|(name, _)| name != head);
        fields.push(((*head).to_string(), value));
        return;
    }
    if !fields.iter().any(|(name, _)| name == head) {
        fields.push(((*head).to_string(), Value::Object(Vec::new())));
    }
    let slot = fields.iter_mut().find(|(name, _)| name == head).expect("just ensured");
    insert_at(&mut slot.1, tail, value);
}

/// The most concurrent `$/progress` operations tracked at once.
///
/// Only the newest is ever displayed, so this is a memory bound rather
/// than a display one -- a server that begins work and never ends it
/// would otherwise accumulate tokens for as long as it runs.
const MAX_PROGRESS: usize = 16;

/// The most unread `window/showMessage` lines held at once.
const MAX_SHOWN: usize = 8;

/// One `$/progress` operation the server has begun and not yet ended.
///
/// `title` is fixed at `begin` and does not change; `message` and
/// `percentage` are whatever the last `report` said, and either may be
/// absent at any point -- a server is free to send a bare `begin` and
/// then nothing until `end`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Progress {
    title: String,
    message: Option<String>,
    percentage: Option<u8>,
}

impl Progress {
    // "Indexing 40%: core", with each half omitted when the server did
    // not supply it. Short on purpose: this competes for the status
    // line with the mode indicator and the cursor position, and a
    // server's own `message` can be a full crate path.
    fn line(&self) -> String {
        let mut text = self.title.clone();
        if let Some(pct) = self.percentage {
            text.push_str(&format!(" {pct}%"));
        }
        if let Some(message) = &self.message
            && !message.is_empty()
        {
            text.push_str(&format!(": {message}"));
        }
        text
    }
}

impl Server {
    /// Spawns `command` in `root` and sends `initialize`. Returns as
    /// soon as the process exists -- the handshake completes later, in
    /// `service`, which is why the returned server starts out
    /// `Initializing` rather than usable.
    pub fn start(id: u64, command: &[String], display: &str, root: &Path, apply_edits: ApplyEdits, settings: Value) -> Result<Server, String> {
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
            progress: Vec::new(),
            shown: VecDeque::new(),
            settings,
            state: State::Initializing,
            responses: Vec::new(),
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
            apply_edits,
            commands: Vec::new(),
            applies: Vec::new(),
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
                        ),
                        (
                            "codeAction".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // The literal form, meaning a server may
                                // answer with `CodeAction` objects
                                // rather than only the older bare
                                // `Command`s -- which is what carries an
                                // edit at all.
                                (
                                    "codeActionLiteralSupport".to_string(),
                                    Value::Object(vec![(
                                        "codeActionKind".to_string(),
                                        Value::Object(vec![("valueSet".to_string(), Value::Array(Vec::new()))]),
                                    )]),
                                ),
                                // Yes to being sent actions without
                                // their edits: `codeAction/resolve`
                                // fetches the one actually chosen, which
                                // is how rust-analyzer avoids computing
                                // every refactor nobody picked.
                                (
                                    "resolveSupport".to_string(),
                                    Value::Object(vec![("properties".to_string(), Value::Array(vec![Value::Str("edit".to_string())]))]),
                                ),
                                ("disabledSupport".to_string(), Value::Bool(true)),
                            ]),
                        ),
                        (
                            "rename".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // Not claimed: `prepareSupport` means
                                // the client asks whether a rename is
                                // legal here before offering one, which
                                // bish does not do -- it just asks.
                                ("prepareSupport".to_string(), Value::Bool(false)),
                            ]),
                        ),
                        (
                            "formatting".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        (
                            "documentSymbol".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // The nested form, which is what an
                                // indented outline wants; a server that
                                // only speaks the flat one still works
                                // (see `lsp::symbols`).
                                ("hierarchicalDocumentSymbolSupport".to_string(), Value::Bool(true)),
                            ]),
                        ),
                        (
                            "semanticTokens".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // Only the whole document. The delta and
                                // range forms exist to make a large file
                                // cheap, and bish asks for tokens off the
                                // idle path with the answer collected
                                // whenever it turns up -- nothing is
                                // waiting on it, so there is nothing for
                                // the cheaper forms to save yet.
                                (
                                    "requests".to_string(),
                                    Value::Object(vec![("full".to_string(), Value::Bool(true)), ("range".to_string(), Value::Bool(false))]),
                                ),
                                // Empty, both of them: a server sends
                                // whatever legend it likes and bish reads
                                // the names back out of it (see
                                // `lsp::SemanticLegend`). Declaring a
                                // fixed list here would mean refusing
                                // token types this client can in fact
                                // colour, since `::bish hl`'s namespace
                                // is open.
                                ("tokenTypes".to_string(), Value::Array(Vec::new())),
                                ("tokenModifiers".to_string(), Value::Array(Vec::new())),
                                ("formats".to_string(), Value::Array(vec![Value::Str("relative".to_string())])),
                                // Neither is claimed: bish paints one
                                // colour per token, so a server splitting
                                // an unrecognised multi-line token into
                                // per-line pieces (the first) and
                                // overlapping tokens (the second) would
                                // both need renderer work with nothing to
                                // show for it yet.
                                ("multilineTokenSupport".to_string(), Value::Bool(false)),
                                ("overlappingTokenSupport".to_string(), Value::Bool(false)),
                            ]),
                        ),
                        (
                            "references".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // "Where else in this file is what I'm on."
                        (
                            "documentHighlight".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // Inline parameter names and inferred types.
                        // `resolveSupport` is not claimed: a hint whose
                        // label only arrives after a second round trip
                        // would appear a beat after the code it
                        // annotates, and every server can answer
                        // completely if the client does not ask it to
                        // defer.
                        (
                            "inlayHint".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // The signature of the call being typed.
                        // `contextSupport` is not claimed: bish sends
                        // the position and nothing about *why* it
                        // asked, and a server told otherwise would look
                        // for a context that is not there.
                        (
                            "signatureHelp".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                (
                                    "signatureInformation".to_string(),
                                    Value::Object(vec![(
                                        "documentationFormat".to_string(),
                                        Value::Array(vec![Value::Str("plaintext".to_string())]),
                                    )]),
                                ),
                            ]),
                        ),
                        (
                            "definition".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // The three questions next to it: `gy`, `gD`,
                        // and `textDocument/implementation` (which has
                        // no key yet -- see vimkeys' own note on `gi`).
                        // Declared even where no key reaches it, since
                        // what a client can do and what it currently
                        // binds are different questions and a server
                        // reads only the first.
                        (
                            "typeDefinition".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        (
                            "implementation".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        (
                            "declaration".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        (
                            "completion".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                (
                                    "completionItem".to_string(),
                                    Value::Object(vec![
                                        // Claimed because it is now
                                        // true: a snippet completion
                                        // splices in tentatively with a
                                        // caret in its first tabstop
                                        // (see `bishedit::snippet`).
                                        // Undeclared, a server sends
                                        // plain text instead -- which is
                                        // why rust-analyzer's function
                                        // completions arrive without
                                        // their parentheses until a
                                        // client says this.
                                        ("snippetSupport".to_string(), Value::Bool(true)),
                                        // Not claimed: bish has no
                                        // second round trip for a
                                        // completion item, so a server
                                        // must send whatever it wants
                                        // used up front.
                                        (
                                            "resolveSupport".to_string(),
                                            Value::Object(vec![("properties".to_string(), Value::Array(Vec::new()))]),
                                        ),
                                        ("insertReplaceSupport".to_string(), Value::Bool(true)),
                                        (
                                            "documentationFormat".to_string(),
                                            Value::Array(vec![Value::Str("markdown".to_string()), Value::Str("plaintext".to_string())]),
                                        ),
                                    ]),
                                ),
                            ]),
                        ),
                        (
                            "hover".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // Markdown first, because that is what
                                // servers put their best answer in --
                                // but plaintext is accepted, and either
                                // is flattened to lines for the popup.
                                (
                                    "contentFormat".to_string(),
                                    Value::Array(vec![Value::Str("markdown".to_string()), Value::Str("plaintext".to_string())]),
                                ),
                            ]),
                        ),
                    ]),
                    ),
                    (
                        "workspace".to_string(),
                        Value::Object(vec![(
                            "workspaceEdit".to_string(),
                            Value::Object(vec![
                                // What bish can actually carry out. A
                                // server that would need to create,
                                // rename or delete a file is told so
                                // here, and the rename that needs one is
                                // refused rather than half-applied (see
                                // `lsp::WorkspaceEdit::unsupported`).
                                ("documentChanges".to_string(), Value::Bool(true)),
                                ("resourceOperations".to_string(), Value::Array(Vec::new())),
                            ]),
                        ),
                        // Declared to match the policy rather than
                        // always: a server told `applyEdit: true` and
                        // then refused every time has been lied to, and
                        // some react by disabling the feature that
                        // needed it anyway. Under `never` the honest
                        // claim is that bish cannot do this.
                        ("applyEdit".to_string(), Value::Bool(self.apply_edits != ApplyEdits::Never)),
                        // Claimed so a server knows a push is coming
                        // and does not have to poll for what it was
                        // already told -- see the `didChangeConfiguration`
                        // sent right after `initialized`.
                        (
                            "didChangeConfiguration".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // "What are this client's settings for these
                        // sections?" -- answered from the same place.
                        ("configuration".to_string(), Value::Bool(true)),
                        // The other half of a command-style code
                        // action: the action names a command, and this
                        // is how it gets run.
                        (
                            "executeCommand".to_string(),
                            Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))]),
                        ),
                        // `:sym` -- the project-wide half of `gO`.
                        (
                            "symbol".to_string(),
                            Value::Object(vec![
                                ("dynamicRegistration".to_string(), Value::Bool(false)),
                                // Not claimed: a `WorkspaceSymbol` whose
                                // location is only a uri would need a
                                // second round trip to find out *where*
                                // in the file, and bish would have
                                // nothing to show in the list until it
                                // came back. Declining means every
                                // answer arrives complete.
                                (
                                    "resolveSupport".to_string(),
                                    Value::Object(vec![("properties".to_string(), Value::Array(Vec::new()))]),
                                ),
                            ]),
                        ),
                    ]),
                    ),
                    (
                        "window".to_string(),
                        Value::Object(vec![
                            // Not a courtesy: rust-analyzer (and others)
                            // send `$/progress` only to a client that
                            // asked for it, so without this the status
                            // line has nothing to show and a cold server
                            // looks identical to a broken one.
                            ("workDoneProgress".to_string(), Value::Bool(true)),
                        ]),
                    ),
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
                        // Immediately after `initialized`, and only for
                        // a server that was actually given settings.
                        //
                        // Both halves are needed and neither is
                        // redundant: `workspace/configuration` is a
                        // *pull*, answered whenever the server gets
                        // round to asking, and some servers never do;
                        // this is the push, and it is what makes a
                        // setting take effect at startup rather than
                        // whenever the server happens to be curious.
                        if !matches!(&self.settings, Value::Object(fields) if fields.is_empty()) {
                            let params = Value::Object(vec![("settings".to_string(), self.settings.clone())]);
                            self.send_now(Message::Notification { method: "workspace/didChangeConfiguration".to_string(), params });
                        }
                        for message in std::mem::take(&mut self.queued) {
                            self.send_now(message);
                        }
                        self.note(format!("ready, position encoding {}", self.encoding.wire_name()));
                        // In the log rather than in `ls`, which is
                        // tab-separated for scripts and whose columns
                        // are not mine to move. This is the one place a
                        // setting that did not take effect can be
                        // checked against what was actually sent.
                        if !matches!(&self.settings, Value::Object(fields) if fields.is_empty()) {
                            self.note(format!("configuration: {}", json::compact_print(&self.settings)));
                        }
                    }
                }
                None
            }
            // A reply to something bish asked. Kept for whoever is
            // waiting on that id rather than handed back, since the
            // waiter is several frames up the stack from here.
            Message::Response { id: Id::Number(id), result } => {
                if self.responses.len() >= MAX_PENDING_RESPONSES {
                    // Dropping the oldest uncollected reply, which by
                    // definition nobody is still waiting for.
                    let _ = self.responses.remove(0);
                }
                self.commands.retain(|pending| pending != id);
                self.responses.push((*id, result.clone()));
                None
            }
            Message::Notification { method, params } if method == "textDocument/publishDiagnostics" => {
                if let Some(publication) = lsp::publication(params)
                    && let Some(document) = self.documents.iter_mut().find(|d| d.uri == publication.uri)
                {
                    document.published = Some(publication);
                    document.unapplied = true;
                }
                // A publication for a document this server was never
                // told about (or one already closed) is dropped: there
                // is no buffer for it to be about.
                None
            }
            // A request from the server. Answering *something* is not
            // optional -- JSON-RPC says a request gets a response, and
            // a server still waiting on one can stall indefinitely --
            // but answering everything `MethodNotFound` is wrong for
            // the few a real server actually sends, so each of those
            // gets the reply a client with nothing to offer should give
            // (see `answer`).
            // An edit the policy allows is not answered here: the
            // reply reports whether it was applied, and this layer
            // cannot apply anything. It is parked for the editor and
            // answered by whoever picks it up.
            Message::Request { id, method, params } if method == "workspace/applyEdit" && self.may_apply() => {
                if self.applies.len() >= MAX_PENDING_APPLIES {
                    let (id, _) = self.applies.remove(0);
                    self.note("refused workspace/applyEdit: too many still waiting to be applied".to_string());
                    self.respond(id, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])));
                }
                self.applies.push((id.clone(), params.clone()));
                None
            }
            Message::Request { id, method, params } => {
                let (answer, note) = self.answer(method, params);
                if let Some(note) = note {
                    self.note(note);
                }
                self.respond(id.clone(), answer);
                None
            }
            // "I have started / am partway through / have finished a
            // piece of work." Tracked rather than logged, because the
            // point of it is to be visible *while* it is true.
            Message::Notification { method, params } if method == "$/progress" => {
                self.progress_update(params);
                None
            }
            // A server's own log lines belong with the ones it wrote to
            // stderr, not in the caller's lap.
            Message::Notification { method, params } if method == "window/logMessage" || method == "window/showMessage" => {
                if let Ok(Value::Str(text)) = json::query(params, ".message") {
                    self.note(format!("{method}: {text}"));
                    // A `showMessage` is also queued for the user. The
                    // log keeps its copy either way, so `::bish lsp log`
                    // still shows the whole conversation in order.
                    if method == "window/showMessage" {
                        let severity = match json::query(params, ".type") {
                            Ok(Value::Number(n)) if *n == 1.0 => "error",
                            Ok(Value::Number(n)) if *n == 2.0 => "warning",
                            // 3 (info), 4 (log), and anything a server
                            // invents: shown without a label rather than
                            // labelled with a guess.
                            _ => "",
                        };
                        if self.shown.len() >= MAX_SHOWN {
                            // Dropping the oldest: a server producing
                            // these faster than they can be read is
                            // saying the same thing repeatedly, and the
                            // newest says it most currently.
                            self.shown.pop_front();
                        }
                        let name = self.display_name().to_string();
                        self.shown.push_back(if severity.is_empty() {
                            format!("{name}: {text}")
                        } else {
                            format!("{name} {severity}: {text}")
                        });
                    }
                }
                None
            }
            _ => Some(message),
        }
    }

    // What to reply to a request the server made, and optionally a line
    // for the log.
    //
    // The default really is `MethodNotFound`: it is the correct answer
    // for a method a client does not implement, and a server reading it
    // learns something true. The exceptions are the requests where
    // "not implemented" is *less* accurate than a real answer, and
    // where refusing degrades the server for no reason.
    fn answer(&self, method: &str, params: &Value) -> (Result<Value, ResponseError>, Option<String>) {
        match method {
            // "What are this client's settings for these sections?"
            // The honest answer from a client with no per-server
            // configuration is one `null` per item -- meaning "nothing
            // set, use your defaults" -- not "I don't know this
            // method", which makes servers log errors and sometimes
            // disable features outright.
            "workspace/configuration" => {
                let Ok(Value::Array(items)) = json::query(params, ".items") else {
                    return (Ok(Value::Array(Vec::new())), None);
                };
                // One answer per item, in order -- the reply is matched
                // to the request positionally, so a short array is a
                // protocol error rather than a partial answer.
                let answers = items
                    .iter()
                    .map(|item| match json::query(item, ".section") {
                        // A section names a path into the settings, the
                        // same dotted form `--setting` takes.
                        Ok(Value::Str(section)) => section_of(&self.settings, section),
                        // No section at all means "everything you have
                        // for me".
                        _ => self.settings.clone(),
                    })
                    .collect();
                (Ok(Value::Array(answers)), None)
            }
            // "May I report progress?" Yes -- the progress
            // notifications that follow are ignored, which costs
            // nothing, whereas refusing can stop a server reporting
            // indexing state it would otherwise recover from cleanly.
            "window/workDoneProgress/create" => (Ok(Value::Null), None),
            // Dynamic registration. bish declares `dynamicRegistration:
            // false` everywhere, so a conforming server never asks; one
            // that asks anyway is already off-spec, and accepting is
            // both what other clients do and harmless -- an unhonoured
            // registration and a refused one leave the server equally
            // uninformed, but the refusal can be treated as fatal.
            // Logged, because it means a server is doing something this
            // client did not expect.
            "client/registerCapability" | "client/unregisterCapability" => {
                (Ok(Value::Null), Some(format!("accepted and ignored {method}: bish declares no dynamic registration")))
            }
            // "Apply these edits to the workspace." Only reached when
            // the policy says no (the yes case never gets here -- see
            // `handle`). `applied: false` is a refusal in the server's
            // own vocabulary, so it can tell the user its refactor did
            // not happen instead of failing obscurely.
            "workspace/applyEdit" => (
                Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])),
                Some(match self.apply_edits {
                    ApplyEdits::Never => "refused workspace/applyEdit: --apply-edits=never".to_string(),
                    _ => "refused workspace/applyEdit: no command of yours was running".to_string(),
                }),
            ),
            // A message with buttons. Nothing displays it, so nothing
            // was chosen: `null` is the spec's own "user dismissed it".
            "window/showMessageRequest" => (Ok(Value::Null), None),
            // bish sends a single `rootUri` and declares no
            // workspace-folder support, so there is no list to give.
            "workspace/workspaceFolders" => (Ok(Value::Null), None),
            _ => (
                Err(ResponseError { code: -32601, message: format!("bish does not implement {method}") }),
                Some(format!("declined {method}: not implemented")),
            ),
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

    /// Whether a server-requested edit would be accepted right now.
    fn may_apply(&self) -> bool {
        match self.apply_edits {
            ApplyEdits::Never => false,
            ApplyEdits::Always => true,
            ApplyEdits::Scoped => !self.commands.is_empty(),
        }
    }

    /// This server's declared `--apply-edits` policy.
    pub fn apply_edits(&self) -> ApplyEdits {
        self.apply_edits
    }

    /// Runs a server command, holding the `applyEdit` window open until
    /// the response arrives. Returns the id to wait for.
    pub fn execute_command(&mut self, params: Value) -> i64 {
        let id = self.request("workspace/executeCommand", params);
        self.commands.push(id);
        id
    }

    /// Takes the next server-requested edit waiting to be applied. The
    /// caller owes it a `reply_to_apply`, or the server waits forever.
    pub fn take_apply_edit(&mut self) -> Option<(Id, Value)> {
        if self.applies.is_empty() {
            None
        } else {
            Some(self.applies.remove(0))
        }
    }

    /// Answers one edit taken with `take_apply_edit`.
    pub fn reply_to_apply(&mut self, id: Id, applied: bool) {
        self.respond(id, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(applied))])));
    }

    /// Refuses everything still parked. Called when the window closes
    /// -- a command finished, or gave up -- so no server is left
    /// waiting on a reply that is never coming.
    pub fn refuse_pending_applies(&mut self) {
        for (id, _) in std::mem::take(&mut self.applies) {
            self.respond(id, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])));
        }
        self.commands.clear();
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
        let encoded = lsp::encode(message);
        if self.outgoing.len() + encoded.len() > MAX_OUTGOING_BYTES {
            // Not a queue to grow: a server this far behind on reading
            // its own stdin is wedged, and pretending otherwise trades a
            // visible failure for an invisible leak.
            self.die(format!("stopped reading its input ({} bytes queued)", self.outgoing.len()));
            return;
        }
        self.outgoing.extend(encoded);
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
        self.documents.push(Document {
            uri: uri.to_string(),
            version,
            pending_since: None,
            sent_text: if self.sync.incremental { text.to_string() } else { String::new() },
            published: None,
            unapplied: false,
        });
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

    /// The version of `uri` the server has actually been told about.
    /// `None` for a document it was never told about at all.
    ///
    /// Asking a server about text it has not seen yet gets an answer
    /// about the text it *has* seen, whose offsets describe a different
    /// file -- which is why anything that positions itself by a server's
    /// answer checks this first.
    pub fn known_version(&self, uri: &str) -> Option<u64> {
        self.documents.iter().find(|d| d.uri == uri).map(|d| d.version)
    }

    /// The answer to `id`, once it has arrived. `None` means "not yet"
    /// -- the caller polls this while servicing everything else.
    pub fn take_response(&mut self, id: i64) -> Option<Result<Value, ResponseError>> {
        let at = self.responses.iter().position(|(pending, _)| *pending == id)?;
        Some(self.responses.remove(at).1)
    }

    /// Whether the server said it answers `textDocument/hover`. A
    /// provider capability may be a bare `true` or an options object,
    /// and both mean yes; anything else (including absent) means no,
    /// and asking anyway would spend a timeout to be told so.
    pub fn provides(&self, capability: &str) -> bool {
        matches!(json::query(&self.capabilities, &format!(".{capability}")), Ok(Value::Bool(true)) | Ok(Value::Object(_)))
    }

    /// The newest diagnostics for this document if they haven't been
    /// applied yet, marking them applied. `None` when nothing new has
    /// arrived -- which is the answer on almost every idle tick, so this
    /// is the cheap check the editor makes before doing any work.
    ///
    /// Deliberately does not drop them: a later call needs to be able to
    /// re-apply the same findings after an edit cleared the buffer's
    /// list, and re-publishing is the server's business, not something a
    /// client can ask for.
    pub fn take_diagnostics(&mut self, uri: &str) -> Option<&lsp::Publication> {
        let document = self.documents.iter_mut().find(|d| d.uri == uri)?;
        if !document.unapplied {
            return None;
        }
        document.unapplied = false;
        document.published.as_ref()
    }

    /// Marks the diagnostics this server last published for `uri` as
    /// needing to be applied again.
    ///
    /// Diagnostics are pushed, never requested -- there is no "send them
    /// again" in the protocol -- so after something clears a buffer's
    /// list (`:diag clear`) the only copy left is the one held here.
    /// This is what lets `:diag` put them back rather than leaving the
    /// user waiting for an edit that may never come.
    pub fn redeliver_diagnostics(&mut self, uri: &str) -> bool {
        let Some(document) = self.documents.iter_mut().find(|d| d.uri == uri) else {
            return false;
        };
        document.unapplied = document.published.is_some();
        document.unapplied
    }

    /// Sends the document's current text. A no-op when the server
    /// already has this version -- which is what makes it safe for
    /// `save_document` to flush unconditionally rather than having to
    /// ask first, and keeps a save that follows a settled edit from
    /// sending the same document twice.
    pub fn change_document(&mut self, uri: &str, version: u64, text: &str) {
        let incremental = self.sync.incremental;
        let encoding = self.encoding;
        let Some(document) = self.documents.iter_mut().find(|d| d.uri == uri) else {
            return;
        };
        if document.version == version {
            document.pending_since = None;
            return;
        }
        document.version = version;
        document.pending_since = None;
        let changes = if incremental {
            let changes = line_changes(&document.sent_text, text, encoding);
            document.sent_text = text.to_string();
            // `None` means the diff was not worth sending as one -- see
            // `line_changes` -- and the whole file goes instead, which
            // is always a legal `didChange` whatever kind the server
            // asked for.
            changes.unwrap_or_else(|| vec![Value::Object(vec![("text".to_string(), Value::Str(text.to_string()))])])
        } else {
            vec![Value::Object(vec![("text".to_string(), Value::Str(text.to_string()))])]
        };
        if changes.is_empty() {
            return;
        }
        self.notify(
            "textDocument/didChange",
            Value::Object(vec![
                (
                    "textDocument".to_string(),
                    Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string())), ("version".to_string(), Value::Number(version as f64))]),
                ),
                ("contentChanges".to_string(), Value::Array(changes)),
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

    // Applies one `$/progress` notification.
    //
    // A token is either a string or a number on the wire and is opaque
    // either way -- it only ever has to match the one an `end` names --
    // so both are kept as their own text. A `report` for a token that
    // was never begun is ignored rather than invented: without the
    // `begin` there is no title, and a percentage on its own says
    // nothing a status line could show.
    fn progress_update(&mut self, params: &Value) {
        apply_progress(&mut self.progress, params);
    }
}

// The body of `Server::progress_update`, as a free function so it can
// be tested against a plain `Vec` -- the alternative is spawning a real
// process to assert that "end" removes a token.
fn apply_progress(progress: &mut Vec<(String, Progress)>, params: &Value) {
    let token = match json::query(params, ".token") {
        Ok(Value::Str(text)) => text.clone(),
        Ok(Value::Number(n)) => format!("{n}"),
        _ => return,
    };
    let kind = match json::query(params, ".value.kind") {
        Ok(Value::Str(kind)) => kind.clone(),
        _ => return,
    };
    let message = match json::query(params, ".value.message") {
        Ok(Value::Str(text)) => Some(text.clone()),
        _ => None,
    };
    // Clamped rather than trusted: the spec says 0-100, and a
    // status line reading "Indexing 4000%" is worse than one
    // reading nothing.
    let percentage = match json::query(params, ".value.percentage") {
        Ok(Value::Number(n)) if *n >= 0.0 => Some((*n as u64).min(100) as u8),
        _ => None,
    };
    match kind.as_str() {
        "begin" => {
            let title = match json::query(params, ".value.title") {
                Ok(Value::Str(title)) => title.clone(),
                _ => String::new(),
            };
            if progress.len() >= MAX_PROGRESS {
                // The oldest is the one least likely to still be
                // running, and something has to give.
                let _ = progress.remove(0);
            }
            progress.retain(|(existing, _)| existing != &token);
            progress.push((token, Progress { title, message, percentage }));
        }
        "report" => {
            if let Some((_, running)) = progress.iter_mut().find(|(existing, _)| existing == &token) {
                // Absent fields mean "unchanged", not "cleared" --
                // rust-analyzer sends a bare percentage bump with no
                // message far more often than it sends both.
                if message.is_some() {
                    running.message = message;
                }
                if percentage.is_some() {
                    running.percentage = percentage;
                }
            }
        }
        "end" => {
            progress.retain(|(existing, _)| existing != &token);
        }
        _ => {}
    }
}

impl Server {
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

    /// The oldest `window/showMessage` line nobody has been shown yet,
    /// removed as it is taken.
    ///
    /// Drained one at a time rather than all at once because the place
    /// it goes -- the status line -- has room for one, and a server
    /// that said two things said the first one first.
    pub fn take_shown_message(&mut self) -> Option<String> {
        self.shown.pop_front()
    }

    /// What this server is busy with right now, or `None` when it is
    /// not busy with anything it has told us about.
    ///
    /// The *newest* operation rather than all of them: a status line
    /// has room for one, and the newest is the one whose progress is
    /// still moving. A server still handshaking reports that instead,
    /// since "not ready" is the same news for the same reason and a
    /// server generally starts reporting progress only once it is.
    pub fn progress_line(&self) -> Option<String> {
        if !self.is_ready() && !self.is_dead() {
            return Some(format!("{} starting", self.display_name()));
        }
        self.progress.last().map(|(_, progress)| progress.line())
    }

    // The server's own name for a status line: the command's first
    // word, without whatever path or arguments it was registered with.
    fn display_name(&self) -> &str {
        self.command.split_whitespace().next().unwrap_or(&self.command).rsplit('/').next().unwrap_or(&self.command)
    }

    /// Whether this server has stopped for good. A dead server is kept
    /// in the table (`::bish lsp status` still has to explain what
    /// happened), so anyone acting on what it once said has to ask.
    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Dead(_))
    }

    /// The names this server gave its own semantic token types and
    /// modifiers, from the handshake. Empty for a server that does not
    /// do semantic tokens at all.
    pub fn semantic_legend(&self) -> lsp::SemanticLegend {
        lsp::SemanticLegend::parse(&self.capabilities)
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
    pub fn get_or_start(
        &mut self,
        id: u64,
        command: &[String],
        display: &str,
        root: &Path,
        apply_edits: ApplyEdits,
        settings: Value,
    ) -> Result<&mut Server, String> {
        if let Some(index) = self.servers.iter().position(|s| s.command == display && s.root == root) {
            return Ok(&mut self.servers[index]);
        }
        if let Some(failure) = self.failures.iter().find(|f| f.command == display && f.root == root) {
            return Err(failure.why.clone());
        }
        match Server::start(id, command, display, root, apply_edits, settings) {
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
            // Every request is already answered inside `service` (see
            // `Server::answer`); what comes back here is notifications
            // and replies nobody collected, neither of which anything
            // consumes yet.
            let _ = server.service();
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

impl crate::exec::ServiceTable for Table {
    // What `::bish lsp status` prints. The shell joins these with tabs
    // and knows nothing about what any of them mean.
    fn rows(&self) -> Vec<Vec<String>> {
        let running = self.servers.iter().map(|s| {
            let encoding = if s.is_ready() { s.encoding().wire_name() } else { "" };
            vec![
                s.id.to_string(),
                s.state().describe(),
                encoding.to_string(),
                s.open_documents().to_string(),
                s.root.display().to_string(),
                s.command.clone(),
            ]
        });
        // A server that never started has no state of its own to
        // report, but it is the thing someone running `status` is most
        // likely looking for, so it appears in the same shape rather
        // than being left out.
        let failed = self.failures.iter().map(|f| {
            vec![f.id.to_string(), format!("dead: {}", f.why), String::new(), "0".to_string(), f.root.display().to_string(), f.command.clone()]
        });
        running.chain(failed).collect()
    }

    fn logs(&self, id: u64) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for server in self.servers.iter().filter(|s| s.id == id) {
            out.extend(server.log().cloned());
        }
        // A failure to start produces no `Server` at all, and is
        // precisely the case someone runs `log` for.
        out.extend(self.failures.iter().filter(|f| f.id == id).map(|f| f.why.clone()));
        out
    }

    fn forget(&mut self, id: u64) -> usize {
        let before = self.servers.len() + self.failures.len();
        // Told to stop on the way out, so a server holding a cache
        // writes it: `Server::drop` would otherwise just kill it.
        for server in self.servers.iter_mut().filter(|s| s.id == id) {
            server.shutdown();
        }
        self.servers.retain(|s| s.id != id);
        self.failures.retain(|f| f.id != id);
        before - (self.servers.len() + self.failures.len())
    }
}

/// The directory to treat as the project root for `path`: the nearest
/// ancestor containing **any** of `markers`.
///
/// Proximity wins, not marker order -- which is what every mainstream
/// client does (nvim-lspconfig's `root_pattern` walks up checking all
/// its patterns at each level; VS Code roots at the opened folder).
/// This was originally the other way round, trying each marker to the
/// top before the next, on the reasoning that `--root=Cargo.toml,.git`
/// meant "prefer a crate to a repository". That reasoning is wrong in
/// the case it was aimed at: with a `Cargo.toml` above and a `.git`
/// below it, marker-order picks the *further* directory, which is a
/// bigger tree than the file belongs to.
///
/// The genuinely hard case this does not solve: a member of a Cargo
/// workspace should usually be rooted at the workspace, not at its own
/// crate, and only `cargo metadata` knows which. Every client that gets
/// that right shells out to Cargo for it. Out of scope here; the
/// documented fix is to name the workspace directory explicitly.
///
/// `None` when nothing matches, which is the caller's cue not to start
/// a server at all rather than to guess: a server given the wrong root
/// indexes the wrong tree, and for a large one that is expensive enough
/// to notice.
/// The most separate edits one `didChange` will carry.
///
/// Past this the diff has stopped describing an edit and started
/// describing a different file (a formatter ran, a branch was checked
/// out), and one full-text change is both smaller on the wire and
/// cheaper for the server to apply than fifty ranges.
const MAX_CONTENT_CHANGES: usize = 32;

/// `old` -> `new` as LSP `contentChanges`, or `None` when sending the
/// whole file would be better.
///
/// Whole lines only. A character-level diff would send fewer bytes, but
/// every column in a range has to be in the negotiated position
/// encoding, and getting that wrong does not look wrong -- it silently
/// describes a different edit. Line boundaries are the same number in
/// every encoding, so the only column arithmetic left is the one at the
/// very end of the file, where a range cannot start on a line that does
/// not exist.
///
/// The changes come out in *reverse* document order, because the server
/// applies them in the order given and each one shifts everything after
/// it -- the same rule `fileeditor::apply_text_edits` follows for a
/// batch of edits going the other way.
fn line_changes(old: &str, new: &str, encoding: PositionEncoding) -> Option<Vec<Value>> {
    if old.is_empty() && !new.is_empty() {
        // Nothing to be a difference from: this is the first change
        // after an open that predates incremental sync being known, or
        // a document that really was empty and is now not.
        return None;
    }
    // `split('\n')` and not `lines()`: a trailing newline means a final
    // empty line, which is exactly LSP's own model of a document, and
    // `lines()` would hide it.
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut changes = Vec::new();
    // One hunk per Delete/Insert run, with an adjacent pair of them
    // treated as the single replacement it is.
    let ops = crate::diff::diff(&old_lines, &new_lines);
    let mut i = 0;
    while i < ops.len() {
        let (from, removed, at) = match ops[i] {
            crate::diff::DiffOp::Equal { .. } => {
                i += 1;
                continue;
            }
            crate::diff::DiffOp::Delete { a, len } => (a, len, None),
            crate::diff::DiffOp::Insert { b, len } => {
                // An insertion's position in `old` is wherever the
                // preceding equal run ended.
                let a = ops[..i]
                    .iter()
                    .rev()
                    .find_map(|op| match op {
                        crate::diff::DiffOp::Equal { a, len, .. } | crate::diff::DiffOp::Delete { a, len } => Some(a + len),
                        crate::diff::DiffOp::Insert { .. } => None,
                    })
                    .unwrap_or(0);
                (a, 0, Some((b, len)))
            }
        };
        // A delete immediately followed by an insert is one replacement.
        let inserted = match at {
            Some(range) => {
                i += 1;
                Some(range)
            }
            None => match ops.get(i + 1) {
                Some(crate::diff::DiffOp::Insert { b, len }) => {
                    i += 2;
                    Some((*b, *len))
                }
                _ => {
                    i += 1;
                    None
                }
            },
        };
        if changes.len() >= MAX_CONTENT_CHANGES {
            return None;
        }
        let to = from + removed;
        let added: Vec<&str> = match inserted {
            Some((b, len)) => new_lines[b..b + len].to_vec(),
            None => Vec::new(),
        };
        // A hunk that runs off the end of the document cannot be
        // expressed as `{line: <count>, character: 0}` -- that line does
        // not exist -- so it ends at the end of the last line that does.
        // Two shapes of that, and they differ in where the newline goes:
        // a *replacement* reaching the end drops its trailing one, while
        // a pure *append* past the end needs a leading one, since the
        // line it is being appended after has none.
        let (start, end, text) = if to < old_lines.len() {
            // The ordinary case: whole lines swapped for whole lines,
            // every replacement newline-terminated so the line after the
            // hunk stays a line.
            (position(from, 0), position(to, 0), added.iter().map(|line| format!("{line}\n")).collect::<String>())
        } else if from == 0 {
            // The whole document.
            (position(0, 0), end_of(&old_lines, encoding), added.join("\n"))
        } else {
            // A hunk running to the end of the document, which cannot be
            // written as lines `[from, count)` -- `{line: count}` does
            // not exist, and worse, a range starting at `{from, 0}`
            // leaves the newline that *ends the line before it* behind,
            // so the result keeps a trailing blank line that the new
            // text does not have.
            //
            // So it starts at the end of the previous line and takes
            // that newline with it, and whatever replaces it brings its
            // own leading one. Deleting to the end and appending past it
            // are the same shape once written this way.
            let previous: Vec<char> = old_lines[from - 1].chars().collect();
            let text = if added.is_empty() { String::new() } else { format!("\n{}", added.join("\n")) };
            (position(from - 1, lsp::to_server_column(&previous, previous.len(), encoding)), end_of(&old_lines, encoding), text)
        };
        changes.push(Value::Object(vec![
            ("range".to_string(), Value::Object(vec![("start".to_string(), start), ("end".to_string(), end)])),
            ("text".to_string(), Value::Str(text)),
        ]));
    }
    changes.reverse();
    Some(changes)
}

// The very last position in a document, as the server counts columns.
fn end_of(lines: &[&str], encoding: PositionEncoding) -> Value {
    let last: Vec<char> = lines[lines.len() - 1].chars().collect();
    position(lines.len() - 1, lsp::to_server_column(&last, last.len(), encoding))
}

fn position(line: usize, character: usize) -> Value {
    Value::Object(vec![
        ("line".to_string(), Value::Number(line as f64)),
        ("character".to_string(), Value::Number(character as f64)),
    ])
}

pub fn root_for(path: &Path, markers: &[String]) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    let mut dir = Some(start);
    while let Some(current) = dir {
        if markers.iter().any(|marker| current.join(marker).exists()) {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
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

        let first = table.get_or_start(1, &command, "cat", &dir, ApplyEdits::default(), Value::Object(Vec::new())).map(|s| s.root.clone()).unwrap();
        assert_eq!(table.servers().len(), 1);
        // Same command, same root: the existing one, which is the whole
        // point -- every pane editing a project shares its server.
        table.get_or_start(1, &command, "cat", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        assert_eq!(table.servers().len(), 1);
        // Same command, different root: a second server, because a
        // server is scoped to the project it was told about.
        table.get_or_start(1, &command, "cat", &other, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
            assert!(table.get_or_start(7, &command, "nope", &dir, ApplyEdits::default(), Value::Object(Vec::new())).is_err());
        }
        assert!(table.servers().is_empty());
        assert_eq!(table.failures().len(), 1, "one record, however many times it was asked for");
        assert_eq!(table.failures()[0].id, 7);
        assert!(table.failures()[0].why.contains("bish-no-such-language-server"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A request has to be answered -- a server still waiting on one can
    // stall -- but "not implemented" is the wrong answer for the few a
    // real server actually sends.
    #[test]
    fn a_server_request_gets_the_answer_a_client_with_nothing_to_offer_should_give() {
        let dir = temp_dir("answer");
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();

        // Settings for three sections, none configured: one `null`
        // each, meaning "use your defaults". MethodNotFound here makes
        // servers log errors and sometimes disable features.
        let params = json::parse(r#"{"items":[{"section":"a"},{"section":"b"},{"section":"c"}]}"#).unwrap();
        let (answer, _) = server.answer("workspace/configuration", &params);
        assert_eq!(answer, Ok(Value::Array(vec![Value::Null; 3])));

        // Progress: accepted, because the notifications that follow are
        // ignored at no cost, while refusing can stop a server
        // reporting indexing state.
        assert_eq!(server.answer("window/workDoneProgress/create", &Value::Null).0, Ok(Value::Null));

        // Dynamic registration: accepted and ignored, and logged --
        // bish declares it does none, so a server asking is already
        // doing something unexpected.
        let (answer, note) = server.answer("client/registerCapability", &Value::Null);
        assert_eq!(answer, Ok(Value::Null));
        assert!(note.unwrap().contains("dynamic registration"));

        // An edit bish cannot apply: said in the server's own
        // vocabulary, so it can tell the user the refactor did not
        // happen rather than failing obscurely.
        assert_eq!(
            server.answer("workspace/applyEdit", &Value::Null).0,
            Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))]))
        );

        // And the default really is MethodNotFound: it is the correct
        // answer for a method a client does not implement.
        let (answer, note) = server.answer("workspace/somethingInvented", &Value::Null);
        assert_eq!(answer.unwrap_err().code, -32601);
        assert!(note.unwrap().contains("not implemented"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Applies `contentChanges` the way a server does: in the order
    // given, each one against the document the previous left behind.
    // The whole point of the reverse ordering `line_changes` produces is
    // that this comes out right.
    fn apply_changes(text: &str, changes: &[Value]) -> String {
        let mut text = text.to_string();
        for change in changes {
            let Ok(Value::Str(new_text)) = json::query(change, ".text") else { panic!("no text") };
            let Ok(range) = json::query(change, ".range") else {
                text = new_text.clone();
                continue;
            };
            let at = |path: &str| match json::query(range, path) {
                Ok(Value::Number(n)) => *n as usize,
                _ => panic!("no {path}"),
            };
            // Char offsets, which is what utf-32 columns already are.
            let offset = |line: usize, character: usize| {
                let mut chars = 0;
                for (i, l) in text.split('\n').enumerate() {
                    if i == line {
                        return chars + character.min(l.chars().count());
                    }
                    chars += l.chars().count() + 1;
                }
                text.chars().count()
            };
            let start = offset(at(".start.line"), at(".start.character"));
            let end = offset(at(".end.line"), at(".end.character"));
            let before: String = text.chars().take(start).collect();
            let after: String = text.chars().skip(end.max(start)).collect();
            text = format!("{before}{new_text}{after}");
        }
        text
    }

    fn round_trip(old: &str, new: &str) -> usize {
        let changes = line_changes(old, new, PositionEncoding::Utf32).unwrap_or_else(|| panic!("{old:?} -> {new:?} refused"));
        assert_eq!(apply_changes(old, &changes), new, "for {old:?} -> {new:?} via {changes:?}");
        changes.len()
    }

    // The one thing that actually has to be true: whatever the diff
    // produces, a server applying it in order ends up with the text bish
    // has. Every shape of edit, checked by reconstructing the result
    // rather than by eyeballing the ranges.
    #[test]
    fn incremental_changes_reconstruct_the_new_text_exactly() {
        // A line rewritten in the middle.
        assert_eq!(round_trip("a\nb\nc\n", "a\nB\nc\n"), 1);
        // Inserted, deleted, both at once.
        round_trip("a\nc\n", "a\nb\nc\n");
        round_trip("a\nb\nc\n", "a\nc\n");
        round_trip("a\nb\nc\n", "a\nx\ny\nz\nc\n");
        // The first line, and the last -- where a range cannot start on
        // a line that does not exist.
        round_trip("a\nb\nc\n", "A\nb\nc\n");
        round_trip("a\nb\nc", "a\nb\nC");
        round_trip("a\nb\nc", "a\nb\nc\nd");
        round_trip("a\nb\nc\n", "a\nb\nc\nd\n");
        round_trip("a\nb\nc\nd\n", "a\nb\n");
        // No trailing newline either side, one whole-file rewrite.
        round_trip("one", "two");
        // Two separate hunks, which is where the reverse ordering earns
        // its place: applied forwards, the second range would describe
        // text the first had already moved.
        assert_eq!(round_trip("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n"), 2);
        assert_eq!(round_trip("1\n2\n3\n4\n5\n6\n7\n", "1\nX\n3\n4\nY\nZ\n7\n"), 2);
        // Nothing changed at all.
        assert!(line_changes("a\nb\n", "a\nb\n", PositionEncoding::Utf32).unwrap().is_empty());
        // Emptied entirely, and filled from nothing.
        round_trip("a\nb\n", "");
        round_trip("", "");
    }

    // Non-ASCII is exactly where a hand-written range goes wrong, and
    // does so invisibly. Whole lines are the same number in every
    // encoding; the one column that is not is the end of the last line.
    #[test]
    fn a_hunk_at_the_end_of_a_file_uses_the_negotiated_encoding() {
        let old = "a\n\u{1f600}\u{1f600}";
        let changes = line_changes(old, "a\nx", PositionEncoding::Utf16).unwrap();
        assert_eq!(json::query(&changes[0], ".range.end.character"), Ok(&Value::Number(4.0)), "two surrogate pairs");
        let changes = line_changes(old, "a\nx", PositionEncoding::Utf32).unwrap();
        assert_eq!(json::query(&changes[0], ".range.end.character"), Ok(&Value::Number(2.0)), "two chars");
        let changes = line_changes(old, "a\nx", PositionEncoding::Utf8).unwrap();
        assert_eq!(json::query(&changes[0], ".range.end.character"), Ok(&Value::Number(8.0)), "eight bytes");
    }

    // The cases above are the ones worth naming; these are the ones
    // nobody thinks of. A tiny deterministic generator over line sets,
    // every result reconstructed -- which is the only property that
    // actually matters and the only one worth checking exhaustively.
    #[test]
    fn incremental_changes_round_trip_for_edits_nobody_would_think_to_write() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = ["a", "bb", "ccc", "", "\u{1f600}", "x y z"];
        for _ in 0..2000 {
            let build = |n: usize, next: &mut dyn FnMut() -> u64| {
                let mut lines: Vec<&str> = (0..n).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
                // Half the time with a trailing newline, half without --
                // the distinction the last-line arithmetic turns on.
                if next().is_multiple_of(2) {
                    lines.push("");
                }
                lines.join("\n")
            };
            let old = build((next() % 12) as usize, &mut next);
            let new = build((next() % 12) as usize, &mut next);
            if old.is_empty() && !new.is_empty() {
                continue;
            }
            round_trip(&old, &new);
        }
    }

    // The declaration is the server's, not the client's: kind 2 means
    // it wants ranges, and sending it whole files instead is the client
    // ignoring what it was told.
    #[test]
    fn a_server_that_asked_for_incremental_sync_gets_ranges() {
        let dir = temp_dir("incremental");
        let capabilities = r#"{"capabilities":{"positionEncoding":"utf-32","textDocumentSync":{"openClose":true,"change":2,"save":false}}}"#;
        let mut server = Server::start(1, &echo_server(capabilities), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert!(server.sync().incremental);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one\ntwo\nthree\n");
        server.change_document("file:///p/x.sh", 2, "one\nTWO\nthree\n");
        let seen = document_notifications(&mut server, 2);
        let (method, params) = &seen[1];
        assert_eq!(method, "textDocument/didChange");
        let Ok(Value::Array(changes)) = json::query(params, ".contentChanges") else { panic!("no changes in {params:?}") };
        assert_eq!(changes.len(), 1);
        // Just the one line, named by range -- not the whole file.
        assert_eq!(json::query(&changes[0], ".text"), Ok(&Value::Str("TWO\n".to_string())));
        assert_eq!(json::query(&changes[0], ".range.start.line"), Ok(&Value::Number(1.0)));
        assert_eq!(json::query(&changes[0], ".range.end.line"), Ok(&Value::Number(2.0)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // And a server that asked for full sync still gets full files --
    // the first change after an open is also full, since there is
    // nothing yet to be a difference from.
    #[test]
    fn a_full_sync_server_still_gets_whole_files() {
        let dir = temp_dir("full-sync");
        let mut server = Server::start(1, &echo_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert!(!server.sync().incremental);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one\ntwo\n");
        server.change_document("file:///p/x.sh", 2, "one\nTWO\n");
        let seen = document_notifications(&mut server, 2);
        let Ok(Value::Array(changes)) = json::query(&seen[1].1, ".contentChanges") else { panic!("no changes") };
        assert_eq!(changes.len(), 1);
        assert_eq!(json::query(&changes[0], ".text"), Ok(&Value::Str("one\nTWO\n".to_string())));
        assert!(!matches!(json::query(&changes[0], ".range"), Ok(Value::Object(_))), "no range means the whole document");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Past a point the diff has stopped describing an edit and started
    // describing a different file, and one full-text change is smaller
    // on the wire and cheaper to apply than fifty ranges.
    #[test]
    fn a_wholesale_rewrite_falls_back_to_sending_the_file() {
        // Every third line rewritten, so the diff really is dozens of
        // separate hunks rather than one big replacement.
        let old: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..200).map(|i| if i % 3 == 0 { format!("changed {i}\n") } else { format!("line {i}\n") }).collect();
        assert!(line_changes(&old, &new, PositionEncoding::Utf32).is_none());
        // And with nothing to be a difference from.
        assert!(line_changes("", "hello\n", PositionEncoding::Utf32).is_none());
    }

    // `--apply-edits` is a policy, so the thing to pin down is what
    // each setting actually does to a request that arrives.
    #[test]
    fn a_server_requested_edit_is_taken_or_refused_by_policy() {
        let dir = temp_dir("apply-policy");
        let params = json::parse(r#"{"edit":{"changes":{}}}"#).unwrap();
        let request = |id: i64| Message::Request { id: Id::Number(id), method: "workspace/applyEdit".to_string(), params: params.clone() };
        let refused = Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))]));

        // scoped, with nothing of the user's running: refused, and the
        // log says which of the two refusals it was.
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Scoped, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert!(!server.may_apply());
        let (answer, note) = server.answer("workspace/applyEdit", &Value::Null);
        assert_eq!(answer, refused);
        assert!(note.unwrap().contains("no command of yours"));
        assert!(server.handle(request(1)).is_none());
        assert!(server.take_apply_edit().is_none(), "refused, so nothing is waiting for the editor");

        // The same server, with a command the user chose in flight:
        // parked rather than answered, because the reply has to say
        // whether the edit landed and only the editor knows that.
        server.execute_command(json::parse(r#"{"command":"x"}"#).unwrap());
        assert!(server.may_apply());
        assert!(server.handle(request(2)).is_none());
        let (id, taken) = server.take_apply_edit().expect("parked for the editor");
        assert_eq!(id, Id::Number(2));
        assert_eq!(json::query(&taken, ".edit.changes"), Ok(&Value::Object(Vec::new())));

        // And the window closes when the command's own response comes
        // back -- that is the whole of what "scoped" means.
        let done = Message::Response { id: Id::Number(server.next_id - 1), result: Ok(Value::Null) };
        assert!(server.handle(done).is_none());
        assert!(!server.may_apply());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn never_and_always_do_not_wait_for_a_command() {
        let dir = temp_dir("apply-ends");
        let request = Message::Request { id: Id::Number(1), method: "workspace/applyEdit".to_string(), params: Value::Null };

        let mut never = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Never, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut never);
        never.execute_command(Value::Null);
        assert!(!never.may_apply(), "a command of the user's own does not unlock `never`");
        assert!(never.answer("workspace/applyEdit", &Value::Null).1.unwrap().contains("never"));

        let mut always = Server::start(2, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Always, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut always);
        assert!(always.may_apply(), "no command needed");
        assert!(always.handle(request).is_none());
        assert!(always.take_apply_edit().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    // Keeps servicing for long enough that everything queued has been
    // written and echoed back into the fixture's log.
    fn drain(server: &mut Server) {
        for _ in 0..100 {
            server.service();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    fn tree(items: &[(&str, &str)]) -> String {
        json::compact_print(&settings_tree(&pairs(items)))
    }

    #[test]
    fn dotted_keys_become_the_nested_object_a_server_expects() {
        assert_eq!(
            tree(&[("rust-analyzer.check.command", "clippy")]),
            r#"{"rust-analyzer":{"check":{"command":"clippy"}}}"#
        );
        // Two keys sharing a prefix share the objects along it rather
        // than each rebuilding their own.
        assert_eq!(
            tree(&[("a.b", "1"), ("a.c", "2")]),
            r#"{"a":{"b":1,"c":2}}"#
        );
    }

    #[test]
    fn a_value_is_json_when_it_is_json_and_a_string_when_it_is_not() {
        // The distinction that matters: `true` the boolean is not
        // `"true"` the string, and a server told the wrong one either
        // ignores the setting or rejects it.
        assert_eq!(tree(&[("x.enable", "true")]), r#"{"x":{"enable":true}}"#);
        assert_eq!(tree(&[("x.count", "3")]), r#"{"x":{"count":3}}"#);
        assert_eq!(tree(&[("x.tags", r#"["a","b"]"#)]), r#"{"x":{"tags":["a","b"]}}"#);
        // A bare word is not valid JSON, and far more settings are
        // strings than are anything else.
        assert_eq!(tree(&[("x.path", "/usr/bin/foo")]), r#"{"x":{"path":"/usr/bin/foo"}}"#);
        assert_eq!(tree(&[("x.name", "clippy")]), r#"{"x":{"name":"clippy"}}"#);
    }

    // `a` cannot be both a number and an object, so one of the two has
    // to go. The rule is the same one a repeated key already follows.
    #[test]
    fn a_key_that_contradicts_an_earlier_one_replaces_it() {
        assert_eq!(tree(&[("a", "1"), ("a.b", "2")]), r#"{"a":{"b":2}}"#, "the object replaces the scalar");
        assert_eq!(tree(&[("a.b", "2"), ("a", "1")]), r#"{"a":1}"#, "and the scalar replaces the object");
        // The plain repeated-key case, for the same reason.
        assert_eq!(tree(&[("a.b", "1"), ("a.b", "2")]), r#"{"a":{"b":2}}"#);
    }

    #[test]
    fn no_settings_is_an_empty_object_not_a_null() {
        // Empty is what "nothing set, use your defaults" looks like
        // everywhere this is read, and it is also what suppresses the
        // `didChangeConfiguration` push.
        assert_eq!(tree(&[]), "{}");
    }

    // The pull half. A server asks for the sections it cares about and
    // gets one answer per item, in order -- a short array would be a
    // protocol error, not a partial answer.
    #[test]
    fn workspace_configuration_is_answered_from_the_settings_it_was_given() {
        let dir = temp_dir("configuration");
        let settings = settings_tree(&pairs(&[("rust-analyzer.check.command", "clippy"), ("other.thing", "1")]));
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), settings).unwrap();
        let request = |sections: &[&str]| {
            let items = sections
                .iter()
                .map(|s| Value::Object(vec![("section".to_string(), Value::Str((*s).to_string()))]))
                .collect();
            Value::Object(vec![("items".to_string(), Value::Array(items))])
        };

        let (answer, _) = server.answer("workspace/configuration", &request(&["rust-analyzer.check"]));
        assert_eq!(json::compact_print(&answer.unwrap()), r#"[{"command":"clippy"}]"#);

        // One answer per item, in the order asked, with `null` for a
        // section nothing was set for -- which is "use your defaults",
        // and is what this used to answer for every item.
        let (answer, _) = server.answer("workspace/configuration", &request(&["nothing.here", "other.thing"]));
        assert_eq!(json::compact_print(&answer.unwrap()), r#"[null,1]"#);

        // An item with no section at all means "everything".
        let all = Value::Object(vec![("items".to_string(), Value::Array(vec![Value::Object(Vec::new())]))]);
        let (answer, _) = server.answer("workspace/configuration", &all);
        assert!(json::compact_print(&answer.unwrap()).contains("rust-analyzer"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // The push half. `workspace/configuration` is answered whenever the
    // server gets round to asking, and some never do.
    #[test]
    fn settings_are_pushed_once_the_handshake_finishes_and_only_when_there_are_any() {
        let dir = temp_dir("didchangeconfig");
        let log = dir.join("received.jsonl");
        let settings = settings_tree(&pairs(&[("mock.enable", "true")]));
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), settings).unwrap();
        run_until_ready(&mut server);
        // `run_until_ready` stops the moment the state flips, which is
        // before what that flip queued has been written to a
        // non-blocking pipe, let alone read at the other end.
        drain(&mut server);
        let methods: Vec<String> = received(&log)
            .iter()
            .filter_map(|m| match m {
                Message::Request { method, .. } | Message::Notification { method, .. } => Some(method.clone()),
                _ => None,
            })
            .collect();
        let at = methods.iter().position(|m| m == "workspace/didChangeConfiguration");
        assert!(at.is_some(), "expected a push, got {methods:?}");
        // After `initialized`, never before: nothing but `initialize`
        // may precede it.
        assert!(at > methods.iter().position(|m| m == "initialized"), "{methods:?}");

        let sent = received(&log).into_iter().find_map(|m| match m {
            Message::Notification { method, params } if method == "workspace/didChangeConfiguration" => Some(params),
            _ => None,
        });
        assert_eq!(json::query(&sent.unwrap(), ".settings.mock.enable"), Ok(&Value::Bool(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_server_with_no_settings_is_not_pushed_an_empty_configuration() {
        let dir = temp_dir("noconfig");
        let log = dir.join("received.jsonl");
        let mut server =
            Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        drain(&mut server);
        assert!(
            !received(&log).iter().any(|m| matches!(m, Message::Notification { method, .. } if method == "workspace/didChangeConfiguration")),
            "nothing to say is not the same as saying nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // The two `window/*Message` notifications look alike on the wire
    // and mean opposite things: one is for someone who went looking,
    // the other is the server asking to interrupt.
    #[test]
    fn a_show_message_reaches_the_user_and_a_log_message_only_the_log() {
        let dir = temp_dir("show-message");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        let notify = |method: &str, kind: f64, text: &str| Message::Notification {
            method: method.to_string(),
            params: Value::Object(vec![
                ("type".to_string(), Value::Number(kind)),
                ("message".to_string(), Value::Str(text.to_string())),
            ]),
        };
        assert!(server.handle(notify("window/logMessage", 1.0, "loading")).is_none());
        assert_eq!(server.take_shown_message(), None, "a log line is not shown");

        assert!(server.handle(notify("window/showMessage", 1.0, "no Cargo.toml")).is_none());
        assert!(server.handle(notify("window/showMessage", 3.0, "reloaded")).is_none());
        // Oldest first, and the severity is labelled only where it says
        // something an unlabelled line would not.
        assert_eq!(server.take_shown_message().as_deref(), Some("mock error: no Cargo.toml"));
        assert_eq!(server.take_shown_message().as_deref(), Some("mock: reloaded"));
        assert_eq!(server.take_shown_message(), None, "each is shown once");

        // Both kinds are still in the log, in the order they arrived --
        // `::bish lsp log` is the whole conversation.
        let log: Vec<&String> = server.log().collect();
        assert!(log.iter().any(|line| line.contains("window/logMessage: loading")), "{log:?}");
        assert!(log.iter().any(|line| line.contains("window/showMessage: no Cargo.toml")), "{log:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unread_shown_messages_are_bounded() {
        let dir = temp_dir("show-bounded");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        for n in 0..MAX_SHOWN * 3 {
            server.handle(Message::Notification {
                method: "window/showMessage".to_string(),
                params: Value::Object(vec![
                    ("type".to_string(), Value::Number(3.0)),
                    ("message".to_string(), Value::Str(format!("line {n}"))),
                ]),
            });
        }
        let drained = std::iter::from_fn(|| server.take_shown_message()).count();
        assert_eq!(drained, MAX_SHOWN);
        std::fs::remove_dir_all(&dir).ok();
    }

    // The declaration has to match the policy: a server told
    // `applyEdit: true` and then refused every time has been lied to,
    // and some react by disabling the feature that needed it.
    #[test]
    fn the_declared_apply_edit_capability_follows_the_policy() {
        let dir = temp_dir("apply-declared");
        for (policy, declared) in [(ApplyEdits::Scoped, true), (ApplyEdits::Always, true), (ApplyEdits::Never, false)] {
            let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, policy, Value::Object(Vec::new())).unwrap();
            let params = server.initialize_params();
            assert_eq!(json::query(&params, ".capabilities.workspace.applyEdit"), Ok(&Value::Bool(declared)), "{policy:?}");
            // Claimed unconditionally: running a command is what an
            // action with no edit of its own needs, whether or not the
            // command is then allowed to change anything.
            assert!(json::query(&params, ".capabilities.workspace.executeCommand.dynamicRegistration").is_ok());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server sends plain text unless the client says it can take a
    // snippet -- which is why a function completion arrives without its
    // parentheses from a client that never claimed this.
    #[test]
    fn snippet_support_is_declared_so_servers_actually_send_snippets() {
        let dir = temp_dir("snippet-cap");
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        let params = server.initialize_params();
        assert_eq!(json::query(&params, ".capabilities.textDocument.completion.completionItem.snippetSupport"), Ok(&Value::Bool(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Nothing may be left parked when the window closes: a server still
    // waiting on a reply to `applyEdit` can sit there indefinitely.
    #[test]
    fn edits_still_parked_when_a_command_ends_are_refused_not_dropped() {
        let dir = temp_dir("apply-leftover");
        let log = dir.join("leftover.log");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::Always, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        for id in 1..=3 {
            server.handle(Message::Request { id: Id::Number(id), method: "workspace/applyEdit".to_string(), params: Value::Null });
        }
        server.refuse_pending_applies();
        assert!(server.take_apply_edit().is_none());
        // Refusals actually sent, not merely forgotten -- read back out
        // of the fixture's own log of what arrived.
        for _ in 0..200 {
            server.service();
            if std::fs::read_to_string(&log).unwrap_or_default().matches(r#""applied":false"#).count() >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let seen = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(seen.matches(r#""applied":false"#).count(), 3, "one refusal per parked edit, in {seen}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server that asks faster than the editor applies is broken or
    // hostile; either way the answer past the cap is a refusal rather
    // than unbounded memory.
    #[test]
    fn parked_edits_are_capped() {
        let dir = temp_dir("apply-cap");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Always, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        for id in 0..(MAX_PENDING_APPLIES as i64 + 5) {
            server.handle(Message::Request { id: Id::Number(id), method: "workspace/applyEdit".to_string(), params: Value::Null });
        }
        assert_eq!(server.applies.len(), MAX_PENDING_APPLIES);
        // The oldest went, not the newest: the ones dropped are the
        // ones the editor has already had the longest to get to.
        assert_eq!(server.applies[0].0, Id::Number(5));
        std::fs::remove_dir_all(&dir).ok();
    }

    // The one sharp edge of queueing instead of blocking: a server that
    // has stopped reading its stdin will never start again, and every
    // keystroke adds another whole document to the pile.
    #[test]
    fn a_server_that_stops_reading_its_input_is_declared_broken_not_queued_forever() {
        let dir = temp_dir("wedged");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document("file:///p/x.sh", "shellscript", 1, "x");
        let big = "x".repeat(1024 * 1024);
        for version in 2..40 {
            server.change_document("file:///p/x.sh", version, &big);
            if matches!(server.state(), State::Dead(_)) {
                break;
            }
        }
        let State::Dead(why) = server.state() else {
            panic!("a wedged server should be declared dead, not queued into memory");
        };
        assert!(why.contains("stopped reading its input"), "{why}");
        assert!(server.outgoing.len() <= MAX_OUTGOING_BYTES);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Diagnostics are pushed, never requested, so after something
    // clears a buffer's list the only copy left is the one held here.
    #[test]
    fn a_publication_can_be_asked_for_again_after_something_cleared_it() {
        let dir = temp_dir("redeliver");
        let uri = "file:///p/x.sh";
        let mut server = Server::start(1, &publishing_server(uri), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document(uri, "shellscript", 1, "one");
        for _ in 0..400 {
            server.service();
            if server.take_diagnostics(uri).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(server.take_diagnostics(uri).is_none(), "collected once");
        assert!(server.redeliver_diagnostics(uri), "and available again on request");
        assert!(server.take_diagnostics(uri).is_some());
        // Nothing to redeliver for a document that never had any.
        server.open_document("file:///p/other.sh", "shellscript", 1, "x");
        assert!(!server.redeliver_diagnostics("file:///p/other.sh"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn forgetting_a_declaration_drops_its_servers_and_its_remembered_failures() {
        use crate::exec::ServiceTable;
        let dir = temp_dir("forget");
        let mut table = Table::default();
        table.get_or_start(1, &["cat".to_string()], "cat", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        assert!(table.get_or_start(2, &["bish-no-such-server".to_string()], "nope", &dir, ApplyEdits::default(), Value::Object(Vec::new())).is_err());
        assert_eq!(table.rows().len(), 2, "one running, one that never started");
        // The failure's reason is what `::bish lsp log` shows for a
        // server that produced no process to have a log of its own.
        assert!(table.logs(2).iter().any(|l| l.contains("bish-no-such-server")));

        assert_eq!(table.forget(2), 1);
        assert_eq!(table.rows().len(), 1);
        // ...and now it will be tried again rather than refused from
        // the remembered failure.
        assert!(table.get_or_start(2, &["bish-no-such-server".to_string()], "nope", &dir, ApplyEdits::default(), Value::Object(Vec::new())).is_err());
        assert_eq!(table.forget(1), 1);
        assert_eq!(table.forget(99), 0);
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

        // Nearest wins, whatever order the markers are given in --
        // the rule every mainstream client uses.
        assert_eq!(root_for(&file, &["Cargo.toml".to_string(), ".git".to_string()]), Some(crate_dir.clone()));
        assert_eq!(root_for(&file, &[".git".to_string(), "Cargo.toml".to_string()]), Some(crate_dir.clone()));
        // ...and the case marker-order got backwards: a marker high up
        // must not beat one directly above the file.
        let deep = repo.join("crates").join("inner").join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        let nested = deep.join("lib.rs");
        std::fs::write(&nested, "").unwrap();
        assert_eq!(root_for(&nested, &["Cargo.toml".to_string(), ".git".to_string()]), Some(crate_dir.clone()), "the crate directly above, not the workspace far above");
        let _ = &repo;
        // And with only the marker that isn't there, nothing does --
        // rather than falling back to a guess.
        assert_eq!(root_for(&file, &["go.mod".to_string()]), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Everything below drives a real child process. `cat` is the
    // simplest server-shaped thing on any system: it reads stdin and
    // writes stdout, so it exercises spawning, non-blocking pipes and
    // the queue without needing a language server installed.

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
        let mut server = Server::start(1, &echo_server(FULL_SYNC), "mock", dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        server
    }

    #[test]
    fn opening_a_document_tells_the_server_its_language_version_and_text() {
        let dir = temp_dir("didopen");
        let mut server = ready_echo_server(&dir);
        assert_eq!(server.sync(), Sync { open_close: true, change: true, incremental: false, save: true });

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
    // A server that publishes diagnostics the moment it is told about a
    // document -- which is what every real one does.
    fn publishing_server(uri: &str) -> Vec<String> {
        let publish = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{uri}","version":1,"diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":3}}}},"severity":2,"message":"careful"}}]}}}}"#
        );
        let init = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{FULL_SYNC}}}"#);
        let script = format!(
            "printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{init}'; sleep 0.2; printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{publish}'; sleep 30",
            init.len(),
            publish.len()
        );
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    // The scripted fixture: a real server that reads the framing and
    // replies according to what arrived. Written out from
    // `include_str!` so it exists in the test binary and nowhere else.
    fn scripted_server(dir: &Path, log: &Path) -> Vec<String> {
        let script = dir.join("lsp-mock.sh");
        std::fs::write(&script, include_str!("testdata/lsp-mock.sh")).unwrap();
        vec!["sh".to_string(), script.to_string_lossy().into_owned(), log.to_string_lossy().into_owned()]
    }

    // Everything the fixture was told, decoded with the same decoder
    // under test elsewhere -- one framed message per line.
    fn received(log: &Path) -> Vec<Message> {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        text.lines().filter_map(|line| json::parse(line).ok()).filter_map(|v| Message::from_value(&v).ok()).collect()
    }

    // One `$/progress` notification's params, as a server sends them.
    fn progress_params(json: &str) -> Value {
        json::parse(json).unwrap()
    }

    // What the status line would show for this sequence of them.
    fn progress_after(notifications: &[&str]) -> Option<String> {
        let mut progress = Vec::new();
        for text in notifications {
            apply_progress(&mut progress, &progress_params(text));
        }
        progress.last().map(|(_, p)| p.line())
    }

    #[test]
    fn a_begin_is_shown_and_its_end_clears_it() {
        let begin = r#"{"token":"rustAnalyzer/Indexing","value":{"kind":"begin","title":"Indexing"}}"#;
        let end = r#"{"token":"rustAnalyzer/Indexing","value":{"kind":"end"}}"#;
        assert_eq!(progress_after(&[begin]).as_deref(), Some("Indexing"));
        assert_eq!(progress_after(&[begin, end]), None);
    }

    #[test]
    fn a_report_updates_the_title_it_belongs_to() {
        let begin = r#"{"token":1,"value":{"kind":"begin","title":"Indexing","percentage":0}}"#;
        let report = r#"{"token":1,"value":{"kind":"report","percentage":40,"message":"core"}}"#;
        assert_eq!(progress_after(&[begin]).as_deref(), Some("Indexing 0%"));
        assert_eq!(progress_after(&[begin, report]).as_deref(), Some("Indexing 40%: core"));
    }

    #[test]
    fn a_report_that_omits_a_field_leaves_it_alone() {
        // rust-analyzer's own shape: one `begin` with a message, then
        // bare percentage bumps. Treating an absent field as a clear
        // would make the message flicker in and out on every tick.
        let begin = r#"{"token":"t","value":{"kind":"begin","title":"Indexing","message":"core","percentage":10}}"#;
        let bump = r#"{"token":"t","value":{"kind":"report","percentage":20}}"#;
        assert_eq!(progress_after(&[begin, bump]).as_deref(), Some("Indexing 20%: core"));
    }

    #[test]
    fn a_report_for_a_token_that_never_began_is_ignored() {
        // There is no title to show it under, so there is nothing to
        // say -- and inventing an empty one would put a bare "40%" on
        // the status line.
        let orphan = r#"{"token":"ghost","value":{"kind":"report","percentage":40}}"#;
        assert_eq!(progress_after(&[orphan]), None);
    }

    #[test]
    fn the_newest_operation_is_the_one_shown() {
        // Two overlapping operations is the normal case (rust-analyzer
        // indexes and builds its proc-macro server at once); the status
        // line has room for one, and the newest is the live one.
        let first = r#"{"token":"a","value":{"kind":"begin","title":"Indexing"}}"#;
        let second = r#"{"token":"b","value":{"kind":"begin","title":"Building"}}"#;
        let end_second = r#"{"token":"b","value":{"kind":"end"}}"#;
        assert_eq!(progress_after(&[first, second]).as_deref(), Some("Building"));
        // ...and ending it falls back to the one still running rather
        // than to nothing.
        assert_eq!(progress_after(&[first, second, end_second]).as_deref(), Some("Indexing"));
    }

    #[test]
    fn a_percentage_outside_the_spec_s_own_range_is_clamped() {
        let begin = r#"{"token":"t","value":{"kind":"begin","title":"Indexing","percentage":4000}}"#;
        assert_eq!(progress_after(&[begin]).as_deref(), Some("Indexing 100%"));
    }

    #[test]
    fn a_server_that_begins_without_ever_ending_does_not_grow_forever() {
        let mut progress = Vec::new();
        for token in 0..MAX_PROGRESS * 4 {
            let text = format!(r#"{{"token":{token},"value":{{"kind":"begin","title":"Work"}}}}"#);
            apply_progress(&mut progress, &progress_params(&text));
        }
        assert_eq!(progress.len(), MAX_PROGRESS);
    }

    #[test]
    fn a_notification_missing_what_it_needs_changes_nothing() {
        let mut progress = Vec::new();
        for text in [r#"{"value":{"kind":"begin","title":"Indexing"}}"#, r#"{"token":"t"}"#, r#"{"token":"t","value":{}}"#] {
            apply_progress(&mut progress, &progress_params(text));
        }
        assert!(progress.is_empty());
    }

    fn wait_for_response(server: &mut Server, id: i64) -> Option<Result<Value, ResponseError>> {
        for _ in 0..600 {
            server.service();
            if let Some(result) = server.take_response(id) {
                return Some(result);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn a_request_is_answered_and_the_reply_is_matched_to_its_own_id() {
        let dir = temp_dir("request");
        let log = dir.join("received.jsonl");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        // Capabilities the fixture declares, and one it doesn't --
        // which is what stops a request being spent on a timeout only
        // to be told the server never offered it.
        assert!(server.provides("hoverProvider"));
        assert!(server.provides("definitionProvider"));
        assert!(server.provides("referencesProvider"));
        // Deliberately a name no server will ever declare, rather than
        // one the fixture merely happens not to support yet: this
        // assertion broke three times as the fixture grew capabilities,
        // which taught it nothing except that the list had changed.
        assert!(!server.provides("bishNoSuchProvider"));

        server.open_document("file:///p/x.sh", "shellscript", 1, "echo hi");
        let id = server.request(
            "textDocument/hover",
            Value::Object(vec![
                ("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str("file:///p/x.sh".to_string()))])),
                (
                    "position".to_string(),
                    Value::Object(vec![("line".to_string(), Value::Number(0.0)), ("character".to_string(), Value::Number(1.0))]),
                ),
            ]),
        );
        assert_ne!(id, 1, "the initialize request already used id 1");
        let result = wait_for_response(&mut server, id).expect("an answer").expect("not an error");
        let lines = lsp::hover_lines(&result).expect("hover lines");
        assert_eq!(lines, vec!["echo [args...]".to_string(), String::new(), "Writes its arguments.".to_string()]);

        // Collected once: a second ask has nothing, which is what stops
        // a waiter from seeing a stale reply.
        assert!(server.take_response(id).is_none());

        // ...and the fixture recorded what bish actually sent, in order.
        let sent = received(&log);
        let methods: Vec<&str> = sent
            .iter()
            .filter_map(|m| match m {
                Message::Request { method, .. } | Message::Notification { method, .. } => Some(method.as_str()),
                Message::Response { .. } => None,
            })
            .collect();
        assert_eq!(methods, vec!["initialize", "initialized", "textDocument/didOpen", "textDocument/hover"], "{methods:?}");
        let Some(Message::Request { params, .. }) = sent.iter().find(|m| matches!(m, Message::Request { method, .. } if method == "textDocument/hover")) else {
            panic!("no hover request recorded");
        };
        assert_eq!(json::query(params, ".position.character"), Ok(&Value::Number(1.0)));
        assert_eq!(json::query(params, ".textDocument.uri"), Ok(&Value::Str("file:///p/x.sh".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A reply nobody is left waiting for must not accumulate forever.
    #[test]
    fn uncollected_replies_are_bounded() {
        let dir = temp_dir("bounded");
        let log = dir.join("received.jsonl");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document("file:///p/x.sh", "shellscript", 1, "echo hi");
        let position = Value::Object(vec![
            ("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str("file:///p/x.sh".to_string()))])),
            (
                "position".to_string(),
                Value::Object(vec![("line".to_string(), Value::Number(0.0)), ("character".to_string(), Value::Number(0.0))]),
            ),
        ]);
        let mut ids = Vec::new();
        for _ in 0..MAX_PENDING_RESPONSES + 10 {
            ids.push(server.request("textDocument/hover", position.clone()));
        }
        // Nobody collects any of them.
        let last = *ids.last().unwrap();
        wait_for_response(&mut server, last);
        assert!(server.responses.len() <= MAX_PENDING_RESPONSES, "{} kept", server.responses.len());
        // The oldest went first, so the newest is what survives.
        assert!(!server.responses.iter().any(|(id, _)| *id == ids[0]));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_publication_waits_for_whoever_holds_the_buffer_and_is_delivered_once() {
        let dir = temp_dir("publish");
        let uri = "file:///p/x.sh";
        let mut server = Server::start(1, &publishing_server(uri), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document(uri, "shellscript", 1, "one");

        for _ in 0..400 {
            server.service();
            if server.take_diagnostics(uri).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Consumed above; a second ask has nothing new to report, which
        // is what keeps the editor from repainting on every idle tick.
        assert!(server.take_diagnostics(uri).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_publication_for_an_unknown_document_is_dropped() {
        let dir = temp_dir("unknown");
        let mut server = Server::start(1, &publishing_server("file:///p/never-opened.sh"), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        for _ in 0..200 {
            server.service();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Nothing here is about any buffer, so nothing is kept.
        assert!(server.take_diagnostics("file:///p/x.sh").is_none());
        assert!(server.take_diagnostics("file:///p/never-opened.sh").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_server_that_wants_no_documents_is_told_about_none() {
        let dir = temp_dir("nosync");
        let capabilities = r#"{"capabilities":{"textDocumentSync":{"openClose":false,"change":0,"save":false}}}"#;
        let mut server = Server::start(1, &echo_server(capabilities), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert_eq!(server.sync(), Sync { open_close: false, change: false, incremental: false, save: false });

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
        assert_eq!(Sync::parse(&number), Sync { open_close: true, change: true, incremental: true, save: false }, "kind 2 is the incremental one");
        let full = json::parse(r#"{"textDocumentSync":1}"#).unwrap();
        assert_eq!(Sync::parse(&full), Sync { open_close: true, change: true, incremental: false, save: false });
        let none = json::parse(r#"{"textDocumentSync":0}"#).unwrap();
        assert_eq!(Sync::parse(&none), Sync { open_close: true, change: false, incremental: false, save: false });
        // `save` may be a bool or an options object; both mean yes.
        let object = json::parse(r#"{"textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":true}}}"#).unwrap();
        assert_eq!(Sync::parse(&object), Sync { open_close: true, change: true, incremental: false, save: true });
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
        let mut server = Server::start(1, &command, "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
        let mut server = Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        assert_eq!(server.encoding(), PositionEncoding::Utf16);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn work_queued_during_the_handshake_goes_out_once_it_finishes() {
        let dir = temp_dir("release");
        let mut server = Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), script], "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        let State::Dead(why) = server.state() else { panic!("still {:?}", server.state()) };
        assert!(why.contains("unsupported client"), "{why}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn starting_a_server_sends_initialize_and_leaves_it_initializing() {
        let dir = temp_dir("start");
        let log = dir.join("received.jsonl");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        // Before anything is serviced: the process exists, the
        // handshake has not happened.
        assert_eq!(*server.state(), State::Initializing);
        assert!(!server.is_ready());
        run_until_ready(&mut server);

        // What actually went out, read back from the fixture's own
        // record rather than from an echo -- requests are answered
        // inside `service` now, so they never reach a caller.
        let sent = received(&log);
        let Some(Message::Request { method, params, .. }) = sent.first() else {
            panic!("expected initialize first, got {sent:?}");
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
        let log = dir.join("received.jsonl");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        server.request("textDocument/hover", Value::Null);
        server.notify("textDocument/didOpen", Value::Null);
        assert_eq!(server.queued.len(), 2, "both should be waiting for `initialized`");
        run_until_ready(&mut server);
        for _ in 0..100 {
            server.service();
            if received(&log).len() >= 4 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The property that matters is the *order* on the wire: nothing
        // may precede `initialized` except `initialize` itself, or the
        // server is entitled to reject it.
        let methods: Vec<String> = received(&log)
            .into_iter()
            .filter_map(|m| match m {
                Message::Request { method, .. } | Message::Notification { method, .. } => Some(method),
                Message::Response { .. } => None,
            })
            .collect();
        assert_eq!(methods.first().map(String::as_str), Some("initialize"));
        assert_eq!(methods.get(1).map(String::as_str), Some("initialized"));
        assert!(methods[2..].contains(&"textDocument/hover".to_string()), "{methods:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_command_that_does_not_exist_fails_to_start_rather_than_hanging() {
        let dir = temp_dir("missing");
        let Err(error) = Server::start(1, &["bish-no-such-language-server".to_string()], "x", &dir, ApplyEdits::default(), Value::Object(Vec::new())) else {
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
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), script.to_string()], "sh", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
        let mut server = Server::start(1, &["sh".to_string(), "-c".to_string(), "exit 0".to_string()], "sh", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
