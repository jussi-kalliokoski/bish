// `bish tool mcp-server` -- the Model Context Protocol, spoken over
// stdio, so an agent can ask bish what it is looking at.
//
// bish knows things nothing outside it can recover: which file is open,
// where the cursor is, what the language server found, what is
// selected. An agent running in a pane can only scrape the screen. This
// module is the door.
//
// MCP is JSON-RPC 2.0, which `lsp.rs` already is -- `lsp::Message`
// carries no notion of which protocol it is for, only which of the three
// shapes a message has, so it needed nothing added for this. The one
// real difference from `lspserver.rs` is the framing: MCP over stdio is
// **newline-delimited JSON**, not `Content-Length`, so `framing.rs`
// stays where it is and a much smaller reader lives here instead.
//
// **This is the only file that knows MCP.** That is the whole point of
// where the seam is. The protocol will move -- it is younger than LSP
// and its capability negotiation is still settling -- and when it does,
// this file changes and nothing else does. Everything underneath it is
// bish's own: the session socket carries opaque bytes, and the answers
// come from the same `TextBuffer` and `lint::Diagnostic` the editor
// draws from.
//
// And **nothing about a particular client is in here either**. The
// transport a given agent wants this year -- a WebSocket, a lockfile in
// a well-known directory, a token in an HTTP header -- is somebody
// else's process, bridging that to this one's stdin and stdout. None of
// it is durable and none of it belongs in bish.
#![allow(dead_code)]

use crate::json::{self, Value};
use crate::lsp::{Id, Message, ResponseError};
use std::io::{Read, Write};

/// Which revision of *this surface* a peer is talking to, and the answer
/// to every `initialize` regardless of what was asked for.
///
/// It says "unstable" because it is: these tools, their payloads and
/// this whole entry point can change or be withdrawn, and `bish tool
/// mcp-server` is unlisted for the same reason. Anything connecting has
/// been told so in the one field every client is guaranteed to read.
///
/// bish's own, and deliberately not a specification date. Those dates
/// belong to the specification and say nothing about bish -- the ones
/// that exist all predate this file, and quoting one would be claiming
/// to have implemented a document rather than describing what is
/// actually here. What is actually here is a set of tools and
/// notifications that bish decides, so what it announces is a version of
/// that set.
///
/// Which means it is **not what a client expects to see**, and it is not
/// meant to be: reconciling this with whatever a given client demands is
/// the job of whatever is bridging bish to that client. Putting the
/// client's answer here instead would be moving that job into the one
/// file that is supposed to know nothing about any client.
///
/// It moves when this surface changes -- a tool removed, a payload
/// reshaped -- and not when bish is released. `serverInfo.version`
/// already says which bish this is; this says what it serves.
const PROTOCOL_VERSION: &str = "bish/unstable";

/// What a peer is told this server is. The name is bish's own rather
/// than an imitation of any editor's: a client that branches on it is
/// entitled to know what it is actually talking to.
const SERVER_NAME: &str = "bish";

/// JSON-RPC's own code for a method that does not exist.
const METHOD_NOT_FOUND: i64 = -32601;

/// One JSON object, written out longhand. `json.rs` has no builder and
/// deliberately so; every caller that needs one writes this same three
/// lines, so this module writes it once.
fn object(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect())
}

fn string(s: &str) -> Value {
    Value::Str(s.to_string())
}

/// A tool result: the `{content: [...]}` envelope every `tools/call`
/// answers with.
///
/// `is_error` is the part worth knowing about. A tool that *fails* still
/// returns a successful JSON-RPC response, with the failure described
/// inside the result -- a JSON-RPC error means "this call was
/// malformed", not "the thing you asked for did not work". Getting that
/// backwards makes a client treat a routine "no such file" as a broken
/// server.
fn tool_result(text: &str, is_error: bool) -> Value {
    object(vec![("content", Value::Array(vec![object(vec![("type", string("text")), ("text", string(text))])])), ("isError", Value::Bool(is_error))])
}

// ---------------------------------------------------------------------
// What bish says about itself, in bish's own terms
// ---------------------------------------------------------------------
//
// Everything below this line is *not* MCP. It is the little protocol
// `bish tool mcp-server` speaks to a running session over its socket,
// and it is deliberately shaped like bish rather than like any client:
// plain paths, 0-based line and character positions, severities spelled
// the way `lint::Severity` spells them. The conversion into whatever a
// client wants happens above, in one place, which is what makes this
// half safe to leave alone when the protocol above it moves.
//
// It also never leaves the machine, so it needs no version negotiation
// and can change whenever both halves ship together -- which they do,
// being the same binary.

/// One finding, wherever it came from -- bish's own linter or a
/// language server, which the editor already merges into one list.
#[derive(Debug, Clone, PartialEq)]
pub struct Diag {
    pub message: String,
    pub severity: crate::bishedit::lint::Severity,
    /// 0-based (line, character), both ends.
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// The language server that said it, or `None` for bish itself.
    pub source: Option<String>,
    pub code: String,
}

/// One file the editor has open.
#[derive(Debug, Clone, PartialEq)]
pub struct FileState {
    pub path: std::path::PathBuf,
    pub line_count: usize,
    /// Whether this is the one the user is looking at.
    pub focused: bool,
    pub dirty: bool,
    /// 0-based (start line, start character, end line, end character).
    pub selection: Option<(usize, usize, usize, usize)>,
    pub selected_text: String,
    pub diagnostics: Vec<Diag>,
}

/// Everything bish is willing to say right now.
///
/// One question and one answer, rather than a question per tool: the
/// whole thing is small, a round trip over a unix socket costs more than
/// the extra fields, and one shape is one shape to test.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditorState {
    pub files: Vec<FileState>,
}

impl EditorState {
    pub fn focused(&self) -> Option<&FileState> {
        self.files.iter().find(|f| f.focused)
    }

    fn to_value(&self) -> Value {
        object(vec![("files", Value::Array(self.files.iter().map(FileState::to_value).collect()))])
    }

    fn from_value(v: &Value) -> EditorState {
        let files = match json::query(v, ".files") {
            Ok(Value::Array(items)) => items.iter().filter_map(FileState::from_value).collect(),
            _ => Vec::new(),
        };
        EditorState { files }
    }
}

impl FileState {
    fn to_value(&self) -> Value {
        let selection = match self.selection {
            Some((a, b, c, d)) => Value::Array([a, b, c, d].iter().map(|n| Value::Number(*n as f64)).collect()),
            None => Value::Null,
        };
        object(vec![
            ("path", string(&self.path.to_string_lossy())),
            ("lines", Value::Number(self.line_count as f64)),
            ("focused", Value::Bool(self.focused)),
            ("dirty", Value::Bool(self.dirty)),
            ("selection", selection),
            ("text", string(&self.selected_text)),
            ("diagnostics", Value::Array(self.diagnostics.iter().map(Diag::to_value).collect())),
        ])
    }

