// One running language server: the process, its pipes, the handshake,
// and the documents it's been told about. The other half of `lsp.rs`
// (the wire format). No `Shell`, `SessionState` or `Rc<RefCell<_>>`, so
// it can be driven from `service_background_jobs`, the idle callback
// every blocking loop in repl.rs calls.
//
// No threads: both pipes are non-blocking, outgoing bytes queue and
// drain a bit per tick, and a server that stops reading costs a growing
// queue (capped) rather than a stuck loop. A writer thread would force
// `Arc<Mutex<_>>` on everything it touched.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use crate::json::{self, Value};
use crate::lsp::{self, Id, Message, PositionEncoding, ResponseError};
use crate::url;

// Bounded, like `drive_fg_job`'s read loop: a server writing faster than
// we read mustn't hold this tick, which every pane's redraw waits on.
const MAX_READS_PER_TICK: u32 = 16;

// How much stderr to keep: servers log freely, but the last screenful
// often explains a failed start.
const LOG_LINES: usize = 200;

// How many uncollected responses to keep. A timed-out request may be
// answered much later with nobody to collect it, so this caps the
// debris; a real conversation has one or two in flight.
const MAX_PENDING_RESPONSES: usize = 32;

// How many bytes may be owed to a server's stdin before it's declared
// broken. Queueing instead of blocking keeps this single-threaded, but a
// server that stopped reading never starts again and each keystroke
// queues a whole document. Room for a large `didOpen` and a burst of
// edits; small enough to notice a wedge early.
const MAX_OUTGOING_BYTES: usize = 8 * 1024 * 1024;

/// Where a server is in its lifecycle. During `Initializing` a server
/// may reject anything but `initialize`, so requests are held, not sent
/// and lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Initializing,
    Ready,
    /// With the reason, shown by `::bish lsp status`. A dead server stays
    /// dead: one that fails at startup fails the same way every time, and a
    /// retry loop would only log it repeatedly.
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

/// How a server wants documents synchronized, from its
/// `textDocumentSync` capability: `openClose: false` means it tracks no
/// documents, and a `didOpen` per visited file would be noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sync {
    pub open_close: bool,
    pub change: bool,
    /// Kind 2: only the changed ranges, not the whole file each time.
    pub incremental: bool,
    pub save: bool,
}

impl Default for Sync {
    // When the server said nothing. The spec's default is no sync, which
    // would make a server that forgot to describe itself look broken in a
    // way that's hard to see; full sync fails the other way, with
    // notifications it can ignore.
    fn default() -> Sync {
        // Full, not incremental: a server that didn't describe itself is
        // unlikely to want ranges.
        Sync { open_close: true, change: true, incremental: false, save: false }
    }
}

impl Sync {
    /// `textDocumentSync` is a kind number or an options object; both are
    /// common.
    fn parse(capabilities: &Value) -> Sync {
        match json::query(capabilities, ".textDocumentSync") {
            // The number form names only the change kind; open and close are
            // implied, as changes need something to be relative to.
            Ok(Value::Number(kind)) => Sync { open_close: true, change: *kind > 0.0, incremental: *kind == 2.0, save: false },
            Ok(value @ Value::Object(_)) => Sync {
                // The object form's fields all default to false, per the
                // spec: a server using it describes itself exactly.
                open_close: matches!(json::query(value, ".openClose"), Ok(Value::Bool(true))),
                change: matches!(json::query(value, ".change"), Ok(Value::Number(k)) if *k > 0.0),
                incremental: matches!(json::query(value, ".change"), Ok(Value::Number(k)) if *k == 2.0),
                // `true` or `{ includeText: bool }`; both ask about saves.
                save: matches!(json::query(value, ".save"), Ok(Value::Bool(true)) | Ok(Value::Object(_))),
            },
            _ => Sync::default(),
        }
    }
}

// One document this server has been told about.
struct Document {
    uri: String,
    // The revision last sent, not the buffer's current one; the gap is what
    // `needs_change` asks about.
    version: u64,
    // When this document first differed from what the server knows, or
    // `None`. The debounce runs from here, not the last keystroke, so
    // continuous typing still sends a `didChange` after a bounded delay.
    pending_since: Option<Instant>,
    // The text last sent, kept only under incremental sync: the next change
    // is a difference from it, which a `TextBuffer` can't supply. Empty
    // under full sync.
    sent_text: String,
    // The newest `publishDiagnostics` for this document and whether it's
    // been applied. Kept because diagnostics arrive on the idle tick, which
    // has no buffer, and the editor loop applies them, so one for an
    // undriven pane waits. Only the newest: a publication replaces the
    // findings.
    published: Option<lsp::Publication>,
    unapplied: bool,
}

pub struct Server {
    /// The `exec::LspServer` declaration this was started from.
    pub id: u64,
    /// For display: the command line, as registered.
    pub command: String,
    /// The project root. With `command` it identifies the server: one per
    /// (command, root), shared by every pane in that project.
    pub root: PathBuf,

    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,

    decoder: lsp::Decoder,
    // Bytes owed to the server's stdin. A non-blocking pipe takes what it
    // has room for, so this drains over as many ticks as needed.
    outgoing: VecDeque<u8>,
    // Messages held until the handshake finishes.
    queued: Vec<Message>,

    log: VecDeque<String>,
    stderr_partial: String,
    // Work the server says it's doing, newest last, keyed by the
    // `$/progress` token that ends it. A server still indexing answers
    // nothing, and each `K` or `gd` would cost a timeout with no
    // explanation; one that reports progress says so. Bounded, since a
    // server that never sends `end` (or crashes) mustn't grow this.
    progress: Vec<(String, Progress)>,
    // `window/showMessage` lines not yet shown. Apart from `log`
    // (`window/logMessage` and stderr): a log line is for someone who looks,
    // a show line is the server saying "tell them", most often that it
    // couldn't load a project.
    shown: VecDeque<String>,
    // This server's configuration, nested (`settings_tree`). An empty
    // object without `--setting`, meaning defaults wherever it's read.
    settings: Value,

    state: State,
    // Responses to bish's requests, waiting to be collected. A `Vec`, since
    // it's also where staleness is bounded: an answer to a timed-out request
    // is never collected, so the oldest go past a handful.
    responses: Vec<(i64, Result<Value, ResponseError>)>,
    // Set at stdout EOF, which for an exiting server comes a moment before
    // its exit is reapable. Counted, not acted on, so `check_alive` can
    // report the exit status: "exited with status 3" is a diagnosis,
    // "closed its stdout" a symptom.
    stdout_eof: bool,
    eof_ticks: u32,
    // When a stdin write found nobody reading: nearly always a server on its
    // way out, so this waits for its exit and reason.
    stdin_broken_at: Option<Instant>,
    next_id: i64,
    initialize_id: i64,
    encoding: PositionEncoding,
    capabilities: Value,
    sync: Sync,
    documents: Vec<Document>,
    apply_edits: ApplyEdits,
    // Ids of `workspace/executeCommand` requests awaiting a response:
    // non-empty means a user-chosen command is running, for
    // `ApplyEdits::Scoped`.
    commands: Vec<i64>,
    // Server-requested edits accepted by policy and not yet applied. Left
    // unanswered: the reply says whether the edit landed, which only the
    // editor knows (`take_apply_edit`).
    applies: Vec<(Id, Value)>,
}

/// How far a server may change files without being asked.
/// `workspace/applyEdit` is a request the server makes when it likes:
/// that's how command-style code actions work (the edit comes back as a
/// separate request), but nothing confines it to then. So this is a
/// policy, per declared server: one you wrote yesterday and a large
/// distribution one don't deserve the same latitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApplyEdits {
    /// Accept only while a user-chosen command is running: from
    /// `workspace/executeCommand` going out until its response, the span a
    /// server needs to do what was picked. The default.
    #[default]
    Scoped,
    /// Refuse always.
    Never,
    /// Accept whenever asked.
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

/// The most server-requested edits held unanswered: past this, a refusal
/// rather than unbounded memory.
const MAX_PENDING_APPLIES: usize = 8;

/// The configuration object for a server, from `::bish lsp add
/// --setting`'s flat `dotted.key=value` pairs:
/// `rust-analyzer.check.command=clippy` becomes
/// `{"rust-analyzer":{"check":{"command":"clippy"}}}`, the shape
/// `workspace/configuration` and `didChangeConfiguration` use.
///
/// A value is JSON if it parses and a string otherwise: `enable=true` is
/// a boolean, `procMacro.attributes=["a"]` an array, `path=/usr/bin/foo` a
/// string. When one key is a prefix of another (`a` can't be a number and
/// an object), the later wins, as for a repeated key.
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

// The subtree a `workspace/configuration` section names, or `Null` for
// nothing set ("use your defaults"). Walked rather than `json::query`:
// a section is literal keys split on `.`, where in a path `[` and a
// leading `.` mean something.
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