    fn from_value(v: &Value) -> Option<FileState> {
        let Ok(Value::Str(path)) = json::query(v, ".path") else { return None };
        let selection = match json::query(v, ".selection") {
            Ok(Value::Array(n)) if n.len() == 4 => {
                let at = |i: usize| match n[i] {
                    Value::Number(x) => x as usize,
                    _ => 0,
                };
                Some((at(0), at(1), at(2), at(3)))
            }
            _ => None,
        };
        Some(FileState {
            path: std::path::PathBuf::from(path),
            line_count: number_at(v, ".lines"),
            focused: matches!(json::query(v, ".focused"), Ok(Value::Bool(true))),
            dirty: matches!(json::query(v, ".dirty"), Ok(Value::Bool(true))),
            selection,
            selected_text: string_at(v, ".text").unwrap_or_default(),
            diagnostics: match json::query(v, ".diagnostics") {
                Ok(Value::Array(items)) => items.iter().filter_map(Diag::from_value).collect(),
                _ => Vec::new(),
            },
        })
    }
}

impl Diag {
    fn to_value(&self) -> Value {
        object(vec![
            ("message", string(&self.message)),
            ("severity", string(severity_name(self.severity))),
            ("start", Value::Array(vec![Value::Number(self.start.0 as f64), Value::Number(self.start.1 as f64)])),
            ("end", Value::Array(vec![Value::Number(self.end.0 as f64), Value::Number(self.end.1 as f64)])),
            ("source", self.source.as_deref().map(string).unwrap_or(Value::Null)),
            ("code", string(&self.code)),
        ])
    }

    fn from_value(v: &Value) -> Option<Diag> {
        let pair = |path: &str| match json::query(v, path) {
            Ok(Value::Array(n)) if n.len() == 2 => {
                let at = |i: usize| match n[i] {
                    Value::Number(x) => x as usize,
                    _ => 0,
                };
                (at(0), at(1))
            }
            _ => (0, 0),
        };
        Some(Diag {
            message: string_at(v, ".message")?,
            severity: severity_from_name(&string_at(v, ".severity").unwrap_or_default()),
            start: pair(".start"),
            end: pair(".end"),
            source: string_at(v, ".source"),
            code: string_at(v, ".code").unwrap_or_default(),
        })
    }
}

/// bish's own spelling, not any client's -- see `mcp_severity` for the
/// place that translates.
fn severity_name(severity: crate::bishedit::lint::Severity) -> &'static str {
    use crate::bishedit::lint::Severity;
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    }
}

fn severity_from_name(name: &str) -> crate::bishedit::lint::Severity {
    use crate::bishedit::lint::Severity;
    match name {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        _ => Severity::Hint,
    }
}