// Walks `path`, creating objects, and puts `value` at its end.
fn insert_at(node: &mut Value, path: &[&str], value: Value) {
    let Value::Object(fields) = node else {
        // A leaf where an object is needed: the later key wins, so it becomes
        // an object.
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

/// The most concurrent `$/progress` operations tracked. Only the newest
/// shows, so this bounds memory against servers that never end work.
const MAX_PROGRESS: usize = 16;

/// The most unread `window/showMessage` lines held at once.
const MAX_SHOWN: usize = 8;

/// A `$/progress` operation begun and not ended. `title` is fixed at
/// `begin`; `message` and `percentage` are the last `report`'s, and may be
/// absent throughout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Progress {
    title: String,
    message: Option<String>,
    percentage: Option<u8>,
}

impl Progress {
    // "Indexing 40%: core", parts omitted when not given. Short: it shares
    // the status line with the mode and cursor position, and a `message`
    // can be a whole crate path.
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
    /// Spawns `command` in `root` and sends `initialize`, returning once the
    /// process exists; the handshake completes later in `service`, so the
    /// server starts `Initializing`.
    pub fn start(id: u64, command: &[String], display: &str, root: &Path, apply_edits: ApplyEdits, settings: Value) -> Result<Server, String> {
        if !cfg!(feature = "lsp") {
            return Err("language servers are not part of this build".to_string());
        }
        let Some((program, args)) = command.split_first() else {
            return Err("no command to run".to_string());
        };
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped: a server that fails to start explains on stderr, and
            // discarding it leaves a dead server with no reason. Unread, a
            // chatty server would fill the pipe and block, so
            // `drain_stderr` runs every tick.
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;
        // Every pipe non-blocking, stdin included (see the module doc). Only
        // where a child can be spawned; elsewhere the spawn already failed.
        #[cfg(unix)]
        {
            crate::pty::set_nonblocking(stdout.as_raw_fd());
            crate::pty::set_nonblocking(stderr.as_raw_fd());
            crate::pty::set_nonblocking(stdin.as_raw_fd());
        }

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
            stdin_broken_at: None,
            next_id: 1,
            initialize_id: 0,
            // The protocol's assumption until negotiated; `initialize`
            // usually agrees on `utf-32`.
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
            ("processId".to_string(), Value::Number(crate::platform::process_id() as f64)),
            (
                "clientInfo".to_string(),
                Value::Object(vec![
                    ("name".to_string(), Value::Str("bish".to_string())),
                    ("version".to_string(), Value::Str(env!("CARGO_PKG_VERSION").to_string())),
                ]),
            ),
            // `rootUri` is deprecated for `workspaceFolders`, but every
            // server still honours it, and one root per server is bish's
            // whole model (see `exec::LspServer`).
            ("rootUri".to_string(), Value::Str(uri.clone())),
            ("rootPath".to_string(), Value::Str(self.root.to_string_lossy().into_owned())),
            (
                "capabilities".to_string(),
                Value::Object(vec![
                    // `utf-32` is bish's own column counting, so agreeing
                    // on it means no conversion and no off-by-one after an
                    // emoji.
                    ("general".to_string(), Value::Object(vec![("positionEncodings".to_string(), Value::Array(encodings))])),
                    (
                        "textDocument".to_string(),
                        Value::Object(vec![
                            (
                                "synchronization".to_string(),
                                Value::Object(vec![
                                    // No dynamic registration: there's no way
                                    // to honour it.
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    // `willSave` is declined: it only matters
                                    // with `willSaveWaitUntil`, which lets a
                                    // server rewrite the buffer before the
                                    // write, and `:w` mustn't hang on a
                                    // language server.
                                    ("willSave".to_string(), Value::Bool(false)),
                                    ("willSaveWaitUntil".to_string(), Value::Bool(false)),
                                    ("didSave".to_string(), Value::Bool(true)),
                                ]),
                            ),
                            (
                                "codeAction".to_string(),
                                Value::Object(vec![
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    // The literal form: a server may answer
                                    // with `CodeAction` objects, which carry
                                    // edits, not only bare `Command`s.
                                    (
                                        "codeActionLiteralSupport".to_string(),
                                        Value::Object(vec![(
                                            "codeActionKind".to_string(),
                                            Value::Object(vec![("valueSet".to_string(), Value::Array(Vec::new()))]),
                                        )]),
                                    ),
                                    // Actions may come without edits:
                                    // `codeAction/resolve` fetches the chosen
                                    // one's, which spares rust-analyzer
                                    // computing every refactor.
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
                                    // No `prepareSupport`: bish doesn't ask
                                    // whether a rename is legal before
                                    // offering one.
                                    ("prepareSupport".to_string(), Value::Bool(false)),
                                ]),
                            ),
                            ("formatting".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            (
                                "documentSymbol".to_string(),
                                Value::Object(vec![
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    // The nested form, for an indented
                                    // outline; flat-only servers still work
                                    // (see `lsp::symbols`).
                                    ("hierarchicalDocumentSymbolSupport".to_string(), Value::Bool(true)),
                                ]),
                            ),
                            (
                                "semanticTokens".to_string(),
                                Value::Object(vec![
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    // Only the whole document: the delta and
                                    // range forms make large files cheap, and
                                    // nothing waits on tokens, which are
                                    // fetched off the idle path.
                                    (
                                        "requests".to_string(),
                                        Value::Object(vec![("full".to_string(), Value::Bool(true)), ("range".to_string(), Value::Bool(false))]),
                                    ),
                                    // Both empty: a server sends whatever
                                    // legend it likes, and bish reads the
                                    // names from it (`lsp::SemanticLegend`); a
                                    // fixed list would refuse types this
                                    // client could colour, `::bish hl`'s
                                    // namespace being open.
                                    ("tokenTypes".to_string(), Value::Array(Vec::new())),
                                    ("tokenModifiers".to_string(), Value::Array(Vec::new())),
                                    ("formats".to_string(), Value::Array(vec![Value::Str("relative".to_string())])),
                                    // Neither: bish paints one colour per
                                    // token, and both would need renderer
                                    // work.
                                    ("multilineTokenSupport".to_string(), Value::Bool(false)),
                                    ("overlappingTokenSupport".to_string(), Value::Bool(false)),
                                ]),
                            ),
                            ("references".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // "Where else in this file is what I'm on."
                            ("documentHighlight".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // Parameter names and inferred types inline. No
                            // `resolveSupport`: a label arriving after a
                            // second round trip would appear a beat late,
                            // and servers answer completely when not asked
                            // to defer.
                            ("inlayHint".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // The signature of the call being typed. No
                            // `contextSupport`: bish sends only the
                            // position.
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
                            ("definition".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // The questions beside it: `gy`, `gD`, and
                            // `textDocument/implementation`, which has no
                            // key yet (see vimkeys' note on `gi`). Declared
                            // anyway: a server reads what the client can
                            // do, not what it binds.
                            ("typeDefinition".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            ("implementation".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            ("declaration".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            (
                                "completion".to_string(),
                                Value::Object(vec![
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    (
                                        "completionItem".to_string(),
                                        Value::Object(vec![
                                            // True now: a snippet completion is
                                            // inserted live with a caret in its first
                                            // tabstop (`bishedit::snippet`). Without
                                            // it a server sends plain text, which is
                                            // why rust-analyzer's function completions
                                            // lack their parentheses.
                                            ("snippetSupport".to_string(), Value::Bool(true)),
                                            // No resolve round trip, so a server must
                                            // send everything up front.
                                            ("resolveSupport".to_string(), Value::Object(vec![("properties".to_string(), Value::Array(Vec::new()))])),
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
                                    // Markdown first, where servers put their
                                    // best answer; plaintext is accepted, and
                                    // both are flattened to lines.
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
                        Value::Object(vec![
                            (
                                "workspaceEdit".to_string(),
                                Value::Object(vec![
                                    // What bish can carry out: an edit may
                                    // create, rename and delete files, and a
                                    // failed step undoes those before it, so a
                                    // project is never left half-changed
                                    // (`apply_workspace_edit`).
                                    ("documentChanges".to_string(), Value::Bool(true)),
                                    (
                                        "resourceOperations".to_string(),
                                        Value::Array(["create", "rename", "delete"].iter().map(|kind| Value::Str(kind.to_string())).collect()),
                                    ),
                                    ("failureHandling".to_string(), Value::Str("undo".to_string())),
                                ]),
                            ),
                            // Matches the policy: a server told
                            // `applyEdit: true` and then always refused has
                            // been lied to, and some disable the feature
                            // anyway. Under `never` the honest claim is no.
                            ("applyEdit".to_string(), Value::Bool(self.apply_edits != ApplyEdits::Never)),
                            // So a server knows a push is coming and
                            // needn't poll (see the
                            // `didChangeConfiguration` sent after
                            // `initialized`).
                            ("didChangeConfiguration".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // "What are this client's settings for these
                            // sections?"
                            ("configuration".to_string(), Value::Bool(true)),
                            // The other half of a command-style code
                            // action: running the command it names.
                            ("executeCommand".to_string(), Value::Object(vec![("dynamicRegistration".to_string(), Value::Bool(false))])),
                            // `:sym`, the project-wide half of `gO`.
                            (
                                "symbol".to_string(),
                                Value::Object(vec![
                                    ("dynamicRegistration".to_string(), Value::Bool(false)),
                                    // No `resolveSupport`: a `WorkspaceSymbol`
                                    // with only a uri would need a second
                                    // round trip to find where in the file,
                                    // leaving the list empty meanwhile.
                                    ("resolveSupport".to_string(), Value::Object(vec![("properties".to_string(), Value::Array(Vec::new()))])),
                                ]),
                            ),
                        ]),
                    ),
                    (
                        "window".to_string(),
                        Value::Object(vec![
                            // rust-analyzer and others send `$/progress`
                            // only to clients that ask, so without this a
                            // cold server looks the same as a broken one.
                            ("workDoneProgress".to_string(), Value::Bool(true)),
                        ]),
                    ),
                ]),
            ),
        ])
    }

    /// Moves ready bytes both ways and returns the messages the caller must
    /// handle; the handshake is handled here. Non-blocking, once per idle
    /// tick, where job ptys are drained.
    pub fn service(&mut self) -> Vec<Message> {
        if matches!(self.state, State::Dead(_)) {
            return Vec::new();
        }
        // Reads before the liveness check: a dying server may have said why.
        self.drain_stderr();
        self.drain_stdout();
        self.check_alive();
        // A server that closed stdout without exiting will never answer, so
        // it's dead either way, but its exit gets a tick to arrive.
        if self.stdout_eof && !matches!(self.state, State::Dead(_)) {
            self.eof_ticks += 1;
            if self.eof_ticks > 1 {
                self.die("server closed its stdout".to_string());
            }
        }
        // A broken stdin with no exit a second later: a server that stopped
        // listening and kept running. An exiting one is collected by
        // `check_alive` long before this.
        const STDIN_GRACE: Duration = Duration::from_secs(1);
        if let Some(broken) = self.stdin_broken_at
            && broken.elapsed() > STDIN_GRACE
            && !matches!(self.state, State::Dead(_))
        {
            self.die("server closed its stdin".to_string());
        }
        self.flush_outgoing();

        let mut incoming = Vec::new();
        loop {
            match self.decoder.take_message() {
                None => break,
                Some(Err(why)) => {
                    self.note(format!("protocol error: {why}"));
                    // A bad body leaves the stream synchronized: logged and
                    // skipped. A framing error loses our place, and there's
                    // nothing to do but stop.
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
        // Again at the end: the `initialize` response releases `initialized`
        // and the queue, which needn't wait a tick.
        self.flush_outgoing();
        incoming
    }

    // The message if the caller should see it; `None` for this module's
    // own business.
    fn handle(&mut self, message: Message) -> Option<Message> {
        match &message {
            Message::Response { id: Id::Number(id), result } if *id == self.initialize_id && self.state == State::Initializing => {
                match result {
                    Err(e) => self.die(format!("initialize failed: {} ({})", e.message, e.code)),
                    Ok(result) => {
                        self.capabilities = json::query(result, ".capabilities").cloned().unwrap_or(Value::Null);
                        self.encoding = match json::query(&self.capabilities, ".positionEncoding") {
                            Ok(Value::Str(name)) => PositionEncoding::from_wire_name(name).unwrap_or(PositionEncoding::Utf16),
                            // Silence means UTF-16, per the spec: the
                            // default for servers that predate the
                            // negotiation.
                            _ => PositionEncoding::Utf16,
                        };
                        self.sync = Sync::parse(&self.capabilities);
                        self.state = State::Ready;
                        self.send_now(Message::Notification { method: "initialized".to_string(), params: Value::Object(Vec::new()) });
                        // Right after `initialized`, for a server given
                        // settings. Both are needed:
                        // `workspace/configuration` is a pull, answered
                        // whenever the server asks (some never do); this
                        // push makes settings apply at startup.
                        if !matches!(&self.settings, Value::Object(fields) if fields.is_empty()) {
                            let params = Value::Object(vec![("settings".to_string(), self.settings.clone())]);
                            self.send_now(Message::Notification { method: "workspace/didChangeConfiguration".to_string(), params });
                        }
                        for message in std::mem::take(&mut self.queued) {
                            self.send_now(message);
                        }
                        self.note(format!("ready, position encoding {}", self.encoding.wire_name()));
                        // In the log rather than `ls`, whose tab-separated
                        // columns are for scripts: the one place to check
                        // what was actually sent.
                        if !matches!(&self.settings, Value::Object(fields) if fields.is_empty()) {
                            self.note(format!("configuration: {}", json::compact_print(&self.settings)));
                        }
                    }
                }
                None
            }
            // A reply to bish's request, kept for whoever waits on that id,
            // several frames up the stack.
            Message::Response { id: Id::Number(id), result } => {
                if self.responses.len() >= MAX_PENDING_RESPONSES {
                    // Dropping the oldest uncollected reply, which nobody
                    // still awaits.
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
                // A publication for a document this server wasn't told
                // about, or one closed, has no buffer and is dropped.
                None
            }
            // An edit the policy allows isn't answered here: the reply says
            // whether it applied, which this layer can't do. Parked for the
            // editor, which answers it.
            Message::Request { id, method, params } if method == "workspace/applyEdit" && self.may_apply() => {
                if self.applies.len() >= MAX_PENDING_APPLIES {
                    let (id, _) = self.applies.remove(0);
                    self.note("refused workspace/applyEdit: too many still waiting to be applied".to_string());
                    self.respond(id, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])));
                }
                self.applies.push((id.clone(), params.clone()));
                None
            }
            // A request from the server must get a response, or it may stall;
            // `answer` gives the reply a client with nothing to offer should.
            Message::Request { id, method, params } => {
                let (answer, note) = self.answer(method, params);
                if let Some(note) = note {
                    self.note(note);
                }
                self.respond(id.clone(), answer);
                None
            }
            // "Started / partway / finished" a piece of work: tracked
            // rather than logged, so it shows while true.
            Message::Notification { method, params } if method == "$/progress" => {
                self.progress_update(params);
                None
            }
            // A server's log lines go with its stderr, not to the caller.
            Message::Notification { method, params } if method == "window/logMessage" || method == "window/showMessage" => {
                if let Ok(Value::Str(text)) = json::query(params, ".message") {
                    self.note(format!("{method}: {text}"));
                    // A `showMessage` is also queued for the user; the log
                    // keeps its copy, so `::bish lsp log` shows everything
                    // in order.
                    if method == "window/showMessage" {
                        let severity = match json::query(params, ".type") {
                            Ok(Value::Number(n)) if *n == 1.0 => "error",
                            Ok(Value::Number(n)) if *n == 2.0 => "warning",
                            // 3 (info), 4 (log) and inventions: no label
                            // rather than a guess.
                            _ => "",
                        };
                        if self.shown.len() >= MAX_SHOWN {
                            // Dropping the oldest: a server outpacing the
                            // reader repeats itself, and the newest is most
                            // current.
                            self.shown.pop_front();
                        }
                        let name = self.display_name().to_string();
                        self.shown.push_back(if severity.is_empty() { format!("{name}: {text}") } else { format!("{name} {severity}: {text}") });
                    }
                }
                None
            }
            _ => Some(message),
        }
    }

    // The reply to a request the server made, and maybe a log line. The
    // default is `MethodNotFound`, the true answer for an unimplemented
    // method; the exceptions are requests where a real answer is more
    // accurate and refusing would degrade the server.
    fn answer(&self, method: &str, params: &Value) -> (Result<Value, ResponseError>, Option<String>) {
        match method {
            // "What are this client's settings for these sections?",
            // answered from `--setting`, with `null` (use your defaults)
            // for anything unset, not "unknown method", which makes servers
            // log errors or disable features.
            "workspace/configuration" => {
                let Ok(Value::Array(items)) = json::query(params, ".items") else {
                    return (Ok(Value::Array(Vec::new())), None);
                };
                // One answer per item, in order: replies match
                // positionally, so a short array is a protocol error.
                let answers = items
                    .iter()
                    .map(|item| match json::query(item, ".section") {
                        // A section is a dotted path into the settings, as
                        // `--setting` takes.
                        Ok(Value::Str(section)) => section_of(&self.settings, section),
                        // No section means everything.
                        _ => self.settings.clone(),
                    })
                    .collect();
                (Ok(Value::Array(answers)), None)
            }
            // "May I report progress?" Yes: refusing can stop a server
            // reporting indexing it would otherwise recover from.
            "window/workDoneProgress/create" => (Ok(Value::Null), None),
            // Dynamic registration. bish declares
            // `dynamicRegistration: false`, so a conforming server never
            // asks; accepting an off-spec request is what other clients do
            // and harmless, while a refusal can be treated as fatal.
            // Logged, since it's unexpected.
            "client/registerCapability" | "client/unregisterCapability" => {
                (Ok(Value::Null), Some(format!("accepted and ignored {method}: bish declares no dynamic registration")))
            }
            // "Apply these edits": reached only when the policy says no
            // (see `handle`). `applied: false` is a refusal in the server's
            // terms, so it can say the refactor didn't happen.
            "workspace/applyEdit" => (
                Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])),
                Some(match self.apply_edits {
                    ApplyEdits::Never => "refused workspace/applyEdit: --apply-edits=never".to_string(),
                    _ => "refused workspace/applyEdit: no command of yours was running".to_string(),
                }),
            ),
            // A message with buttons, never displayed, so nothing was
            // chosen: `null` is the spec's "dismissed".
            "window/showMessageRequest" => (Ok(Value::Null), None),
            // bish sends one `rootUri` and claims no workspace folders.
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
                // A server closing stderr while still talking on stdout is
                // legal; not this half's call.
                Ok(0) => return,
                Ok(n) => {
                    self.stderr_partial.push_str(&String::from_utf8_lossy(&buf[..n]));
                    while let Some(newline) = self.stderr_partial.find('\n') {
                        let line: String = self.stderr_partial.drain(..=newline).collect();
                        self.note(line.trim_end().to_string());
                    }
                    // One enormous unterminated line mustn't grow this
                    // without bound.
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
                // The pipe is full because the server is busy: the bytes
                // wait for a later tick, which is what a writer thread
                // would otherwise handle.
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                // Nobody is reading: the server closed its stdin, almost
                // always while exiting. Its exit status and last words are
                // a tick away and are the reason worth reporting, so stop
                // writing and let `service` wait.
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    self.stdin_broken_at.get_or_insert_with(Instant::now);
                    self.outgoing.clear();
                    return;
                }
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
                // Its last line is usually the reason, so it goes in the
                // reason, not only the log.
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
        if self.applies.is_empty() { None } else { Some(self.applies.remove(0)) }
    }

    /// Answers one edit taken with `take_apply_edit`.
    pub fn reply_to_apply(&mut self, id: Id, applied: bool) {
        self.respond(id, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(applied))])));
    }

    /// Refuses everything parked, when the window closes (a command
    /// finished or gave up), so no server waits forever.
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

    /// Answers a request the server made, echoing the id exactly, which is
    /// why `Id` keeps its string case.
    pub fn respond(&mut self, id: Id, result: Result<Value, ResponseError>) {
        self.send(Message::Response { id, result });
    }

    fn send(&mut self, message: Message) {
        match self.state {
            State::Dead(_) => {}
            State::Ready => self.send_now(message),
            // Sending now would be ignored or answered with an error, and
            // the caller wouldn't learn its request went nowhere.
            State::Initializing => self.queued.push(message),
        }
    }

    fn send_now(&mut self, message: Message) {
        self.enqueue(&message);
    }

    fn enqueue(&mut self, message: &Message) {
        let encoded = lsp::encode(message);
        if self.outgoing.len() + encoded.len() > MAX_OUTGOING_BYTES {
            // A server this far behind on reading is wedged; failing
            // visibly beats a leak.
            self.die(format!("stopped reading its input ({} bytes queued)", self.outgoing.len()));
            return;
        }
        self.outgoing.extend(encoded);
    }

    /// Tells the server about a document it hasn't seen. A no-op if it
    /// already knows, or if it said it doesn't track documents.
    pub fn open_document(&mut self, uri: &str, language_id: &str, version: u64, text: &str) {
        if !self.sync.open_close || self.has_document(uri) {
            return;
        }
        // Recorded before sending: `send` holds it until the handshake, and a
        // second open meanwhile mustn't queue a duplicate.
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

    /// Whether this document changed since the server was told, and has
    /// settled long enough to tell it. Asked every idle tick, separately from
    /// `change_document`, since producing the text walks the whole buffer.
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

    /// The version of `uri` the server has been told about; `None` if never.
    /// An answer about text the server hasn't seen describes a different
    /// file, so anything positioned by an answer checks this first.
    pub fn known_version(&self, uri: &str) -> Option<u64> {
        self.documents.iter().find(|d| d.uri == uri).map(|d| d.version)
    }

    /// The answer to `id`, once it has arrived; `None` until then, while the
    /// caller polls and services everything else.
    pub fn take_response(&mut self, id: i64) -> Option<Result<Value, ResponseError>> {
        let at = self.responses.iter().position(|(pending, _)| *pending == id)?;
        Some(self.responses.remove(at).1)
    }

    /// Whether the server said it answers `capability`. A bare `true` or an
    /// options object both mean yes; anything else no, since asking anyway
    /// would spend a timeout to learn it.
    pub fn provides(&self, capability: &str) -> bool {
        matches!(json::query(&self.capabilities, &format!(".{capability}")), Ok(Value::Bool(true)) | Ok(Value::Object(_)))
    }

    /// The newest diagnostics for this document if not yet applied, marking
    /// them applied; `None` almost every tick, the cheap check before any
    /// work. Kept, so they can be applied again after an edit clears the
    /// buffer's list.
    pub fn take_diagnostics(&mut self, uri: &str) -> Option<&lsp::Publication> {
        let document = self.documents.iter_mut().find(|d| d.uri == uri)?;
        if !document.unapplied {
            return None;
        }
        document.unapplied = false;
        document.published.as_ref()
    }

    /// Marks `uri`'s last diagnostics as needing to be applied again.
    /// Diagnostics are pushed, never requested, so after `:diag clear` the
    /// only copy is here, and `:diag` restores it.
    pub fn redeliver_diagnostics(&mut self, uri: &str) -> bool {
        let Some(document) = self.documents.iter_mut().find(|d| d.uri == uri) else {
            return false;
        };
        document.unapplied = document.published.is_some();
        document.unapplied
    }

    /// Sends the current text. A no-op when the server has this version, so
    /// `save_document` can flush unconditionally without sending a settled
    /// document twice.
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
            // `None` means the diff wasn't worth it (see `line_changes`):
            // the whole file goes, always a legal `didChange`.
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
        // `text` is always included: a server that didn't ask for it must
        // ignore it, and it costs one copy of a file just saved, against a
        // server that wanted it working from stale text.
        self.notify(
            "textDocument/didSave",
            Value::Object(vec![
                ("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string()))])),
                ("text".to_string(), Value::Str(text.to_string())),
            ]),
        );
    }

    /// Tells the server the document is gone, and forgets it; its
    /// diagnostics are then the server's to withdraw, which is why this is
    /// sent while the buffer still exists.
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

    /// The protocol's goodbye, `shutdown` then `exit`, queued and flushed
    /// once; `Drop` makes sure the process is gone.
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

    // Applies one `$/progress` notification. A token is a string or a number
    // and opaque, only matched against an `end`, so both are kept as text.
    // A `report` for a token never begun is ignored: without the `begin`
    // there's no title to show.
    fn progress_update(&mut self, params: &Value) {
        apply_progress(&mut self.progress, params);
    }
}

// `Server::progress_update`'s body as a free function, testable without
// spawning a process.
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
    // Clamped: the spec says 0-100, and "Indexing 4000%" is worse than
    // nothing.
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
                // The oldest is least likely still running.
                let _ = progress.remove(0);
            }
            progress.retain(|(existing, _)| existing != &token);
            progress.push((token, Progress { title, message, percentage }));
        }
        "report" => {
            if let Some((_, running)) = progress.iter_mut().find(|(existing, _)| existing == &token) {
                // Absent fields mean unchanged, not cleared: rust-analyzer
                // usually sends a bare percentage.
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

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    /// The oldest `window/showMessage` line not yet shown, removed. One at a
    /// time: the status line has room for one, and the first said came
    /// first.
    pub fn take_shown_message(&mut self) -> Option<String> {
        self.shown.pop_front()
    }

    /// What this server is busy with, or `None`. The newest operation: the
    /// status line has room for one, and the newest is still moving. A
    /// server still handshaking says so instead.
    pub fn progress_line(&self) -> Option<String> {
        if !self.is_ready() && !self.is_dead() {
            return Some(format!("{} starting", self.display_name()));
        }
        self.progress.last().map(|(_, progress)| progress.line())
    }

    // The server's name for a status line: the command's first word.
    fn display_name(&self) -> &str {
        self.command.split_whitespace().next().unwrap_or(&self.command).rsplit('/').next().unwrap_or(&self.command)
    }

    /// Whether this server has stopped for good. Dead servers stay in the
    /// table for `::bish lsp status`, so anyone acting on their answers asks.
    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Dead(_))
    }

    /// The server's semantic token type and modifier names, from the
    /// handshake; empty without semantic tokens.
    pub fn semantic_legend(&self) -> lsp::SemanticLegend {
        lsp::SemanticLegend::parse(&self.capabilities)
    }

    /// How this server counts columns. Only meaningful once ready.
    pub fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// What the server said it can do, verbatim, to read with `json::query`
    /// (see lsp.rs).
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

    /// The tail of the server's stderr plus this module's lifecycle notes,
    /// oldest first.
    pub fn log(&self) -> impl Iterator<Item = &String> {
        self.log.iter()
    }
}

impl Drop for Server {
    // An orphaned language server would index a workspace forever.
    // Dropping `stdin` closes it, which most servers take as the end; the
    // kill makes sure, and the wait reaps it.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Every running language server, keyed by (command, root): one per
/// project, shared by every pane in it, which makes cross-file answers
/// possible and keeps workspace indexing affordable. Behind an
/// `Rc<RefCell<_>>` on `Shell`, as `jobs` is: one table for the process,
/// reached by every session's shell, so `::bish lsp status` reads it
/// live.
#[derive(Default)]
pub struct Table {
    servers: Vec<Server>,
    failures: Vec<Failure>,
}

/// A server that couldn't start: no process, but what `::bish lsp status`
/// is usually asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub id: u64,
    pub command: String,
    pub root: PathBuf,
    pub why: String,
}

impl Table {
    /// The server for this command and root, started if needed. A failed
    /// start is remembered and not retried: a command not on `$PATH` won't be
    /// next time either, and files are opened constantly. A server that died
    /// stays dead too.
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

    /// One tick for every server, from the idle callback every blocking loop
    /// in repl.rs shares.
    pub fn service_all(&mut self) {
        for server in &mut self.servers {
            // Requests are answered inside `service` (`Server::answer`);
            // what comes back is notifications and uncollected replies,
            // which nothing consumes yet.
            let _ = server.service();
        }
    }

    pub fn servers(&self) -> &[Server] {
        &self.servers
    }

    /// The running server for this command and root, without spawning: only
    /// opening a file starts one, not document events.
    pub fn running(&mut self, display: &str, root: &Path) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| s.command == display && s.root == root)
    }

    /// The protocol's goodbye to every server, for a clean exit; `Drop`
    /// makes sure.
    pub fn shutdown_all(&mut self) {
        for server in &mut self.servers {
            server.shutdown();
        }
        self.servers.clear();
        // Failures are forgotten too: the cause may be fixed by next time.
        self.failures.clear();
    }
}

impl crate::exec::ServiceTable for Table {
    // What `::bish lsp status` prints; the shell joins these with tabs.
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
        // A server that never started has no state, but it's what `status` is
        // usually run for, so it appears in the same shape.
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
        // A failure to start has no `Server`, and is what `log` is run for.
        out.extend(self.failures.iter().filter(|f| f.id == id).map(|f| f.why.clone()));
        out
    }

    fn forget(&mut self, id: u64) -> usize {
        let before = self.servers.len() + self.failures.len();
        // Told to stop, so a server with a cache writes it; `Drop` would just
        // kill it.
        for server in self.servers.iter_mut().filter(|s| s.id == id) {
            server.shutdown();
        }
        self.servers.retain(|s| s.id != id);
        self.failures.retain(|f| f.id != id);
        before - (self.servers.len() + self.failures.len())
    }
}

/// The most separate edits one `didChange` carries. Past this the diff
/// describes a different file (a formatter ran, a branch changed), and
/// one full-text change is smaller and cheaper for the server.
const MAX_CONTENT_CHANGES: usize = 32;

/// `old` to `new` as LSP `contentChanges`, or `None` when the whole file
/// is better. Whole lines only: columns would have to be in the
/// negotiated encoding, and a mistake there silently describes another
/// edit; line numbers are the same in every encoding. Reverse document
/// order, since the server applies them in turn and each shifts what
/// follows, as `fileeditor::apply_text_edits` does the other way.
fn line_changes(old: &str, new: &str, encoding: PositionEncoding) -> Option<Vec<Value>> {
    if old.is_empty() && !new.is_empty() {
        // Nothing to differ from: the first change after an open that
        // predates knowing about incremental sync, or an empty document.
        return None;
    }
    // `split('\n')`, not `lines()`: a trailing newline is a final empty
    // line, LSP's model of a document.
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    let mut changes = Vec::new();
    // One hunk per Delete/Insert run, an adjacent pair being one
    // replacement.
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
                // An insertion's position in `old` is where the preceding
                // equal run ended.
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
        // A delete directly followed by an insert is one replacement.
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
        // A hunk running off the end can't use
        // `{line: <count>, character: 0}`, which doesn't exist, so it ends
        // at the end of the last line: a replacement there drops its
        // trailing newline, an append past it needs a leading one.
        let (start, end, text) = if to < old_lines.len() {
            // Whole lines for whole lines, each newline-terminated so the
            // line after stays a line.
            (position(from, 0), position(to, 0), added.iter().map(|line| format!("{line}\n")).collect::<String>())
        } else if from == 0 {
            // The whole document.
            (position(0, 0), end_of(&old_lines, encoding), added.join("\n"))
        } else {
            // A hunk to the end can't be lines `[from, count)`, and a range
            // from `{from, 0}` would leave the previous line's newline
            // behind as a trailing blank line. So it starts at the end of
            // the previous line, taking that newline, and the replacement
            // brings its own.
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

// The last position in a document, in the server's columns.
fn end_of(lines: &[&str], encoding: PositionEncoding) -> Value {
    let last: Vec<char> = lines[lines.len() - 1].chars().collect();
    position(lines.len() - 1, lsp::to_server_column(&last, last.len(), encoding))
}

fn position(line: usize, character: usize) -> Value {
    Value::Object(vec![("line".to_string(), Value::Number(line as f64)), ("character".to_string(), Value::Number(character as f64))])
}

/// The project root for `path`: the nearest ancestor containing any of
/// `markers`. Proximity wins over marker order, as other clients do; by
/// marker order a `Cargo.toml` above a `.git` would pick the bigger tree.
/// A Cargo workspace member would usually want the workspace, which only
/// `cargo metadata` knows; name it explicitly. `None` when nothing
/// matches, and no server starts: a wrong root indexes the wrong tree.
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
        // Same command and root: the existing server, shared by every pane.
        table.get_or_start(1, &command, "cat", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        assert_eq!(table.servers().len(), 1);
        // Same command, different root: a second server, scoped to its
        // project.
        table.get_or_start(1, &command, "cat", &other, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        assert_eq!(table.servers().len(), 2);
        assert_eq!(first, dir);
        std::fs::remove_dir_all(&dir).ok();
    }

    // A missing command costs one failed spawn, not one per open.
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

    #[test]
    fn a_server_request_gets_the_answer_a_client_with_nothing_to_offer_should_give() {
        let dir = temp_dir("answer");
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();

        // Three sections, none configured: one `null` each.
        let params = json::parse(r#"{"items":[{"section":"a"},{"section":"b"},{"section":"c"}]}"#).unwrap();
        let (answer, _) = server.answer("workspace/configuration", &params);
        assert_eq!(answer, Ok(Value::Array(vec![Value::Null; 3])));

        // Progress: accepted.
        assert_eq!(server.answer("window/workDoneProgress/create", &Value::Null).0, Ok(Value::Null));

        // Dynamic registration: accepted, ignored, and logged.
        let (answer, note) = server.answer("client/registerCapability", &Value::Null);
        assert_eq!(answer, Ok(Value::Null));
        assert!(note.unwrap().contains("dynamic registration"));

        // An edit bish can't apply, refused in the server's terms.
        assert_eq!(server.answer("workspace/applyEdit", &Value::Null).0, Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))])));

        // Anything else is `MethodNotFound`.
        let (answer, note) = server.answer("workspace/somethingInvented", &Value::Null);
        assert_eq!(answer.unwrap_err().code, -32601);
        assert!(note.unwrap().contains("not implemented"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Applies `contentChanges` as a server does: in order, each against the
    // result of the last, which is why `line_changes` reverses them.
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
            // Char offsets, which utf-32 columns are.
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

    // Whatever the diff, a server applying it in order ends with bish's
    // text; checked by reconstructing the result.
    #[test]
    fn incremental_changes_reconstruct_the_new_text_exactly() {
        // A line rewritten in the middle.
        assert_eq!(round_trip("a\nb\nc\n", "a\nB\nc\n"), 1);
        // Inserted, deleted, both at once.
        round_trip("a\nc\n", "a\nb\nc\n");
        round_trip("a\nb\nc\n", "a\nc\n");
        round_trip("a\nb\nc\n", "a\nx\ny\nz\nc\n");
        // The first and last lines, where a range can't start on a line that
        // doesn't exist.
        round_trip("a\nb\nc\n", "A\nb\nc\n");
        round_trip("a\nb\nc", "a\nb\nC");
        round_trip("a\nb\nc", "a\nb\nc\nd");
        round_trip("a\nb\nc\n", "a\nb\nc\nd\n");
        round_trip("a\nb\nc\nd\n", "a\nb\n");
        // No trailing newline either side, one whole-file rewrite.
        round_trip("one", "two");
        // Two hunks, where reverse order matters: forwards, the second range
        // would describe moved text.
        assert_eq!(round_trip("a\nb\nc\nd\ne\n", "A\nb\nc\nd\nE\n"), 2);
        assert_eq!(round_trip("1\n2\n3\n4\n5\n6\n7\n", "1\nX\n3\n4\nY\nZ\n7\n"), 2);
        // Nothing changed at all.
        assert!(line_changes("a\nb\n", "a\nb\n", PositionEncoding::Utf32).unwrap().is_empty());
        // Emptied entirely, and filled from nothing.
        round_trip("a\nb\n", "");
        round_trip("", "");
    }

    // Non-ASCII is where a hand-written range goes wrong invisibly; only the
    // last line's end has a column.
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

    // The cases nobody thinks of: a small deterministic generator over line
    // sets, every result reconstructed.
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
                // Half with a trailing newline, half without, which the
                // last-line arithmetic turns on.
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

    // Kind 2 means the server wants ranges.
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
        // The one line, by range, not the whole file.
        assert_eq!(json::query(&changes[0], ".text"), Ok(&Value::Str("TWO\n".to_string())));
        assert_eq!(json::query(&changes[0], ".range.start.line"), Ok(&Value::Number(1.0)));
        assert_eq!(json::query(&changes[0], ".range.end.line"), Ok(&Value::Number(2.0)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A full-sync server gets whole files, as does the first change after an
    // open.
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

    #[test]
    fn a_wholesale_rewrite_falls_back_to_sending_the_file() {
        // Every third line rewritten: dozens of hunks, not one replacement.
        let old: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..200).map(|i| if i % 3 == 0 { format!("changed {i}\n") } else { format!("line {i}\n") }).collect();
        assert!(line_changes(&old, &new, PositionEncoding::Utf32).is_none());
        // And with nothing to differ from.
        assert!(line_changes("", "hello\n", PositionEncoding::Utf32).is_none());
    }

    // What each `--apply-edits` setting does to an arriving request.
    #[test]
    fn a_server_requested_edit_is_taken_or_refused_by_policy() {
        let dir = temp_dir("apply-policy");
        let params = json::parse(r#"{"edit":{"changes":{}}}"#).unwrap();
        let request = |id: i64| Message::Request { id: Id::Number(id), method: "workspace/applyEdit".to_string(), params: params.clone() };
        let refused = Ok(Value::Object(vec![("applied".to_string(), Value::Bool(false))]));

        // Scoped, with nothing of the user's running: refused, and the log
        // says which refusal.
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Scoped, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert!(!server.may_apply());
        let (answer, note) = server.answer("workspace/applyEdit", &Value::Null);
        assert_eq!(answer, refused);
        assert!(note.unwrap().contains("no command of yours"));
        assert!(server.handle(request(1)).is_none());
        assert!(server.take_apply_edit().is_none(), "refused, so nothing is waiting for the editor");

        // With a user's command in flight: parked, since the reply says
        // whether it landed.
        server.execute_command(json::parse(r#"{"command":"x"}"#).unwrap());
        assert!(server.may_apply());
        assert!(server.handle(request(2)).is_none());
        let (id, taken) = server.take_apply_edit().expect("parked for the editor");
        assert_eq!(id, Id::Number(2));
        assert_eq!(json::query(&taken, ".edit.changes"), Ok(&Value::Object(Vec::new())));

        // And the window closes when the command's response arrives.
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

    // Services until everything queued is written and echoed into the
    // fixture's log.
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
        assert_eq!(tree(&[("rust-analyzer.check.command", "clippy")]), r#"{"rust-analyzer":{"check":{"command":"clippy"}}}"#);
        // Keys sharing a prefix share its objects.
        assert_eq!(tree(&[("a.b", "1"), ("a.c", "2")]), r#"{"a":{"b":1,"c":2}}"#);
    }

    #[test]
    fn a_value_is_json_when_it_is_json_and_a_string_when_it_is_not() {
        // The boolean `true` isn't the string `"true"`; a server would
        // ignore or reject the wrong one.
        assert_eq!(tree(&[("x.enable", "true")]), r#"{"x":{"enable":true}}"#);
        assert_eq!(tree(&[("x.count", "3")]), r#"{"x":{"count":3}}"#);
        assert_eq!(tree(&[("x.tags", r#"["a","b"]"#)]), r#"{"x":{"tags":["a","b"]}}"#);
        // A bare word isn't JSON, and most settings are strings.
        assert_eq!(tree(&[("x.path", "/usr/bin/foo")]), r#"{"x":{"path":"/usr/bin/foo"}}"#);
        assert_eq!(tree(&[("x.name", "clippy")]), r#"{"x":{"name":"clippy"}}"#);
    }

    // `a` can't be a number and an object; the later wins, as for a repeated
    // key.
    #[test]
    fn a_key_that_contradicts_an_earlier_one_replaces_it() {
        assert_eq!(tree(&[("a", "1"), ("a.b", "2")]), r#"{"a":{"b":2}}"#, "the object replaces the scalar");
        assert_eq!(tree(&[("a.b", "2"), ("a", "1")]), r#"{"a":1}"#, "and the scalar replaces the object");
        // The plain repeated key.
        assert_eq!(tree(&[("a.b", "1"), ("a.b", "2")]), r#"{"a":{"b":2}}"#);
    }

    #[test]
    fn no_settings_is_an_empty_object_not_a_null() {
        // Empty means use your defaults, and suppresses the push.
        assert_eq!(tree(&[]), "{}");
    }

    // The pull half: one answer per item, in order.
    #[test]
    fn workspace_configuration_is_answered_from_the_settings_it_was_given() {
        let dir = temp_dir("configuration");
        let settings = settings_tree(&pairs(&[("rust-analyzer.check.command", "clippy"), ("other.thing", "1")]));
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), settings).unwrap();
        let request = |sections: &[&str]| {
            let items = sections.iter().map(|s| Value::Object(vec![("section".to_string(), Value::Str((*s).to_string()))])).collect();
            Value::Object(vec![("items".to_string(), Value::Array(items))])
        };

        let (answer, _) = server.answer("workspace/configuration", &request(&["rust-analyzer.check"]));
        assert_eq!(json::compact_print(&answer.unwrap()), r#"[{"command":"clippy"}]"#);

        // One answer per item, in order, `null` where nothing was set.
        let (answer, _) = server.answer("workspace/configuration", &request(&["nothing.here", "other.thing"]));
        assert_eq!(json::compact_print(&answer.unwrap()), r#"[null,1]"#);

        // An item with no section means everything.
        let all = Value::Object(vec![("items".to_string(), Value::Array(vec![Value::Object(Vec::new())]))]);
        let (answer, _) = server.answer("workspace/configuration", &all);
        assert!(json::compact_print(&answer.unwrap()).contains("rust-analyzer"));
        std::fs::remove_dir_all(&dir).ok();
    }

    // The push half, for servers that never ask.
    #[test]
    fn settings_are_pushed_once_the_handshake_finishes_and_only_when_there_are_any() {
        let dir = temp_dir("didchangeconfig");
        let log = dir.join("received.jsonl");
        let settings = settings_tree(&pairs(&[("mock.enable", "true")]));
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), settings).unwrap();
        run_until_ready(&mut server);
        // `run_until_ready` stops when the state flips, before what that
        // queued is written to the pipe.
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
        // After `initialized`: only `initialize` may precede it.
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
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        drain(&mut server);
        assert!(
            !received(&log).iter().any(|m| matches!(m, Message::Notification { method, .. } if method == "workspace/didChangeConfiguration")),
            "nothing to say is not the same as saying nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // `logMessage` is for someone who looks; `showMessage` asks to
    // interrupt.
    #[test]
    fn a_show_message_reaches_the_user_and_a_log_message_only_the_log() {
        let dir = temp_dir("show-message");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        let notify = |method: &str, kind: f64, text: &str| Message::Notification {
            method: method.to_string(),
            params: Value::Object(vec![("type".to_string(), Value::Number(kind)), ("message".to_string(), Value::Str(text.to_string()))]),
        };
        assert!(server.handle(notify("window/logMessage", 1.0, "loading")).is_none());
        assert_eq!(server.take_shown_message(), None, "a log line is not shown");

        assert!(server.handle(notify("window/showMessage", 1.0, "no Cargo.toml")).is_none());
        assert!(server.handle(notify("window/showMessage", 3.0, "reloaded")).is_none());
        // Oldest first, with a severity label only where it adds something.
        assert_eq!(server.take_shown_message().as_deref(), Some("mock error: no Cargo.toml"));
        assert_eq!(server.take_shown_message().as_deref(), Some("mock: reloaded"));
        assert_eq!(server.take_shown_message(), None, "each is shown once");

        // Both stay in the log, in order.
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
                params: Value::Object(vec![("type".to_string(), Value::Number(3.0)), ("message".to_string(), Value::Str(format!("line {n}")))]),
            });
        }
        let drained = std::iter::from_fn(|| server.take_shown_message()).count();
        assert_eq!(drained, MAX_SHOWN);
        std::fs::remove_dir_all(&dir).ok();
    }

    // The declaration matches the policy.
    #[test]
    fn the_declared_apply_edit_capability_follows_the_policy() {
        let dir = temp_dir("apply-declared");
        for (policy, declared) in [(ApplyEdits::Scoped, true), (ApplyEdits::Always, true), (ApplyEdits::Never, false)] {
            let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, policy, Value::Object(Vec::new())).unwrap();
            let params = server.initialize_params();
            assert_eq!(json::query(&params, ".capabilities.workspace.applyEdit"), Ok(&Value::Bool(declared)), "{policy:?}");
            // Always claimed: an action with no edit needs its command run,
            // whether or not the command may change anything.
            assert!(json::query(&params, ".capabilities.workspace.executeCommand.dynamicRegistration").is_ok());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // Without the claim a server sends plain text, and function completions
    // lack parentheses.
    #[test]
    fn snippet_support_is_declared_so_servers_actually_send_snippets() {
        let dir = temp_dir("snippet-cap");
        let server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        let params = server.initialize_params();
        assert_eq!(json::query(&params, ".capabilities.textDocument.completion.completionItem.snippetSupport"), Ok(&Value::Bool(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Nothing stays parked when the window closes: a server can wait on an
    // `applyEdit` reply forever.
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
        // The refusals were sent, as the fixture's log shows.
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

    // Past the cap, a refusal rather than unbounded memory.
    #[test]
    fn parked_edits_are_capped() {
        let dir = temp_dir("apply-cap");
        let mut server = Server::start(1, &mock_server(FULL_SYNC), "mock", &dir, ApplyEdits::Always, Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        for id in 0..(MAX_PENDING_APPLIES as i64 + 5) {
            server.handle(Message::Request { id: Id::Number(id), method: "workspace/applyEdit".to_string(), params: Value::Null });
        }
        assert_eq!(server.applies.len(), MAX_PENDING_APPLIES);
        // The oldest went: the editor has had longest to get to them.
        assert_eq!(server.applies[0].0, Id::Number(5));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Queueing's sharp edge: a server that stopped reading never starts
    // again, and each keystroke queues a document.
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

    // Diagnostics are pushed, never requested, so after a clear this is
    // the only copy.
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
        // Nothing to redeliver for a document that had none.
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
        // A failure's reason is the log for a server with no process.
        assert!(table.logs(2).iter().any(|l| l.contains("bish-no-such-server")));

        assert_eq!(table.forget(2), 1);
        assert_eq!(table.rows().len(), 1);
        // And it's tried again rather than refused from memory.
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

        // Nearest wins, whatever the markers' order.
        assert_eq!(root_for(&file, &["Cargo.toml".to_string(), ".git".to_string()]), Some(crate_dir.clone()));
        assert_eq!(root_for(&file, &[".git".to_string(), "Cargo.toml".to_string()]), Some(crate_dir.clone()));
        // A marker high up doesn't beat one directly above the file.
        let deep = repo.join("crates").join("inner").join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "").unwrap();
        let nested = deep.join("lib.rs");
        std::fs::write(&nested, "").unwrap();
        assert_eq!(
            root_for(&nested, &["Cargo.toml".to_string(), ".git".to_string()]),
            Some(crate_dir.clone()),
            "the crate directly above, not the workspace far above"
        );
        let _ = &repo;
        // With only an absent marker, nothing, rather than a guess.
        assert_eq!(root_for(&file, &["go.mod".to_string()]), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // Everything below drives a real child. `cat` is the simplest
    // server-shaped thing anywhere: stdin to stdout, exercising spawning,
    // non-blocking pipes and the queue without a language server.

    // A mock that answers `initialize` and stays alive. It parses nothing:
    // `initialize` is always bish's first request, id 1, so a canned reply
    // to id 1 completes the handshake.
    fn mock_server(result: &str) -> Vec<String> {
        mock_server_script(result, "sleep 30")
    }

    // The same mock echoing its stdin, so everything bish sends comes back
    // through `lsp::Decoder` and tests assert on exact messages, order and
    // versions. Document sync is all notifications, so no reply loop can
    // start. Per-method replies need `scripted_server`.
    fn echo_server(result: &str) -> Vec<String> {
        mock_server_script(result, "exec cat")
    }

    fn mock_server_script(result: &str, then: &str) -> Vec<String> {
        let body = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#);
        let script = format!("printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{body}'; {then}", body.len());
        vec!["sh".to_string(), "-c".to_string(), script]
    }

    // Services until `want` `textDocument/*` notifications are back or time
    // runs out, ignoring the rest.
    fn document_notifications(server: &mut Server, want: usize) -> Vec<(String, Value)> {
        collect_notifications(server, want, None)
    }

    // The negative form, for asserting `want` is not reached: waits until
    // 100ms pass with nothing new, rather than the whole two seconds, with
    // the 400-round ceiling as a backstop.
    fn document_notifications_until_quiet(server: &mut Server, want: usize) -> Vec<(String, Value)> {
        collect_notifications(server, want, Some(20))
    }

    fn collect_notifications(server: &mut Server, want: usize, quiet_rounds: Option<usize>) -> Vec<(String, Value)> {
        let mut seen = Vec::new();
        let mut quiet = 0;
        for _ in 0..400 {
            let before = seen.len();
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
            quiet = if seen.len() == before { quiet + 1 } else { 0 };
            if quiet_rounds.is_some_and(|limit| quiet >= limit) {
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

        // Opening the same document again says nothing: a second `didOpen`
        // for a uri is a protocol error.
        server.open_document("file:///p/x.sh", "shellscript", 4, "echo bye\n");
        assert_eq!(server.open_documents(), 1);
        assert!(document_notifications_until_quiet(&mut server, 2).len() < 2);
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
        // Unchanged: nothing, and no clock started.
        assert!(!server.needs_change("file:///p/x.sh", 1, start, debounce));
        // Just changed: this starts the clock.
        assert!(!server.needs_change("file:///p/x.sh", 2, start, debounce));
        // Still typing: measured from the first unsent change, so a burst
        // still sends.
        assert!(!server.needs_change("file:///p/x.sh", 3, start + Duration::from_millis(100), debounce));
        assert!(server.needs_change("file:///p/x.sh", 3, start + debounce, debounce));

        server.change_document("file:///p/x.sh", 3, "one two three");
        let seen = document_notifications(&mut server, 1);
        let (method, params) = seen.last().expect("a didChange");
        assert_eq!(method, "textDocument/didChange");
        assert_eq!(json::query(params, ".textDocument.version"), Ok(&Value::Number(3.0)));
        // Full sync: one event with the whole document and no range, legal
        // for an incremental server too.
        assert_eq!(json::query(params, ".contentChanges[0].text"), Ok(&Value::Str("one two three".to_string())));
        assert_eq!(json::query(params, ".contentChanges[0].range"), Ok(&Value::Null));

        // Sent: the clock resets, so an idle editor stops asking.
        assert!(!server.needs_change("file:///p/x.sh", 3, start + Duration::from_secs(10), debounce));
        std::fs::remove_dir_all(&dir).ok();
    }

    // A save mustn't report a version the server was never told about.
    #[test]
    fn saving_flushes_a_pending_change_before_announcing_the_save() {
        let dir = temp_dir("didsave");
        let mut server = ready_echo_server(&dir);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        server.change_document("file:///p/x.sh", 2, "one two");
        // A save flushes unconditionally, so this mustn't send the document
        // twice.
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

        // Closing what isn't open says nothing.
        server.close_document("file:///p/other.sh");
        assert!(document_notifications_until_quiet(&mut server, 3).len() < 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server that publishes diagnostics as soon as it's told about a
    // document, as real ones do.
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

    // The scripted fixture: reads the framing and replies to what arrived.
    // Written from `include_str!`, so it exists only in the test binary.
    fn scripted_server(dir: &Path, log: &Path) -> Vec<String> {
        let script = dir.join("lsp-mock.sh");
        std::fs::write(&script, include_str!("testdata/lsp-mock.sh")).unwrap();
        vec!["sh".to_string(), script.to_string_lossy().into_owned(), log.to_string_lossy().into_owned()]
    }

    // Everything the fixture was told, one framed message per line,
    // decoded with the decoder under test.
    fn received(log: &Path) -> Vec<Message> {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        text.lines().filter_map(|line| json::parse(line).ok()).filter_map(|v| Message::from_value(&v).ok()).collect()
    }

    // One `$/progress` notification's params, as a server sends them.
    fn progress_params(json: &str) -> Value {
        json::parse(json).unwrap()
    }

    // What the status line shows after these.
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
        // rust-analyzer's shape: a `begin` with a message, then bare
        // percentages, which mustn't clear the message.
        let begin = r#"{"token":"t","value":{"kind":"begin","title":"Indexing","message":"core","percentage":10}}"#;
        let bump = r#"{"token":"t","value":{"kind":"report","percentage":20}}"#;
        assert_eq!(progress_after(&[begin, bump]).as_deref(), Some("Indexing 20%: core"));
    }

    #[test]
    fn a_report_for_a_token_that_never_began_is_ignored() {
        // No title to show it under, and a bare "40%" would say nothing.
        let orphan = r#"{"token":"ghost","value":{"kind":"report","percentage":40}}"#;
        assert_eq!(progress_after(&[orphan]), None);
    }

    #[test]
    fn the_newest_operation_is_the_one_shown() {
        // Overlapping operations are normal (rust-analyzer indexes and builds
        // proc macros at once); the newest is shown.
        let first = r#"{"token":"a","value":{"kind":"begin","title":"Indexing"}}"#;
        let second = r#"{"token":"b","value":{"kind":"begin","title":"Building"}}"#;
        let end_second = r#"{"token":"b","value":{"kind":"end"}}"#;
        assert_eq!(progress_after(&[first, second]).as_deref(), Some("Building"));
        // Ending it falls back to the one still running.
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
        // Declared and undeclared capabilities, so a request isn't spent on a
        // timeout.
        assert!(server.provides("hoverProvider"));
        assert!(server.provides("definitionProvider"));
        assert!(server.provides("referencesProvider"));
        // A name no server will declare, so the assertion doesn't depend on
        // the fixture's list.
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

        // Collected once, so a waiter never sees a stale reply.
        assert!(server.take_response(id).is_none());

        // And the fixture recorded what bish sent, in order.
        let sent = received(&log);
        let methods: Vec<&str> = sent
            .iter()
            .filter_map(|m| match m {
                Message::Request { method, .. } | Message::Notification { method, .. } => Some(method.as_str()),
                Message::Response { .. } => None,
            })
            .collect();
        assert_eq!(methods, vec!["initialize", "initialized", "textDocument/didOpen", "textDocument/hover"], "{methods:?}");
        let Some(Message::Request { params, .. }) =
            sent.iter().find(|m| matches!(m, Message::Request { method, .. } if method == "textDocument/hover"))
        else {
            panic!("no hover request recorded");
        };
        assert_eq!(json::query(params, ".position.character"), Ok(&Value::Number(1.0)));
        assert_eq!(json::query(params, ".textDocument.uri"), Ok(&Value::Str("file:///p/x.sh".to_string())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uncollected_replies_are_bounded() {
        let dir = temp_dir("bounded");
        let log = dir.join("received.jsonl");
        let mut server = Server::start(1, &scripted_server(&dir, &log), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        server.open_document("file:///p/x.sh", "shellscript", 1, "echo hi");
        let position = Value::Object(vec![
            ("textDocument".to_string(), Value::Object(vec![("uri".to_string(), Value::Str("file:///p/x.sh".to_string()))])),
            ("position".to_string(), Value::Object(vec![("line".to_string(), Value::Number(0.0)), ("character".to_string(), Value::Number(0.0))])),
        ]);
        let mut ids = Vec::new();
        for _ in 0..MAX_PENDING_RESPONSES + 10 {
            ids.push(server.request("textDocument/hover", position.clone()));
        }
        // Nobody collects them.
        let last = *ids.last().unwrap();
        wait_for_response(&mut server, last);
        assert!(server.responses.len() <= MAX_PENDING_RESPONSES, "{} kept", server.responses.len());
        // The oldest went, the newest survives.
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
        // Consumed: a second ask has nothing, so the editor doesn't repaint
        // every tick.
        assert!(server.take_diagnostics(uri).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_publication_for_an_unknown_document_is_dropped() {
        let dir = temp_dir("unknown");
        let mut server =
            Server::start(1, &publishing_server("file:///p/never-opened.sh"), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new()))
                .unwrap();
        run_until_ready(&mut server);
        server.open_document("file:///p/x.sh", "shellscript", 1, "one");
        for _ in 0..200 {
            server.service();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // About no buffer, so nothing kept.
        assert!(server.take_diagnostics("file:///p/x.sh").is_none());
        assert!(server.take_diagnostics("file:///p/never-opened.sh").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server's options object means what it says; notifications it
    // declined are noise.
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
        assert!(document_notifications_until_quiet(&mut server, 1).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_sync_capability_is_read_from_either_form_a_server_may_use() {
        // The number form names only the change kind; open and close are
        // implied.
        let number = json::parse(r#"{"textDocumentSync":2}"#).unwrap();
        assert_eq!(Sync::parse(&number), Sync { open_close: true, change: true, incremental: true, save: false }, "kind 2 is the incremental one");
        let full = json::parse(r#"{"textDocumentSync":1}"#).unwrap();
        assert_eq!(Sync::parse(&full), Sync { open_close: true, change: true, incremental: false, save: false });
        let none = json::parse(r#"{"textDocumentSync":0}"#).unwrap();
        assert_eq!(Sync::parse(&none), Sync { open_close: true, change: false, incremental: false, save: false });
        // `save` as a bool or an object both mean yes.
        let object = json::parse(r#"{"textDocumentSync":{"openClose":true,"change":1,"save":{"includeText":true}}}"#).unwrap();
        assert_eq!(Sync::parse(&object), Sync { open_close: true, change: true, incremental: false, save: true });
        // Nothing said: full sync rather than the spec's none (see
        // `Sync::default`).
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
        // utf-32, bish's own counting, makes positions exact.
        assert_eq!(server.encoding(), PositionEncoding::Utf32);
        // Capabilities are kept verbatim for `json::query`.
        assert_eq!(json::query(server.capabilities(), ".hoverProvider"), Ok(&Value::Bool(true)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // Every server predating the negotiation lands here, so the default must
    // be right.
    #[test]
    fn a_server_that_names_no_encoding_is_assumed_to_mean_utf16() {
        let dir = temp_dir("utf16");
        let mut server =
            Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        run_until_ready(&mut server);
        assert_eq!(*server.state(), State::Ready, "log: {:?}", server.log().collect::<Vec<_>>());
        assert_eq!(server.encoding(), PositionEncoding::Utf16);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn work_queued_during_the_handshake_goes_out_once_it_finishes() {
        let dir = temp_dir("release");
        let mut server =
            Server::start(1, &mock_server(r#"{"capabilities":{}}"#), "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
        server.request("textDocument/hover", Value::Null);
        assert_eq!(server.queued.len(), 1);
        run_until_ready(&mut server);
        assert!(server.queued.is_empty(), "the handshake should have released it");
        // After that, sends go straight out.
        server.notify("textDocument/didSave", Value::Null);
        assert!(server.queued.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    // A rejected handshake is reported as itself, not as a dead process.
    #[test]
    fn an_initialize_that_is_answered_with_an_error_kills_the_server() {
        let dir = temp_dir("refused");
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unsupported client"}}"#;
        let script = format!("printf 'Content-Length: %d\\r\\n\\r\\n%s' {} '{body}'; sleep 30", body.len());
        let mut server =
            Server::start(1, &["sh".to_string(), "-c".to_string(), script], "mock", &dir, ApplyEdits::default(), Value::Object(Vec::new())).unwrap();
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
        // Before servicing: the process exists, no handshake yet.
        assert_eq!(*server.state(), State::Initializing);
        assert!(!server.is_ready());
        run_until_ready(&mut server);

        // What went out, from the fixture's record: requests are answered
        // inside `service`.
        let sent = received(&log);
        let Some(Message::Request { method, params, .. }) = sent.first() else {
            panic!("expected initialize first, got {sent:?}");
        };
        assert_eq!(method, "initialize");
        assert_eq!(json::query(params, ".clientInfo.name"), Ok(&Value::Str("bish".to_string())));
        // utf-32 offered first.
        let Ok(Value::Array(encodings)) = json::query(params, ".capabilities.general.positionEncodings") else {
            panic!("no positionEncodings in {params:?}");
        };
        assert_eq!(encodings[0], Value::Str("utf-32".to_string()));
        // The root is a percent-encoded file URI.
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
        // Order on the wire: nothing but `initialize` precedes `initialized`.
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
        let Err(error) = Server::start(1, &["bish-no-such-language-server".to_string()], "x", &dir, ApplyEdits::default(), Value::Object(Vec::new()))
        else {
            panic!("a command that isn't there should not have started");
        };
        assert!(error.contains("bish-no-such-language-server"), "{error}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // A server exiting at once is the most common failure (a typo, a
    // missing runtime), and its last words are why stderr is kept.
    #[test]
    fn a_server_that_exits_is_noticed_with_its_last_words() {
        let dir = temp_dir("dies");
        let script = "echo 'cannot find configuration' >&2; exit 3";
        let mut server =
            Server::start(1, &["sh".to_string(), "-c".to_string(), script.to_string()], "sh", &dir, ApplyEdits::default(), Value::Object(Vec::new()))
                .unwrap();
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

    // A write reaching a server that stopped reading but hasn't exited, or
    // exited uncollected: the broken pipe is how the death is noticed, not
    // its reason. Forced by closing stdin well before exiting.
    #[test]
    fn a_server_that_stops_reading_before_it_exits_is_reported_by_its_exit() {
        let dir = temp_dir("stops-reading");
        let script = "exec 0<&-; echo 'cannot find configuration' >&2; sleep 0.3; exit 3";
        let mut server =
            Server::start(1, &["sh".to_string(), "-c".to_string(), script.to_string()], "sh", &dir, ApplyEdits::default(), Value::Object(Vec::new()))
                .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
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
        assert!(why.contains("cannot find configuration"), "{why}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dead_server_stops_accepting_work() {
        let dir = temp_dir("dead");
        let mut server = Server::start(
            1,
            &["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
            "sh",
            &dir,
            ApplyEdits::default(),
            Value::Object(Vec::new()),
        )
        .unwrap();
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