fn string_at(value: &Value, path: &str) -> Option<String> {
    match json::query(value, path) {
        Ok(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn number_at(value: &Value, path: &str) -> usize {
    match json::query(value, path) {
        Ok(Value::Number(n)) if *n >= 0.0 => *n as usize,
        _ => 0,
    }
}

/// The question a peer asks, and the answer a session gives.
///
/// Called from `repl.rs`'s own idle tick with the state it alone can
/// assemble. `None` means "nothing to send back", which is what a
/// question nobody recognizes gets: this is a private protocol between
/// two halves of one binary, so an unknown question is a bug on the
/// asking side, not something to negotiate about.
pub fn answer(state: &EditorState, question: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(question).ok()?;
    let value = json::parse(text).ok()?;
    match string_at(&value, ".q")?.as_str() {
        "state" => Some(json::compact_print(&state.to_value()).into_bytes()),
        // Announcing itself. Deliberately answered with nothing: the
        // point is the side effect -- the session now knows somebody is
        // listening, and starts keeping current the things it only
        // bothers to track when somebody is. A reply here would also sit
        // in the socket waiting to be mistaken for the answer to the
        // first real question.
        "hello" => None,
        _ => None,
    }
}

/// The focus notification, in the private protocol: what the editor is
/// showing now, or that it is showing nothing.
///
/// Sent unasked, which is the whole reason the socket carries replies
/// and notifications on one message kind -- see `session::Message`.
pub fn selection_notification(focus: Option<&FileState>) -> Option<Vec<u8>> {
    let payload = object(vec![("n", string("focus")), ("file", focus.map(FileState::to_value).unwrap_or(Value::Null))]);
    Some(json::compact_print(&payload).into_bytes())
}

/// A proposed new version of a file, for somebody to go through change
/// by change -- `openDiff`, in bish's own terms.
///
/// The one question in this protocol whose answer cannot come from the
/// tick that received it: it waits on a person. So it carries an `id`,
/// and the answer names it, rather than being whatever reply comes next.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRequest {
    pub id: u64,
    pub path: std::path::PathBuf,
    pub proposed: String,
    pub title: String,
}

impl ReviewRequest {
    fn to_value(&self) -> Value {
        object(vec![
            ("q", string("review")),
            ("id", Value::Number(self.id as f64)),
            ("path", string(&self.path.to_string_lossy())),
            ("proposed", string(&self.proposed)),
            ("title", string(&self.title)),
        ])
    }

    /// The request `question` is, if it is one.
    pub fn from_question(question: &[u8]) -> Option<ReviewRequest> {
        let value = json::parse(std::str::from_utf8(question).ok()?).ok()?;
        if string_at(&value, ".q")? != "review" {
            return None;
        }
        Some(ReviewRequest {
            id: number_at(&value, ".id") as u64,
            path: std::path::PathBuf::from(string_at(&value, ".path")?),
            proposed: string_at(&value, ".proposed")?,
            title: string_at(&value, ".title").unwrap_or_default(),
        })
    }
}

/// How a review ended: with the file as it now stands, written, or with
/// nothing done to it.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewOutcome {
    Saved(String),
    Rejected,
}

/// The answer to review `id`.
pub fn review_answer(id: u64, outcome: &ReviewOutcome) -> Vec<u8> {
    let mut fields = vec![("a", string("review")), ("id", Value::Number(id as f64))];
    match outcome {
        ReviewOutcome::Saved(text) => {
            fields.push(("outcome", string("saved")));
            fields.push(("text", string(text)));
        }
        ReviewOutcome::Rejected => fields.push(("outcome", string("rejected"))),
    }
    json::compact_print(&object(fields)).into_bytes()
}

/// The outcome `payload` reports for review `id`, if that is what it is.
fn review_outcome(payload: &[u8], id: u64) -> Option<ReviewOutcome> {
    let value = json::parse(std::str::from_utf8(payload).ok()?).ok()?;
    if string_at(&value, ".a")? != "review" || number_at(&value, ".id") as u64 != id {
        return None;
    }
    match string_at(&value, ".outcome")?.as_str() {
        "saved" => Some(ReviewOutcome::Saved(string_at(&value, ".text")?)),
        _ => Some(ReviewOutcome::Rejected),
    }
}

/// The other end of it.
fn ask_state(peer: &mut crate::session::Peer) -> Result<EditorState, String> {
    let reply = peer.ask(br#"{"q":"state"}"#, std::time::Duration::from_millis(2000)).map_err(|e| e.to_string())?;
    let text = String::from_utf8(reply).map_err(|e| e.to_string())?;
    Ok(EditorState::from_value(&json::parse(&text)?))
}

pub struct Server {
    /// Set by `initialize`, so a peer that skips the handshake and asks
    /// for something is told what it did rather than quietly served.
    initialized: bool,
    /// The session being reported on, if any. `None` is a perfectly
    /// good server that has nothing to say: it completes the handshake
    /// and offers no tools, which a client discovers and works around.
    /// That is the shape that makes starting small safe.
    session: Option<crate::session::Peer>,
    /// The last review asked for, so each answer can name its question.
    next_review: u64,
    /// What the session said while a review was being waited on and
    /// that was not its answer -- held for `forward_notifications`,
    /// since a wait is no reason to drop a notification.
    held: Vec<Vec<u8>>,
}

impl Default for Server {
    fn default() -> Server {
        Server::new()
    }
}

/// `bish tool mcp-server [--session NAME]`.
///
/// Without `--session`, the bish it is running inside, found through the
/// `BISH_SOCKET` every interactive bish gives its panes (see
/// `session::listen_here`). Run from anywhere else, it reports on nothing,
/// as it always did.
///
/// Unlisted, and with no `--help` of its own: `tool.rs` leaves it out of
/// the subcommand table, so it is not offered, not suggested after a
/// typo, and not described anywhere a reader would find it by looking.
/// That is the point rather than an omission -- this surface has not
/// settled, and anything documented is something somebody is entitled to
/// keep. This module's own doc comment is the documentation until that
/// changes, and `PROTOCOL_VERSION` tells anything connecting the same.
pub fn run(args: &[String]) -> i32 {
    let mut session = None;
    let mut rest = args;
    // `--session NAME` and `--session=NAME`, and nothing else. There is
    // no `--help` here, deliberately -- see this function's own comment.
    while let Some(arg) = rest.first() {
        let named = arg
            .strip_prefix("--session=")
            .map(str::to_string)
            .or_else(|| (arg == "--session").then(|| rest.get(1).cloned().unwrap_or_default()).filter(|name| !name.is_empty()));
        let Some(name) = named else {
            eprintln!("bish tool mcp-server: unrecognized option '{arg}'");
            return 2;
        };
        rest = &rest[if arg == "--session" { 2 } else { 1 }..];
        session = Some(name);
    }
    let connected = match &session {
        Some(name) => Some(crate::session::Peer::connect(name)),
        None => std::env::var_os("BISH_SOCKET")
            .filter(|path| !path.is_empty())
            .map(|path| crate::session::Peer::connect_path(std::path::Path::new(&path))),
    };
    let peer = match connected {
        Some(connected) => match connected {
            Ok(mut peer) => {
                // Before the client has asked anything. What the editor
                // publishes for us to read is only kept current while a
                // peer exists, so a peer that first announces itself
                // when it asks would find the answer to its own first
                // question not ready yet -- which it did, and this is
                // the fix for it.
                if let Err(e) = peer.tell(br#"{"q":"hello"}"#) {
                    eprintln!("bish tool mcp-server: {e}");
                    return 1;
                }
                Some(peer)
            }
            Err(e) => {
                eprintln!("bish tool mcp-server: {e}");
                return 1;
            }
        },
        None => None,
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    Server { initialized: false, next_review: 0, held: Vec::new(), session: peer }.serve(&mut stdin.lock(), &mut stdout.lock())
}

impl Server {
    pub fn new() -> Server {
        Server { initialized: false, next_review: 0, held: Vec::new(), session: None }
    }

    /// The loop, over any reader and writer rather than stdin and stdout
    /// specifically -- which is what lets the tests drive a whole
    /// session by handing it bytes, the same shape `lspserver.rs` uses.
    pub fn serve(&mut self, input: &mut impl Read, output: &mut impl Write) -> i32 {
        let mut lines = Lines::default();
        let mut buf = [0u8; 8192];
        loop {
            // Everything already complete is answered before another
            // read: one read can carry several messages, and a client
            // that sent `initialize` and `tools/list` together is
            // entitled to both answers.
            while let Some(line) = lines.take_line() {
                if let Some(message) = self.decode(&line, output) {
                    self.handle(message, output);
                }
            }
            // Anything the session has said unasked, before waiting on
            // the client again -- a notification nobody is waiting for
            // is one that has to be noticed rather than replied to.
            self.forward_notifications(output);
            match input.read(&mut buf) {
                // The peer is gone; there is nobody left to answer.
                Ok(0) => return 0,
                Ok(n) => lines.feed(&buf[..n]),
                Err(e) => {
                    eprintln!("bish tool mcp-server: error reading stdin: {e}");
                    return 1;
                }
            }
        }
    }

    /// Passes on whatever the session has said on its own, translated.
    ///
    /// A notice this side does not recognize is dropped rather than
    /// forwarded: the two halves ship together, so a shape neither
    /// understands is a bug rather than something to relay blindly.
    fn forward_notifications(&mut self, output: &mut impl Write) {
        let Some(peer) = self.session.as_mut() else { return };
        // What arrived during a review's wait first, since it came first.
        let mut payloads = std::mem::take(&mut self.held);
        if let Ok(fresh) = peer.poll() {
            payloads.extend(fresh);
        }
        for payload in payloads {
            if let Some(message) = selection_changed(&payload) {
                self.send(output, message);
            }
        }
    }

    /// One line to a message, or `None` after reporting why not.
    ///
    /// Unlike a `Content-Length` stream, a newline-delimited one stays
    /// synchronised through a bad message: the next newline is still the
    /// next message. So a line that will not parse is answered and the
    /// loop carries on, where `lspserver.rs` has to give up.
    fn decode(&self, line: &str, output: &mut impl Write) -> Option<Message> {
        let value = match json::parse(line) {
            Ok(value) => value,
            Err(e) => {
                // No id to answer against -- a message we cannot read is
                // a message whose id we do not know either.
                eprintln!("bish tool mcp-server: {e}");
                return None;
            }
        };
        match Message::from_value(&value) {
            Ok(message) => Some(message),
            Err(e) => {
                if let Ok(id) = json::query(&value, ".id")
                    && let Some(id) = id_of(id)
                {
                    self.send(output, Message::Response { id, result: Err(ResponseError { code: METHOD_NOT_FOUND, message: e }) });
                } else {
                    eprintln!("bish tool mcp-server: {e}");
                }
                None
            }
        }
    }

    fn handle(&mut self, message: Message, output: &mut impl Write) {
        match message {
            Message::Request { id, method, params } => {
                let result = self.request(&method, &params);
                self.send(output, Message::Response { id, result });
            }
            Message::Notification { method, params } => self.notification(&method, &params),
            // This server asks its peer for nothing, so anything
            // arriving as a response answers a question never put.
            Message::Response { .. } => {}
        }
    }

    fn send(&self, output: &mut impl Write, message: Message) {
        // One message, one line. A write that fails means the peer is
        // gone, which the next read discovers; there is no useful
        // recovery here and nowhere to report it that the peer sees.
        let _ = writeln!(output, "{}", json::compact_print(&message.to_value()));
        let _ = output.flush();
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, ResponseError> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            // Liveness, and nothing else: an empty result is the whole
            // of the answer the specification asks for.
            "ping" => Ok(object(vec![])),
            "tools/list" => Ok(object(vec![("tools", Value::Array(self.tools()))])),
            "tools/call" => Ok(self.call(params)),
            other => Err(ResponseError { code: METHOD_NOT_FOUND, message: format!("bish serves no {other}") }),
        }
    }

    /// Notifications have no answer by definition, so an unrecognized
    /// one is not an error -- there is nowhere to report it and nothing
    /// the peer would do about it. `notifications/initialized` and a
    /// client's own connection notices all land here.
    fn notification(&mut self, _method: &str, _params: &Value) {}

    fn initialize(&mut self, _params: &Value) -> Value {
        self.initialized = true;
        object(vec![
            // What bish speaks, whatever was asked for. A client that
            // wants a different revision is a client something in front
            // of bish has to reconcile -- see `PROTOCOL_VERSION`.
            ("protocolVersion", string(PROTOCOL_VERSION)),
            // Only `tools`. No resources, no prompts, no logging --
            // declaring a capability is promising to serve it, and a
            // client is entitled to skip asking about anything not
            // declared here.
            ("capabilities", object(vec![("tools", object(vec![("listChanged", Value::Bool(true))]))])),
            ("serverInfo", object(vec![("name", string(SERVER_NAME)), ("version", string(env!("CARGO_PKG_VERSION")))])),
        ])
    }

    /// Every tool this server has, as `tools/list` reports them.
    ///
    /// Deliberately short. A client discovers what is here rather than
    /// assuming, so an absent tool costs whatever that one tool did and
    /// nothing else -- which is what makes starting small safe.
    fn tools(&self) -> Vec<Value> {
        // Nothing to report on, so nothing to offer. A client reads this
        // list rather than assuming one, so an empty one costs exactly
        // the tools that are not in it.
        if self.session.is_none() {
            return Vec::new();
        }
        vec![
            tool(
                "getDiagnostics",
                "Get diagnostics from bish for a file, or for every file it has open",
                vec![("uri", "string", "Optional file:// URI. Omitted, this reports every open file.")],
            ),
            tool("closeAllDiffTabs", "Close any diff views bish has open", Vec::new()),
            tool(
                "openDiff",
                "Show a proposed new version of a file in bish, to be gone through change by change",
                vec![
                    ("old_file_path", "string", "The file being changed."),
                    ("new_file_path", "string", "Where the new version goes; the same file, for an edit."),
                    ("new_file_contents", "string", "The whole proposed text."),
                    ("tab_name", "string", "What to call the review."),
                ],
            ),
            tool("close_tab", "Close a diff view by name", vec![("tab_name", "string", "The name it was opened with.")]),
        ]
    }

    fn call(&mut self, params: &Value) -> Value {
        let name = match json::query(params, ".name") {
            Ok(Value::Str(name)) => name.clone(),
            _ => return tool_result("tools/call needs a tool name", true),
        };
        let arguments = json::query(params, ".arguments").cloned().unwrap_or(Value::Null);
        match name.as_str() {
            "getDiagnostics" => self.get_diagnostics(&arguments),
            "openDiff" => self.open_diff(&arguments),
            // A review closes itself the moment it is decided, which is
            // before `openDiff` answers -- so by the time a client asks
            // for one to be closed, there is never one open. Answered
            // rather than refused because a client calls both of these
            // unprompted, and an error there is noise about nothing.
            "close_tab" => tool_result("TAB_CLOSED", false),
            "closeAllDiffTabs" => tool_result("CLOSED_0_DIFF_TABS", false),
            other => tool_result(&format!("bish has no tool called {other}"), true),
        }
    }

    /// Puts a proposed version of a file in front of whoever is at the
    /// session, and answers once they have been through it: the file as
    /// it now stands, written, or word that nothing was taken.
    ///
    /// Blocks this server until then, deliberately. The client asking is
    /// itself waiting on exactly this answer, and has nothing else it
    /// could usefully be told meanwhile.
    fn open_diff(&mut self, arguments: &Value) -> Value {
        let Some(proposed) = string_at(arguments, ".new_file_contents") else {
            return tool_result("openDiff needs new_file_contents", true);
        };
        let Some(path) = string_at(arguments, ".new_file_path").or_else(|| string_at(arguments, ".old_file_path")) else {
            return tool_result("openDiff needs a file path", true);
        };
        self.next_review += 1;
        let id = self.next_review;
        let request = ReviewRequest { id, path: path.into(), proposed, title: string_at(arguments, ".tab_name").unwrap_or_default() };
        let Some(peer) = self.session.as_mut() else {
            return tool_result("this bish is not reporting on any session", true);
        };
        if let Err(e) = peer.tell(json::compact_print(&request.to_value()).as_bytes()) {
            return tool_result(&format!("could not ask the session: {e}"), true);
        }
        match peer.wait_for(|payload| review_outcome(payload, id).is_some()) {
            Ok((answer, others)) => {
                self.held.extend(others);
                let block = |text: &str| object(vec![("type", string("text")), ("text", string(text))]);
                match review_outcome(&answer, id) {
                    // Two blocks, the second the file as written: what a
                    // client reads a saved review as.
                    Some(ReviewOutcome::Saved(text)) => {
                        object(vec![("content", Value::Array(vec![block("FILE_SAVED"), block(&text)])), ("isError", Value::Bool(false))])
                    }
                    _ => tool_result("DIFF_REJECTED", false),
                }
            }
            Err(e) => tool_result(&format!("the session went away before the review was decided: {e}"), true),
        }
    }

    fn get_diagnostics(&mut self, arguments: &Value) -> Value {
        let Some(peer) = self.session.as_mut() else {
            return tool_result("this bish is not reporting on any session", true);
        };
        let state = match ask_state(peer) {
            Ok(state) => state,
            Err(e) => return tool_result(&format!("could not ask the session: {e}"), true),
        };
        // A `file://` URI back to the path it names, percent-decoded, so
        // `file:///tmp/my%20notes.sh` is the file with a space in its
        // name. Anything else -- a bare path, a scheme bish does not
        // serve -- matches nothing, which is the honest answer rather
        // than an error.
        let wanted = string_at(arguments, ".uri").map(|uri| crate::url::to_file_path(&uri));
        let files: Vec<Value> = state
            .files
            .iter()
            .filter(|f| match &wanted {
                None => true,
                Some(path) => path.as_deref() == Some(f.path.as_path()),
            })
            .map(file_diagnostics)
            .collect();
        tool_result(&json::pretty_print(&Value::Array(files)), false)
    }
}

/// One tool as `tools/list` describes it, with a flat object schema --
/// which is every schema here, and all MCP's own tools use.
fn tool(name: &str, description: &str, params: Vec<(&str, &str, &str)>) -> Value {
    let properties: Vec<(String, Value)> = params
        .iter()
        .map(|(name, kind, about)| ((*name).to_string(), object(vec![("type", string(kind)), ("description", string(about))])))
        .collect();
    object(vec![
        ("name", string(name)),
        ("description", string(description)),
        // No `required`: every parameter here is optional, and saying so
        // by omission is what the schema means.
        ("inputSchema", object(vec![("type", string("object")), ("properties", Value::Object(properties))])),
    ])
}

/// One file's diagnostics, in the shape a client reads them in.
///
/// Three details here are not negotiable, and each one silently discards
/// the answer if it is wrong: the URI has to carry a `file://` scheme,
/// positions are 0-based, and the severity is a *name* rather than a
/// number.
fn file_diagnostics(file: &FileState) -> Value {
    let findings: Vec<Value> = file
        .diagnostics
        .iter()
        .map(|d| {
            object(vec![
                ("message", string(&d.message)),
                ("severity", string(mcp_severity(d.severity))),
                ("range", object(vec![("start", position(d.start)), ("end", position(d.end))])),
                ("source", string(d.source.as_deref().unwrap_or("bish"))),
                ("code", string(&d.code)),
            ])
        })
        .collect();
    object(vec![
        ("uri", string(&crate::url::from_file_path(&file.path))),
        ("linesInFile", Value::Number(file.line_count as f64)),
        ("diagnostics", Value::Array(findings)),
    ])
}

/// The session's own focus notice, turned into what a client listens
/// for. `None` when there is nothing worth saying -- an unnamed buffer,
/// or a pane that is not showing a file at all.
///
/// Named for the client's vocabulary rather than bish's, and that is the
/// point of it being here: everything below this file says "focus", and
/// only this line knows what a particular client calls it.
fn selection_changed(payload: &[u8]) -> Option<Message> {
    let text = std::str::from_utf8(payload).ok()?;
    let value = json::parse(text).ok()?;
    if string_at(&value, ".n")? != "focus" {
        return None;
    }
    let file = FileState::from_value(json::query(&value, ".file").ok()?)?;
    let (start, end) = match file.selection {
        Some((a, b, c, d)) => ((a, b), (c, d)),
        // No selection is still worth sending: it is how a client learns
        // the last one is over, and which file is in front now.
        None => ((0, 0), (0, 0)),
    };
    let uri = crate::url::from_file_path(&file.path);
    Some(Message::Notification {
        method: "selection_changed".to_string(),
        params: object(vec![
            ("text", string(&file.selected_text)),
            ("filePath", string(&file.path.to_string_lossy())),
            ("fileUrl", string(&uri)),
            ("selection", object(vec![("start", position(start)), ("end", position(end)), ("isEmpty", Value::Bool(file.selection.is_none()))])),
        ]),
    })
}

fn position((line, character): (usize, usize)) -> Value {
    object(vec![("line", Value::Number(line as f64)), ("character", Value::Number(character as f64))])
}

/// bish's four severities under the names a client expects. `Info` is
/// the one that is spelled differently at each end, which is exactly the
/// kind of thing this seam exists to absorb.
fn mcp_severity(severity: crate::bishedit::lint::Severity) -> &'static str {
    use crate::bishedit::lint::Severity;
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Information",
        Severity::Hint => "Hint",
    }
}

/// The other half of a JSON-RPC id, for the one place a message failed
/// to parse but its id survived.
fn id_of(value: &Value) -> Option<Id> {
    match value {
        Value::Number(n) => Some(Id::Number(*n as i64)),
        Value::Str(s) => Some(Id::Str(s.clone())),
        _ => None,
    }
}

/// Newline-delimited JSON, which is what MCP over stdio is framed as.
///
/// Smaller than `framing.rs` because it has less to do: there is no
/// header to parse, no length to disbelieve, and a message that will not
/// read leaves the stream in a known place rather than an unknown one.
/// The one thing it does have to do is not grow without bound on a peer
/// that never sends a newline.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    /// Set once a single line has run past `MAX_LINE`, so the rest of
    /// that line is dropped instead of buffered -- and dropped up to and
    /// including its newline, so the *next* line still parses.
    overrun: bool,
}

/// As much of one message as this will hold. Generous: a tool result
/// carrying a whole file's diagnostics is the big case and is nowhere
/// near this.
const MAX_LINE: usize = 16 * 1024 * 1024;

impl Lines {
    fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete line, without its newline, or `None` when there
    /// is not one yet. A `\r\n` line ending is accepted too: nothing in
    /// the specification promises which a peer writes.
    fn take_line(&mut self) -> Option<String> {
        loop {
            let Some(at) = self.buf.iter().position(|b| *b == b'\n') else {
                if self.buf.len() > MAX_LINE {
                    // Keep nothing; the line is already lost, and the
                    // point of noticing is to stop holding it.
                    self.overrun = true;
                    self.buf.clear();
                }
                return None;
            };
            let line = self.buf.drain(..=at).collect::<Vec<u8>>();
            if self.overrun {
                self.overrun = false;
                continue;
            }
            let line = &line[..at];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            // A blank line between messages is not a message.
            if line.is_empty() {
                continue;
            }
            return Some(String::from_utf8_lossy(line).into_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole session, driven the way `lspserver.rs`'s own tests drive
    /// theirs: bytes in, bytes out, no process and no pipes. `serve`
    /// takes a reader and a writer rather than stdin and stdout for
    /// exactly this.
    fn session(messages: &[Value]) -> (i32, Vec<Message>) {
        let mut input = String::new();
        for m in messages {
            input.push_str(&json::compact_print(m));
            input.push('\n');
        }
        raw_session(&input)
    }

    /// The same, over bytes written out by hand -- for the cases about
    /// the framing itself, which a well-formed session cannot reach.
    fn raw_session(input: &str) -> (i32, Vec<Message>) {
        let mut output: Vec<u8> = Vec::new();
        let code = Server::new().serve(&mut input.as_bytes(), &mut output);
        let text = String::from_utf8(output).expect("the server writes UTF-8");
        let out = text
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| Message::from_value(&json::parse(line).expect("well-formed JSON")).expect("a well-formed message"))
            .collect();
        (code, out)
    }

    fn request(id: f64, method: &str, params: Value) -> Value {
        object(vec![("jsonrpc", string("2.0")), ("id", Value::Number(id)), ("method", string(method)), ("params", params)])
    }

    fn notification(method: &str, params: Value) -> Value {
        object(vec![("jsonrpc", string("2.0")), ("method", string(method)), ("params", params)])
    }

    /// The handshake a test has to get past before it can ask for
    /// anything. What it asks for does not matter -- bish answers with
    /// its own either way -- so the common case says nothing, and only
    /// the test that is *about* the asking spells one out.
    fn initialize() -> Value {
        request(0.0, "initialize", object(vec![]))
    }

    fn initialize_asking(version: &str) -> Value {
        request(0.0, "initialize", object(vec![("protocolVersion", string(version))]))
    }

    /// The result for one id. Panics rather than returning an option:
    /// every test here knows which answers it asked for.
    fn result(messages: &[Message], id: f64) -> Value {
        for m in messages {
            if let Message::Response { id: Id::Number(n), result } = m
                && *n == id as i64
            {
                return result.clone().expect("a successful result");
            }
        }
        panic!("no result for id {id} in {messages:?}");
    }

    fn error(messages: &[Message], id: f64) -> ResponseError {
        for m in messages {
            if let Message::Response { id: Id::Number(n), result } = m
                && *n == id as i64
            {
                return result.clone().expect_err("an error");
            }
        }
        panic!("no error for id {id} in {messages:?}");
    }

    // The whole of "connected", as far as a client is concerned: a
    // protocol version it recognizes and a `serverInfo`. Those two are
    // the only things a client is entitled to refuse the connection
    // over, so they are the two this pins.
    #[test]
    fn the_handshake_says_who_this_is_and_what_it_speaks() {
        let (_, out) = session(&[initialize()]);
        let answer = result(&out, 0.0);
        assert_eq!(json::query(&answer, ".serverInfo.name"), Ok(&string("bish")));
        assert_eq!(json::query(&answer, ".serverInfo.version"), Ok(&string(env!("CARGO_PKG_VERSION"))));
        // bish's own, not a specification date, and saying out loud that
        // it is not to be relied on -- see PROTOCOL_VERSION.
        assert_eq!(json::query(&answer, ".protocolVersion"), Ok(&string("bish/unstable")));
        // Only what this server will actually serve. Declaring a
        // capability is promising to answer for it.
        assert_eq!(json::query(&answer, ".capabilities.tools.listChanged"), Ok(&Value::Bool(true)));
        assert_eq!(json::query(&answer, ".capabilities.resources"), Ok(&Value::Null));
        assert_eq!(json::query(&answer, ".capabilities.prompts"), Ok(&Value::Null));
    }

    // bish answers with its own surface version, whatever was asked for,
    // and it is not a specification date -- see `PROTOCOL_VERSION`. What
    // a particular client will accept is that client's business, and
    // reconciling the two is the bridge's.
    #[test]
    fn the_protocol_version_is_bishs_own_whatever_was_asked_for() {
        for asked in [PROTOCOL_VERSION, "2024-01-01", "1999-01-01", "", "whatever-a-client-wants"] {
            let (_, out) = session(&[initialize_asking(asked)]);
            assert_eq!(json::query(&result(&out, 0.0), ".protocolVersion"), Ok(&string(PROTOCOL_VERSION)), "{asked}");
        }
        // Including when the client says nothing about it at all.
        let (_, out) = session(&[request(0.0, "initialize", object(vec![]))]);
        assert_eq!(json::query(&result(&out, 0.0), ".protocolVersion"), Ok(&string(PROTOCOL_VERSION)));
    }

    // A notification has no answer by definition, so an unrecognized one
    // is silence rather than an error -- including the ones a client
    // sends about its own state, which this server has nothing to say
    // about and must not choke on.
    #[test]
    fn notifications_are_accepted_and_answered_with_nothing() {
        let (code, out) = session(&[
            initialize(),
            notification("notifications/initialized", Value::Null),
            notification("ide_connected", object(vec![("pid", Value::Number(1234.0))])),
            notification("notifications/cancelled", object(vec![("requestId", Value::Number(7.0))])),
            request(1.0, "ping", Value::Null),
        ]);
        assert_eq!(code, 0);
        // Two answers for two requests, and not one more.
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(result(&out, 1.0), object(vec![]));
    }

    #[test]
    fn an_unknown_method_is_an_error_and_not_a_silence() {
        let (_, out) = session(&[initialize(), request(1.0, "resources/list", Value::Null)]);
        let e = error(&out, 1.0);
        assert_eq!(e.code, METHOD_NOT_FOUND);
        assert!(e.message.contains("resources/list"), "{}", e.message);
    }

    // A tool that fails is a *successful* JSON-RPC response describing a
    // failure. Getting this backwards makes a client treat a routine "no
    // such thing" as a broken server.
    #[test]
    fn a_tool_failure_is_content_and_not_a_json_rpc_error() {
        let (_, out) = session(&[initialize(), request(1.0, "tools/call", object(vec![("name", string("nosuchtool"))]))]);
        let answer = result(&out, 1.0);
        assert_eq!(json::query(&answer, ".isError"), Ok(&Value::Bool(true)));
        assert_eq!(json::query(&answer, ".content[0].type"), Ok(&string("text")));
        let Ok(Value::Str(text)) = json::query(&answer, ".content[0].text") else { panic!("{answer:?}") };
        assert!(text.contains("nosuchtool"), "{text}");
    }

    // Newline framing, and the reason it was worth writing instead of
    // reusing `framing.rs`: a message that will not read leaves the
    // stream in a known place, so the next one still works.
    #[test]
    fn a_line_that_will_not_parse_does_not_lose_the_stream() {
        let (code, out) = raw_session("not json at all\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n");
        assert_eq!(code, 0);
        assert_eq!(result(&out, 1.0), object(vec![]));
    }

    // Whitespace between messages is not a message, and neither ending
    // is promised by the specification.
    #[test]
    fn blank_lines_and_carriage_returns_are_tolerated() {
        let (_, out) = raw_session("\r\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\r\n\n");
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(result(&out, 1.0), object(vec![]));
    }

    // A peer that never sends a newline must not be able to make this
    // process grow without bound, and the line *after* the one that was
    // dropped still has to parse.
    #[test]
    fn an_endless_line_is_dropped_rather_than_buffered() {
        let mut lines = Lines::default();
        lines.feed(&vec![b'x'; MAX_LINE + 1]);
        assert_eq!(lines.take_line(), None);
        assert!(lines.buf.is_empty(), "the oversized line is not still being held");
        lines.feed(b" still the same line\n{\"id\":1}\n");
        assert_eq!(lines.take_line().as_deref(), Some("{\"id\":1}"), "the next line is unaffected");
    }

    fn diag(message: &str, severity: crate::bishedit::lint::Severity, start: (usize, usize), end: (usize, usize)) -> Diag {
        Diag { message: message.to_string(), severity, start, end, source: None, code: "some-code".to_string() }
    }

    fn a_file() -> FileState {
        FileState {
            path: std::path::PathBuf::from("/tmp/bad.sh"),
            line_count: 5,
            focused: true,
            dirty: true,
            selection: Some((1, 0, 2, 4)),
            selected_text: "x=1\nif [".to_string(),
            diagnostics: vec![
                diag("unquoted", crate::bishedit::lint::Severity::Warning, (2, 5), (2, 7)),
                diag("also this", crate::bishedit::lint::Severity::Info, (3, 7), (3, 21)),
            ],
        }
    }

    // The private half: the two ends of one binary agreeing about a
    // shape neither a client nor the specification has any say over.
    #[test]
    fn the_editor_state_survives_the_socket() {
        let state = EditorState { files: vec![a_file(), FileState { focused: false, ..a_file() }] };
        let answered = answer(&state, br#"{"q":"state"}"#).expect("a state question is answered");
        let back = EditorState::from_value(&json::parse(std::str::from_utf8(&answered).unwrap()).unwrap());
        assert_eq!(back, state);

        // A question this side does not know is silence, not an error:
        // both halves ship together, so an unknown one is a bug on the
        // asking side rather than something to negotiate.
        assert_eq!(answer(&state, br#"{"q":"whatever"}"#), None);
        assert_eq!(answer(&state, b"not json"), None);
    }

    /// A `Server` wired to a fake session over a socketpair: the daemon
    /// itself needs a controlling terminal, and this exercises every
    /// byte of the protocol without one.
    fn with_session(state: EditorState, messages: &[Value]) -> Vec<Message> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        // The far end plays the daemon: read one question, answer it,
        // exactly as `answer_rpc_requests` does from the idle tick.
        let daemon = std::thread::spawn(move || {
            use crate::session::{Decoder, Message as Wire};
            use std::io::{Read, Write};
            let mut stream = theirs;
            let mut decoder = Decoder::default();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    return;
                }
                decoder.feed(&buf[..n]);
                while let Ok(Some(message)) = decoder.next_message() {
                    let Wire::Rpc(question) = message else { continue };
                    // A review is decided on the spot, the way someone at
                    // the session would decide it: kept, unless the
                    // proposal asks to be refused. The focus notice sent
                    // ahead of the answer is what a wait has to hold on
                    // to rather than drop.
                    if let Some(review) = ReviewRequest::from_question(&question) {
                        if let Some(focus) = state.focused().or(state.files.first())
                            && let Some(notice) = selection_notification(Some(focus))
                        {
                            let _ = stream.write_all(&Wire::RpcReply(notice).encode());
                        }
                        let outcome =
                            if review.proposed.contains("REFUSE") { ReviewOutcome::Rejected } else { ReviewOutcome::Saved(review.proposed.clone()) };
                        let _ = stream.write_all(&Wire::RpcReply(review_answer(review.id, &outcome)).encode());
                    } else if let Some(reply) = answer(&state, &question) {
                        let _ = stream.write_all(&Wire::RpcReply(reply).encode());
                    }
                }
            }
        });

        let mut input = String::new();
        for m in messages {
            input.push_str(&json::compact_print(m));
            input.push('\n');
        }
        let mut output: Vec<u8> = Vec::new();
        let mut server = Server { initialized: false, next_review: 0, held: Vec::new(), session: Some(crate::session::Peer::over(ours)) };
        server.serve(&mut input.as_bytes(), &mut output);
        drop(server);
        let _ = daemon.join();
        String::from_utf8(output)
            .expect("UTF-8")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| Message::from_value(&json::parse(line).unwrap()).unwrap())
            .collect()
    }

    // The whole of `openDiff` as a client sees it: a proposal goes in,
    // and the answer comes back only once somebody has decided -- in the
    // shape a client reads, two text blocks for a saved file and one for
    // a refused one. The focus notice the session sent while the review
    // was open is not lost to the wait.
    #[test]
    fn open_diff_answers_once_the_review_is_decided_and_keeps_what_arrived_meanwhile() {
        let file = FileState {
            path: "/tmp/x.rs".into(),
            line_count: 1,
            focused: true,
            dirty: false,
            selection: None,
            selected_text: String::new(),
            diagnostics: Vec::new(),
        };
        let open = |id: f64, contents: &str| {
            let arguments = object(vec![
                ("old_file_path", string("/tmp/x.rs")),
                ("new_file_path", string("/tmp/x.rs")),
                ("new_file_contents", string(contents)),
                ("tab_name", string("x.rs")),
            ]);
            request(id, "tools/call", object(vec![("name", string("openDiff")), ("arguments", arguments)]))
        };
        let out = with_session(EditorState { files: vec![file] }, &[initialize(), open(1.0, "fn main() {}\n"), open(2.0, "REFUSE\n")]);
        let texts = |id: f64| -> Vec<String> {
            let answer = result(&out, id);
            let Ok(Value::Array(blocks)) = json::query(&answer, ".content") else { panic!("{answer:?}") };
            blocks.iter().filter_map(|b| string_at(b, ".text")).collect()
        };
        assert_eq!(texts(1.0), vec!["FILE_SAVED", "fn main() {}\n"]);
        assert_eq!(texts(2.0), vec!["DIFF_REJECTED"]);
        assert!(
            out.iter().any(|m| matches!(m, Message::Notification { method, .. } if method == "selection_changed")),
            "the notice that arrived during the wait still reaches the client: {out:?}"
        );
    }

    #[test]
    fn a_review_request_reads_back_as_itself_and_its_answer_names_it() {
        let request = ReviewRequest { id: 7, path: "/tmp/x.rs".into(), proposed: "fn main() {}\n".to_string(), title: "edit x.rs".to_string() };
        let question = json::compact_print(&request.to_value());
        assert_eq!(ReviewRequest::from_question(question.as_bytes()), Some(request));
        assert_eq!(ReviewRequest::from_question(br#"{"q":"state"}"#), None, "another question is not a review");

        let saved = review_answer(7, &ReviewOutcome::Saved("text".to_string()));
        assert_eq!(review_outcome(&saved, 7), Some(ReviewOutcome::Saved("text".to_string())));
        assert_eq!(review_outcome(&saved, 8), None, "an answer is to one review, not the next one waiting");
        assert_eq!(review_outcome(&review_answer(7, &ReviewOutcome::Rejected), 7), Some(ReviewOutcome::Rejected));
        assert_eq!(review_outcome(br#"{"n":"focus","file":null}"#, 7), None, "a notification is not an answer");
    }

    // What a client is told exists. Short on purpose -- a client reads
    // this rather than assuming, so an absent tool costs that tool and
    // nothing else.
    #[test]
    fn the_tool_list_is_what_bish_can_actually_answer() {
        let out = with_session(EditorState::default(), &[initialize(), request(1.0, "tools/list", Value::Null)]);
        let listed = result(&out, 1.0);
        let Ok(Value::Array(tools)) = json::query(&listed, ".tools") else { panic!() };
        let names: Vec<String> = tools.iter().filter_map(|t| string_at(t, ".name")).collect();
        assert_eq!(names, vec!["getDiagnostics", "closeAllDiffTabs", "openDiff", "close_tab"]);
        assert_eq!(json::query(&tools[0], ".inputSchema.type"), Ok(&string("object")));
        assert_eq!(json::query(&tools[0], ".inputSchema.properties.uri.type"), Ok(&string("string")));

        // With nothing to report on there is nothing to offer, and
        // saying so is a working server, not a broken one.
        let (_, out) = session(&[initialize(), request(1.0, "tools/list", Value::Null)]);
        assert_eq!(json::query(&result(&out, 1.0), ".tools"), Ok(&Value::Array(vec![])));
    }

    // Three details a client silently discards the answer over: the
    // scheme, the base of the line numbers, and the spelling of the
    // severity. Each was read off a real client rather than guessed.
    #[test]
    fn diagnostics_come_out_in_the_shape_a_client_reads() {
        let state = EditorState { files: vec![a_file()] };
        let out = with_session(state, &[initialize(), request(1.0, "tools/call", object(vec![("name", string("getDiagnostics"))]))]);
        let answer = result(&out, 1.0);
        assert_eq!(json::query(&answer, ".isError"), Ok(&Value::Bool(false)));
        let Ok(Value::Str(text)) = json::query(&answer, ".content[0].text") else { panic!("{answer:?}") };
        let files = json::parse(text).expect("the text block is itself JSON");

        assert_eq!(json::query(&files, "[0].uri"), Ok(&string("file:///tmp/bad.sh")), "the scheme is not optional");
        assert_eq!(json::query(&files, "[0].linesInFile"), Ok(&Value::Number(5.0)));
        // 0-based, and carried through unchanged from what the editor
        // published.
        assert_eq!(json::query(&files, "[0].diagnostics[0].range.start.line"), Ok(&Value::Number(2.0)));
        assert_eq!(json::query(&files, "[0].diagnostics[0].range.start.character"), Ok(&Value::Number(5.0)));
        // A name, not a number -- and `Info` is spelled differently at
        // each end, which is the case that catches a missing conversion.
        assert_eq!(json::query(&files, "[0].diagnostics[0].severity"), Ok(&string("Warning")));
        assert_eq!(json::query(&files, "[0].diagnostics[1].severity"), Ok(&string("Information")));
        // A finding with no server behind it is bish's own.
        assert_eq!(json::query(&files, "[0].diagnostics[0].source"), Ok(&string("bish")));
    }

    #[test]
    fn a_uri_selects_one_file_and_an_unknown_one_selects_none() {
        let state = EditorState { files: vec![a_file(), FileState { path: std::path::PathBuf::from("/tmp/other.sh"), focused: false, ..a_file() }] };
        let call = |uri: &str| object(vec![("name", string("getDiagnostics")), ("arguments", object(vec![("uri", string(uri))]))]);
        let out = with_session(state.clone(), &[initialize(), request(1.0, "tools/call", call("file:///tmp/other.sh"))]);
        let answer = result(&out, 1.0);
        let Ok(Value::Str(text)) = json::query(&answer, ".content[0].text") else { panic!() };
        let files = json::parse(text).unwrap();
        assert_eq!(json::query(&files, "[0].uri"), Ok(&string("file:///tmp/other.sh")));
        assert_eq!(json::query(&files, "[1]"), Ok(&Value::Null), "only the one asked for");

        // A file bish does not have is an empty answer, not an error:
        // "I have nothing to say about that" is true and useful.
        let out = with_session(state, &[initialize(), request(1.0, "tools/call", call("file:///nowhere"))]);
        let answer = result(&out, 1.0);
        assert_eq!(json::query(&answer, ".isError"), Ok(&Value::Bool(false)));
        let Ok(Value::Str(text)) = json::query(&answer, ".content[0].text") else { panic!() };
        assert_eq!(json::parse(text).unwrap(), Value::Array(vec![]));
    }

    // A path is not a URI. One with a space, a `#` or anything outside
    // ASCII has to go out encoded -- `#` would otherwise start a fragment
    // -- and come back in decoded, or the file a client asks about is
    // never the file bish has.
    #[test]
    fn a_path_that_needs_encoding_goes_out_encoded_and_is_found_by_it() {
        let odd = std::path::PathBuf::from("/tmp/my notes/#1 caf\u{e9}.sh");
        let encoded = "file:///tmp/my%20notes/%231%20caf%C3%A9.sh";
        let state = EditorState {
            files: vec![
                FileState { path: std::path::PathBuf::from("/tmp/other.sh"), focused: false, ..a_file() },
                FileState { path: odd, ..a_file() },
            ],
        };
        let call = |uri: &str| object(vec![("name", string("getDiagnostics")), ("arguments", object(vec![("uri", string(uri))]))]);
        let files_for = |uri: &str| {
            let out = with_session(state.clone(), &[initialize(), request(1.0, "tools/call", call(uri))]);
            let answer = result(&out, 1.0);
            let Ok(Value::Str(text)) = json::query(&answer, ".content[0].text") else { panic!("{answer:?}") };
            json::parse(text).unwrap()
        };

        let files = files_for(encoded);
        assert_eq!(json::query(&files, "[0].uri"), Ok(&string(encoded)), "encoded going out");
        assert_eq!(json::query(&files, "[1]"), Ok(&Value::Null), "and only that file, found by it");
        // Not a URI at all, and not one naming a file: nothing, rather
        // than a guess at what was meant.
        assert_eq!(files_for("/tmp/other.sh"), Value::Array(vec![]));
        assert_eq!(files_for("https://example.com/tmp/other.sh"), Value::Array(vec![]));
    }

    // Registration is a side effect and nothing else. A reply here
    // would sit in the socket waiting to be mistaken for the answer to
    // the first real question, which is why `answer` gives none -- and
    // why the next question still gets its own answer and not this one.
    #[test]
    fn announcing_yourself_registers_without_answering() {
        let state = EditorState { files: vec![a_file()] };
        assert_eq!(answer(&state, br#"{"q":"hello"}"#), None);

        // And the session is still able to answer normally afterwards,
        // through the same connection the announcement went down.
        let out = with_session(state, &[initialize(), request(1.0, "tools/call", object(vec![("name", string("getDiagnostics"))]))]);
        let answered = result(&out, 1.0);
        let Ok(Value::Str(text)) = json::query(&answered, ".content[0].text") else { panic!() };
        assert_eq!(json::query(&json::parse(text).unwrap(), "[0].uri"), Ok(&string("file:///tmp/bad.sh")));
    }

    #[test]
    fn close_all_diff_tabs_answers_rather_than_refusing() {
        let out =
            with_session(EditorState::default(), &[initialize(), request(1.0, "tools/call", object(vec![("name", string("closeAllDiffTabs"))]))]);
        let answer = result(&out, 1.0);
        assert_eq!(json::query(&answer, ".isError"), Ok(&Value::Bool(false)));
        assert_eq!(json::query(&answer, ".content[0].text"), Ok(&string("CLOSED_0_DIFF_TABS")));
    }

    // The one place a client's own vocabulary is spoken. Everything
    // below this file calls it "focus"; only the translation knows what
    // a listener calls it, or that its positions are 0-based.
    #[test]
    fn a_focus_notice_becomes_the_notification_a_client_listens_for() {
        let notice = selection_notification(Some(&a_file())).expect("a notice");
        let Some(Message::Notification { method, params }) = selection_changed(&notice) else { panic!() };
        assert_eq!(method, "selection_changed");
        assert_eq!(json::query(&params, ".filePath"), Ok(&string("/tmp/bad.sh")));
        assert_eq!(json::query(&params, ".fileUrl"), Ok(&string("file:///tmp/bad.sh")));
        assert_eq!(json::query(&params, ".text"), Ok(&string("x=1\nif [")));
        assert_eq!(json::query(&params, ".selection.start.line"), Ok(&Value::Number(1.0)));
        assert_eq!(json::query(&params, ".selection.end.character"), Ok(&Value::Number(4.0)));
        assert_eq!(json::query(&params, ".selection.isEmpty"), Ok(&Value::Bool(false)));

        // No selection still says something: it is how a listener learns
        // the last one is over, and which file is in front now.
        let notice = selection_notification(Some(&FileState { selection: None, selected_text: String::new(), ..a_file() })).unwrap();
        let Some(Message::Notification { params, .. }) = selection_changed(&notice) else { panic!() };
        assert_eq!(json::query(&params, ".selection.isEmpty"), Ok(&Value::Bool(true)));

        // The path as it is, the URL encoded -- the same file either way.
        let odd = FileState { path: std::path::PathBuf::from("/tmp/my notes/#1 caf\u{e9}.sh"), ..a_file() };
        let notice = selection_notification(Some(&odd)).unwrap();
        let Some(Message::Notification { params, .. }) = selection_changed(&notice) else { panic!() };
        assert_eq!(json::query(&params, ".filePath"), Ok(&string("/tmp/my notes/#1 caf\u{e9}.sh")));
        assert_eq!(json::query(&params, ".fileUrl"), Ok(&string("file:///tmp/my%20notes/%231%20caf%C3%A9.sh")));

        // Nothing in front of the user is nothing to say.
        let notice = selection_notification(None).unwrap();
        assert!(selection_changed(&notice).is_none());
        // And a notice of a kind this side does not know is dropped
        // rather than relayed as something it is not.
        assert!(selection_changed(br#"{"n":"something-else"}"#).is_none());
    }

    // Half a message, then the rest -- what a pipe actually delivers.
    #[test]
    fn a_message_split_across_reads_is_answered_once_it_is_whole() {
        let mut lines = Lines::default();
        lines.feed(b"{\"jsonrpc\":\"2.0\",");
        assert_eq!(lines.take_line(), None);
        lines.feed(b"\"id\":1,\"method\":\"ping\"}");
        assert_eq!(lines.take_line(), None, "still no newline, so still not a message");
        lines.feed(b"\n");
        assert_eq!(lines.take_line().as_deref(), Some("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}"));
        assert_eq!(lines.take_line(), None);
    }
}
