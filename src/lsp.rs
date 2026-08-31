// The Language Server Protocol, as far as it is a *wire format*: the
// stdio framing, JSON-RPC 2.0 on top of it, and the position arithmetic
// that has to happen at the boundary because bish and LSP count columns
// differently. Deliberately nothing else -- no process, no editor, no
// `Shell`. This module can be exercised entirely by handing it bytes and
// reading back messages, which is the whole reason it is separate from
// the half that owns a running server.
//
// Same split, and the same reasoning, as `session.rs`'s own framing: a
// hand-rolled length-prefixed format whose encode and decode are tested
// directly against each other, rather than a parser bolted onto the
// thing that reads the socket.
//
// Messages are built on `json::Value` rather than a typed model of LSP.
// A full one is thousands of lines of optional fields that vary by
// server and by protocol version, and `json::query` already exists for
// reaching into a reply whose exact shape we don't want to commit to
// (`query(&result, ".contents.value")`). Typed structs earn their place
// only where bish *constructs* a message, which is a much smaller set.
//
// `allow(dead_code)`, the way theme.rs does it and for the same kind of
// reason: this is a complete, self-contained description of a wire
// format, and which parts of it have a caller yet is a fact about how
// far the client has been built, not about whether the protocol has
// those pieces. Half of it goes unused until document sync lands.
#![allow(dead_code)]

use crate::json::{self, Value};

// A body larger than this is refused outright rather than allocated.
// Nothing legitimate comes close -- the biggest message in practice is a
// `didChange` carrying a whole file, or a completion list a few hundred
// KB wide -- and `Content-Length` arrives as text from a process that
// may be malfunctioning, so the one number that drives an allocation
// gets a ceiling.
pub const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------

/// A JSON-RPC id. Ours are always numbers; a *server*'s own requests
/// (`workspace/configuration`, `client/registerCapability`) may use
/// either, and the reply has to echo back exactly what arrived -- which
/// is the only reason the string case exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Id {
    Number(i64),
    Str(String),
}

impl Id {
    fn to_value(&self) -> Value {
        match self {
            Id::Number(n) => Value::Number(*n as f64),
            Id::Str(s) => Value::Str(s.clone()),
        }
    }

    fn from_value(v: &Value) -> Option<Id> {
        match v {
            Value::Number(n) => Some(Id::Number(*n as i64)),
            Value::Str(s) => Some(Id::Str(s.clone())),
            _ => None,
        }
    }
}

/// An error object from a response's `error` field.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

/// One JSON-RPC message, in either direction. The three shapes are
/// distinguished exactly as the spec does: a `method` with an `id` is a
/// request, a `method` without one is a notification, and an `id` with
/// `result`/`error` is a response.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Request { id: Id, method: String, params: Value },
    Notification { method: String, params: Value },
    Response { id: Id, result: Result<Value, ResponseError> },
}

impl Message {
    pub fn to_value(&self) -> Value {
        let mut fields = vec![("jsonrpc".to_string(), Value::Str("2.0".to_string()))];
        match self {
            Message::Request { id, method, params } => {
                fields.push(("id".to_string(), id.to_value()));
                fields.push(("method".to_string(), Value::Str(method.clone())));
                fields.push(("params".to_string(), params.clone()));
            }
            Message::Notification { method, params } => {
                fields.push(("method".to_string(), Value::Str(method.clone())));
                fields.push(("params".to_string(), params.clone()));
            }
            Message::Response { id, result } => {
                fields.push(("id".to_string(), id.to_value()));
                match result {
                    Ok(value) => fields.push(("result".to_string(), value.clone())),
                    Err(e) => fields.push((
                        "error".to_string(),
                        Value::Object(vec![
                            ("code".to_string(), Value::Number(e.code as f64)),
                            ("message".to_string(), Value::Str(e.message.clone())),
                        ]),
                    )),
                }
            }
        }
        Value::Object(fields)
    }

    /// The inverse. `Err` for anything that isn't recognizably one of
    /// the three shapes -- a server that sends such a thing is broken,
    /// and guessing which shape was meant would only move the failure
    /// somewhere less obvious.
    pub fn from_value(v: &Value) -> Result<Message, String> {
        let Value::Object(fields) = v else {
            return Err("message is not a JSON object".to_string());
        };
        let field = |name: &str| fields.iter().find(|(k, _)| k == name).map(|(_, v)| v);
        let id = field("id").and_then(Id::from_value);
        let method = match field("method") {
            Some(Value::Str(m)) => Some(m.clone()),
            Some(_) => return Err("`method` is not a string".to_string()),
            None => None,
        };
        // `params` is optional in JSON-RPC; an absent one is `Null`
        // rather than an error, so a caller reading it with `json::query`
        // gets the same "absent means null" it gets everywhere else.
        let params = field("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => Ok(Message::Request { id, method, params }),
            (Some(method), None) => Ok(Message::Notification { method, params }),
            (None, Some(id)) => {
                let result = match field("error") {
                    Some(Value::Object(_)) => {
                        let error = field("error").unwrap();
                        let code = match json::query(error, ".code") {
                            Ok(Value::Number(n)) => *n as i64,
                            _ => 0,
                        };
                        let message = match json::query(error, ".message") {
                            Ok(Value::Str(s)) => s.clone(),
                            _ => String::new(),
                        };
                        Err(ResponseError { code, message })
                    }
                    // A response with neither `result` nor `error` is
                    // malformed by the spec, but the harmless reading is
                    // "succeeded with nothing," which is also what a
                    // `null` result means -- so it takes that reading
                    // rather than failing the whole message.
                    _ => Ok(field("result").cloned().unwrap_or(Value::Null)),
                };
                Ok(Message::Response { id, result })
            }
            (None, None) => Err("message has neither `method` nor `id`".to_string()),
        }
    }
}

/// One message, framed and ready to write to a server's stdin.
///
/// `Content-Length` counts *bytes*, not characters -- the one detail
/// this whole function exists to get right, since every string that
/// reaches here has been through a `char`-based codebase.
pub fn encode(message: &Message) -> Vec<u8> {
    let body = json::compact_print(&message.to_value());
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// The receiving half: bytes in (in whatever sizes a non-blocking read
/// happens to produce), whole messages out.
///
/// A state machine rather than a `read_message(fd)` function because
/// there is no blocking read to hide behind -- a message can and does
/// arrive split across reads, and half a header has to be kept until the
/// rest of it shows up.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    // Once the byte stream stops making sense there is no way to find
    // the next message boundary -- the framing is the only thing that
    // told us where one was. So a framing error is terminal for this
    // decoder: it is reported once, and after that nothing more is
    // decoded, rather than emitting a cascade of nonsense from a stream
    // we have lost our place in.
    failed: bool,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.failed {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// The next complete message, if the bytes fed so far contain one.
    /// `None` means "not yet" -- call again after feeding more.
    pub fn take_message(&mut self) -> Option<Result<Message, String>> {
        match self.take_body() {
            None => None,
            Some(Err(e)) => Some(Err(e)),
            Some(Ok(body)) => Some(match json::parse(&body) {
                // A body that isn't JSON is *not* a framing error: the
                // frame itself was well-formed, so the stream is still
                // synchronized and the next message is still findable.
                // Report it and carry on.
                Err(e) => Err(format!("malformed JSON body: {e}")),
                Ok(value) => Message::from_value(&value),
            }),
        }
    }

    fn take_body(&mut self) -> Option<Result<String, String>> {
        if self.failed {
            return None;
        }
        let header_end = find(&self.buf, b"\r\n\r\n")?;
        let header = match std::str::from_utf8(&self.buf[..header_end]) {
            Ok(h) => h,
            Err(_) => return Some(Err(self.fail("header is not valid UTF-8"))),
        };
        let mut length: Option<usize> = None;
        for line in header.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                return Some(Err(self.fail(&format!("header line without a colon: {line:?}"))));
            };
            // Header names are case-insensitive; every server in
            // practice writes `Content-Length`, but the spec doesn't
            // promise it and matching loosely costs nothing.
            if name.trim().eq_ignore_ascii_case("content-length") {
                match value.trim().parse::<usize>() {
                    Ok(n) if n <= MAX_CONTENT_LENGTH => length = Some(n),
                    Ok(n) => return Some(Err(self.fail(&format!("Content-Length {n} exceeds the {MAX_CONTENT_LENGTH}-byte limit")))),
                    Err(_) => return Some(Err(self.fail(&format!("unparseable Content-Length: {:?}", value.trim())))),
                }
            }
            // Every other header (`Content-Type`, and anything a server
            // invents) is ignored rather than rejected.
        }
        let Some(length) = length else {
            return Some(Err(self.fail("headers with no Content-Length")));
        };
        let body_start = header_end + 4;
        if self.buf.len() < body_start + length {
            // The header is complete but the body isn't. Leave
            // everything in place and re-parse the header next time --
            // it is a handful of bytes, and keeping no partial state
            // between calls is what makes this correct regardless of how
            // the reads happened to land.
            return None;
        }
        let body = self.buf[body_start..body_start + length].to_vec();
        self.buf.drain(..body_start + length);
        Some(match String::from_utf8(body) {
            Ok(s) => Ok(s),
            // The frame was well-formed, so this is recoverable in the
            // same way a malformed JSON body is -- we know exactly where
            // the next message starts.
            Err(_) => Err("message body is not valid UTF-8".to_string()),
        })
    }

    fn fail(&mut self, why: &str) -> String {
        self.failed = true;
        self.buf.clear();
        format!("LSP framing error, stream abandoned: {why}")
    }

    /// Whether a framing error has put this decoder out of action. The
    /// owner of the server treats this as "the connection is dead."
    pub fn is_failed(&self) -> bool {
        self.failed
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------

/// One finding from a `textDocument/publishDiagnostics`, still in the
/// protocol's own coordinates.
///
/// Named `Finding` rather than `Diagnostic` on purpose: `lint::
/// Diagnostic` is what the editor draws, and the two are genuinely
/// different things until a buffer has converted one into the other.
/// This one's positions are `(line, character)` in whatever encoding the
/// server negotiated; that one's are flat char offsets. Only something
/// holding the actual text can bridge them, which is why this type stops
/// here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// LSP's 1..=4. Kept as the number rather than mapped here, because
    /// the enum it maps to lives in `lint` and this module has no
    /// business knowing about it.
    pub severity: u8,
    pub code: String,
    pub source: Option<String>,
    pub message: String,
}

/// A whole `publishDiagnostics` payload: which document, which revision
/// of it, and what was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub uri: String,
    /// The document version these describe, when the server said. LSP
    /// makes it optional; absent means "no idea", and a client has
    /// little choice but to trust them.
    pub version: Option<u64>,
    pub findings: Vec<Finding>,
}

/// Reads a `textDocument/publishDiagnostics` payload. `None` if it isn't
/// one -- no uri, or a `diagnostics` that isn't an array.
///
/// Every string here comes from another process and ends up drawn into a
/// terminal, so all of them go through `sanitize` on the way in: one
/// place, at the boundary, rather than a rule every rendering path has
/// to remember. Exactly what `url::is_safe` exists for, one layer down.
pub fn publication(params: &Value) -> Option<Publication> {
    let Ok(Value::Str(uri)) = json::query(params, ".uri") else { return None };
    let Ok(Value::Array(items)) = json::query(params, ".diagnostics") else { return None };
    let version = match json::query(params, ".version") {
        Ok(Value::Number(v)) if *v >= 0.0 => Some(*v as u64),
        _ => None,
    };
    let findings = items.iter().filter_map(finding).collect();
    Some(Publication { uri: uri.clone(), version, findings })
}

fn finding(value: &Value) -> Option<Finding> {
    let start = position(json::query(value, ".range.start").ok()?)?;
    let end = position(json::query(value, ".range.end").ok()?)?;
    // A range whose end precedes its start is a server bug; taking it at
    // face value would produce an underline of negative width. Reading
    // it as an empty range at `start` keeps the finding, which is the
    // part the user actually needs.
    let end = if end < start { start } else { end };
    Some(Finding {
        start,
        end,
        severity: match json::query(value, ".severity") {
            Ok(Value::Number(s)) if (1.0..=4.0).contains(s) => *s as u8,
            // "If omitted it is up to the client to interpret" -- and
            // the safe interpretation is the loudest one: a finding
            // shown as an error that was meant as a hint is a small
            // annoyance, the reverse is a missed problem.
            _ => 1,
        },
        // A code may be a string or a number, and both are common.
        code: match json::query(value, ".code") {
            Ok(Value::Str(code)) => sanitize(code),
            Ok(Value::Number(code)) => format!("{}", *code as i64),
            _ => String::new(),
        },
        source: match json::query(value, ".source") {
            Ok(Value::Str(source)) => Some(sanitize(source)),
            _ => None,
        },
        message: match json::query(value, ".message") {
            Ok(Value::Str(message)) => sanitize(message),
            _ => String::new(),
        },
    })
}

fn position(value: &Value) -> Option<(usize, usize)> {
    let (Ok(Value::Number(line)), Ok(Value::Number(character))) = (json::query(value, ".line"), json::query(value, ".character")) else {
        return None;
    };
    if *line < 0.0 || *character < 0.0 {
        return None;
    }
    Some((*line as usize, *character as usize))
}

/// How long a message may be before it is cut. A compiler can produce a
/// diagnostic with a whole worked example in it; a gutter row cannot.
const MAX_MESSAGE: usize = 400;

/// Text from a server, made safe to draw.
///
/// Two separate hazards. Control characters would be spliced straight
/// into a terminal's escape-sequence stream -- the exact bug class
/// `url::is_safe` exists for -- and newlines and tabs would break the
/// one-line-per-finding shape every place that shows these assumes
/// (`rustc` messages are routinely several lines). So every whitespace
/// run collapses to one space and every other control character is
/// dropped, and the result is capped.
fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_MESSAGE));
    let mut pending_space = false;
    for c in text.chars() {
        if out.chars().count() >= MAX_MESSAGE {
            out.push('\u{2026}');
            break;
        }
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------
// Hover
// ---------------------------------------------------------------------

/// How many lines of hover to keep, and how wide. A server can return a
/// type signature with every trait bound spelled out; a popup floating
/// over someone's code cannot.
const MAX_HOVER_LINES: usize = 30;
const MAX_HOVER_WIDTH: usize = 120;

/// A `textDocument/hover` result as lines to draw, or `None` when the
/// server had nothing to say (a `null` result, or contents that are
/// empty once cleaned up) -- which is a perfectly ordinary answer and
/// the caller's cue to fall back to whatever it knows itself.
///
/// LSP allows three shapes here and all three are in the wild: the
/// current `MarkupContent { kind, value }`, the deprecated bare or
/// `{language, value}` `MarkedString`, and an array of those. Handling
/// only the first would mean no hover at all from a number of real
/// servers, so all three are read.
pub fn hover_lines(result: &Value) -> Option<Vec<String>> {
    let contents = json::query(result, ".contents").ok()?;
    let raw = match contents {
        // MarkupContent, and the `{language, value}` MarkedString, are
        // told apart by which fields they have -- both carry `value`.
        Value::Object(_) => match json::query(contents, ".value") {
            Ok(Value::Str(value)) => value.clone(),
            _ => return None,
        },
        Value::Str(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Value::Str(text) => Some(text.clone()),
                Value::Object(_) => match json::query(item, ".value") {
                    Ok(Value::Str(value)) => Some(value.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let lines = hover_text_lines(&raw);
    (!lines.is_empty()).then_some(lines)
}

/// Server markdown as plain lines.
///
/// Deliberately not a markdown *renderer*: the popup this feeds shows
/// plain text, and the one thing worth doing is not showing the
/// syntax. Code fences are dropped rather than kept, because a hover is
/// most often exactly one fenced signature and leaving the ``` in makes
/// the useful line look like debris. Everything else is left as written
/// -- a half-rendered `**bold**` would be worse than an honest one.
fn hover_text_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        if out.len() >= MAX_HOVER_LINES {
            out.push("\u{2026}".to_string());
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue;
        }
        // Same hazard as a diagnostic message: this is text from
        // another process on its way into a terminal. Line structure is
        // meaningful here, so only the *within*-line control characters
        // are dropped, with a tab becoming a space rather than nothing.
        let mut clean = String::new();
        for c in trimmed.chars() {
            if clean.chars().count() >= MAX_HOVER_WIDTH {
                clean.push('\u{2026}');
                break;
            }
            match c {
                '\t' => clean.push(' '),
                c if c.is_control() => {}
                c => clean.push(c),
            }
        }
        // A blank line between paragraphs is worth keeping; a run of
        // them, or ones at either end, is not.
        if clean.trim().is_empty() && (out.is_empty() || out.last().is_some_and(|l| l.trim().is_empty())) {
            continue;
        }
        out.push(clean);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------

/// Somewhere in some file: what `textDocument/definition` (and later
/// `references`, `documentSymbol`) answers with. Positions are still in
/// the server's own `(line, character)`, for the same reason `Finding`'s
/// are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Reads a definition-shaped result. Empty when the server had nothing
/// -- a `null` result is the ordinary way of saying "I don't know where
/// that is", not a failure.
///
/// Three shapes again, and again all three are in use: a single
/// `Location`, an array of them, and an array of `LocationLink` (the
/// newer form, which names its target with `targetUri` and carries both
/// a full `targetRange` and the narrower `targetSelectionRange` -- the
/// selection range is the one to jump to, since it is the identifier
/// itself rather than the whole declaration body).
pub fn locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Object(_) => location(result).into_iter().collect(),
        Value::Array(items) => items.iter().filter_map(location).collect(),
        _ => Vec::new(),
    }
}

fn location(value: &Value) -> Option<Location> {
    // A LocationLink names its target differently, and prefers the
    // narrower of its two ranges.
    if let Ok(Value::Str(uri)) = json::query(value, ".targetUri") {
        let range = match json::query(value, ".targetSelectionRange") {
            Ok(range @ Value::Object(_)) => range,
            _ => json::query(value, ".targetRange").ok()?,
        };
        return Some(Location { uri: uri.clone(), start: position(json::query(range, ".start").ok()?)?, end: position(json::query(range, ".end").ok()?)? });
    }
    let Ok(Value::Str(uri)) = json::query(value, ".uri") else { return None };
    let range = json::query(value, ".range").ok()?;
    Some(Location { uri: uri.clone(), start: position(json::query(range, ".start").ok()?)?, end: position(json::query(range, ".end").ok()?)? })
}

// ---------------------------------------------------------------------
// Edits
// ---------------------------------------------------------------------

/// One replacement a server wants made: put `text` where `start..end`
/// currently is. Positions are the server's own, as everywhere else
/// here.
///
/// This is LSP's `TextEdit`, and it is what `textDocument/formatting`,
/// `rename` and a code action's `WorkspaceEdit` are all made of --
/// which is why it is its own type rather than something formatting
/// owns. `lint::Fix` cannot stand in: it is deliberately a *single*
/// range (see its own doc comment), and these arrive in batches that
/// have to be applied together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub text: String,
}

/// Reads a `TextEdit[]` -- what `textDocument/formatting` answers with,
/// and what each file's entry in a `WorkspaceEdit` holds. `null` (the
/// server had nothing to change) reads as an empty list, which is a
/// real answer and not a failure.
pub fn text_edits(result: &Value) -> Vec<TextEdit> {
    let Value::Array(items) = result else { return Vec::new() };
    items.iter().filter_map(text_edit).collect()
}

fn text_edit(value: &Value) -> Option<TextEdit> {
    let range = json::query(value, ".range").ok()?;
    let start = position(json::query(range, ".start").ok()?)?;
    let end = position(json::query(range, ".end").ok()?)?;
    // A backwards range would delete nothing and insert in the wrong
    // place; read it as empty at the start, as a diagnostic's is.
    let end = if end < start { start } else { end };
    let Ok(Value::Str(text)) = json::query(value, ".newText") else { return None };
    // Deliberately *not* sanitized. Unlike a message or a hover, this
    // is text going into the buffer, where a tab or a newline is
    // meaningful and stripping control characters would silently
    // corrupt what the server asked for. A formatter's whole output is
    // whitespace decisions.
    Some(TextEdit { start, end, text: text.clone() })
}

/// A change to a whole project: which files, and what to do to each.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceEdit {
    /// One entry per file, in the order the server gave them.
    pub changes: Vec<(String, Vec<TextEdit>)>,
    /// Resource operations bish cannot perform -- creating, renaming or
    /// deleting files -- named so the caller can say what it is
    /// refusing.
    ///
    /// These are not ignorable. rust-analyzer sends a `RenameFile` when
    /// the thing being renamed owns a module file, and applying only
    /// the text half of that leaves the project broken. A caller that
    /// sees any of these must do nothing at all.
    pub unsupported: Vec<String>,
}

/// Reads a `WorkspaceEdit`, in either of the two shapes it comes in:
/// the older `changes` map from uri to edits, and `documentChanges`,
/// which is an array that may interleave `TextDocumentEdit`s with
/// resource operations.
///
/// `documentChanges` wins when both are present, as the spec says.
pub fn workspace_edit(result: &Value) -> WorkspaceEdit {
    let mut edit = WorkspaceEdit::default();
    if let Ok(Value::Array(items)) = json::query(result, ".documentChanges") {
        for item in items {
            // A resource operation names its own kind; a
            // `TextDocumentEdit` has none.
            if let Ok(Value::Str(kind)) = json::query(item, ".kind") {
                edit.unsupported.push(kind.clone());
                continue;
            }
            let Ok(Value::Str(uri)) = json::query(item, ".textDocument.uri") else { continue };
            let edits = json::query(item, ".edits").map(text_edits).unwrap_or_default();
            if !edits.is_empty() {
                edit.changes.push((uri.clone(), edits));
            }
        }
        return edit;
    }
    if let Ok(Value::Object(files)) = json::query(result, ".changes") {
        for (uri, edits) in files {
            let edits = text_edits(edits);
            if !edits.is_empty() {
                edit.changes.push((uri.clone(), edits));
            }
        }
    }
    edit
}

// ---------------------------------------------------------------------
// Code actions
// ---------------------------------------------------------------------

/// Something a server offers to do to the code here -- a quick fix for
/// a diagnostic, a refactor, an import to add.
// No `Eq`: `unresolved` is a `json::Value`, which holds `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeAction {
    /// What the picker shows.
    pub title: String,
    /// `quickfix`, `refactor.extract`, ... Empty when the server said
    /// nothing, which is common for plain commands.
    pub kind: String,
    /// The change to make, when the server sent it up front.
    pub edit: Option<WorkspaceEdit>,
    /// The action verbatim, for `codeAction/resolve`. A server is
    /// allowed to send actions without their edits and compute one only
    /// for the action actually chosen -- rust-analyzer does exactly
    /// this -- so an action with no `edit` is not necessarily an action
    /// with nothing to do.
    pub unresolved: Value,
    /// The name of the server-side command this action runs, for
    /// saying which one when there is something to explain.
    pub command: Option<String>,
    /// That same command verbatim -- `{command, arguments}` -- to hand
    /// straight back in `workspace/executeCommand`.
    ///
    /// Deliberately not sanitized, and deliberately kept whole: the
    /// `arguments` are the server's own opaque payload (rust-analyzer
    /// puts entire computed edits in there), so anything but echoing
    /// them back exactly is corruption.
    pub invocation: Option<Value>,
    /// A server can offer an action and say it does not apply here,
    /// with a reason. Shown, and refused if chosen.
    pub disabled: Option<String>,
}

/// Reads a `textDocument/codeAction` answer: an array mixing
/// `CodeAction`s and bare `Command`s, the older form.
pub fn code_actions(result: &Value) -> Vec<CodeAction> {
    let Value::Array(items) = result else { return Vec::new() };
    items.iter().filter_map(code_action).collect()
}

fn code_action(value: &Value) -> Option<CodeAction> {
    let Ok(Value::Str(title)) = json::query(value, ".title") else { return None };
    let title = sanitize(title);
    if title.is_empty() {
        return None;
    }
    // A bare `Command` has `command` as a string at the top level; a
    // `CodeAction`'s own `command` is an object with one inside.
    let (command, invocation) = match json::query(value, ".command") {
        // The bare-`Command` shape: this whole object is the command.
        Ok(Value::Str(name)) => (Some(sanitize(name)), Some(value.clone())),
        Ok(object @ Value::Object(_)) => match json::query(object, ".command") {
            Ok(Value::Str(name)) => (Some(sanitize(name)), Some(object.clone())),
            _ => (None, None),
        },
        _ => (None, None),
    };
    let edit = match json::query(value, ".edit") {
        Ok(edit @ Value::Object(_)) => Some(workspace_edit(edit)),
        _ => None,
    };
    Some(CodeAction {
        title,
        kind: match json::query(value, ".kind") {
            Ok(Value::Str(kind)) => sanitize(kind),
            _ => String::new(),
        },
        edit,
        unresolved: value.clone(),
        command,
        invocation,
        disabled: match json::query(value, ".disabled.reason") {
            Ok(Value::Str(reason)) => Some(sanitize(reason)),
            _ => None,
        },
    })
}

// ---------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------

/// One candidate from `textDocument/completion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// What the list shows.
    pub label: String,
    /// `CompletionItemKind` as its own name -- "function", "variable".
    /// Empty for a number outside the table.
    pub kind: String,
    /// The server's own one-line elaboration: a signature, a type.
    /// Empty when it gave none.
    pub detail: String,
    /// What to actually insert.
    pub insert: String,
    /// The range the server says to replace, in its own coordinates.
    /// `None` means it did not say, and the caller should replace the
    /// word being typed -- which is what every client does, but is a
    /// guess where this is the server's own answer.
    pub edit: Option<((usize, usize), (usize, usize))>,
    /// What to sort by. `sortText` when the server gave one, which is
    /// how a server puts the likely answers first, and the label
    /// otherwise.
    pub sort: String,
    /// `insertTextFormat: 2` -- `insert` is a snippet, in the notation
    /// `bishedit::snippet` reads, and goes into the buffer tentatively
    /// with a caret in its first tabstop rather than as finished text.
    pub snippet: bool,
}

impl Completion {
    /// What to show beside the label: the server's own elaboration when
    /// it gave one, and the kind otherwise -- so a row is never bare
    /// when there is something true to say about it.
    pub fn detail_or_kind(&self) -> String {
        if self.detail.is_empty() { self.kind.clone() } else { self.detail.clone() }
    }
}

fn completion_kind(kind: f64) -> String {
    const KINDS: [&str; 25] = [
        "text", "method", "function", "constructor", "field", "variable", "class", "interface", "module", "property", "unit", "value",
        "enum", "keyword", "snippet", "color", "file", "reference", "folder", "enum-member", "constant", "struct", "event", "operator",
        "type-parameter",
    ];
    let index = kind as usize;
    if (1..=KINDS.len()).contains(&index) { KINDS[index - 1].to_string() } else { String::new() }
}

/// Reads a `textDocument/completion` answer.
///
/// Two shapes, both in use: a bare `CompletionItem[]`, and a
/// `CompletionList { isIncomplete, items }`. `isIncomplete` is read and
/// discarded -- it means "ask again as the user types more", which this
/// client does not do (it fetches once and filters what it got), and
/// silently ignoring a field is better than pretending to honour it.
pub fn completions(result: &Value) -> Vec<Completion> {
    let items = match result {
        Value::Array(items) => items,
        Value::Object(_) => match json::query(result, ".items") {
            Ok(Value::Array(items)) => items,
            _ => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    items.iter().filter_map(completion).collect()
}

fn completion(value: &Value) -> Option<Completion> {
    let Ok(Value::Str(label)) = json::query(value, ".label") else { return None };
    let label = sanitize(label);
    if label.is_empty() {
        return None;
    }
    // `insertTextFormat` 2 means the text is a snippet, with `$1`/
    // `${1:name}` placeholders and a `$0` final position. bish has a
    // snippet engine (`bishedit::snippet`, behind `abbr`), but wiring
    // an LSP snippet into it is a feature of its own; until then the
    // placeholders are flattened to their default text, which gives
    // `foo(a, b)` rather than the literal `foo(${1:a}, ${2:b})` that
    // ignoring the field would insert.
    let snippet = matches!(json::query(value, ".insertTextFormat"), Ok(Value::Number(f)) if *f == 2.0);
    let mut edit = None;
    // A `textEdit` is the server saying exactly what to replace, which
    // beats guessing at the word under the cursor -- it is how a server
    // completes something the client would not have recognised as one
    // word, like a dotted path or an import.
    let text_edit = json::query(value, ".textEdit").ok().filter(|v| matches!(v, Value::Object(_)));
    let insert = if let Some(text_edit) = text_edit {
        // `InsertReplaceEdit` has `insert`/`replace` instead of
        // `range`; `insert` is the conservative one (it does not eat
        // what follows the cursor), so that is what is taken.
        let range = match json::query(text_edit, ".range") {
            Ok(range @ Value::Object(_)) => Some(range),
            _ => json::query(text_edit, ".insert").ok().filter(|v| matches!(v, Value::Object(_))),
        };
        if let Some(range) = range
            && let (Some(start), Some(end)) =
                (json::query(range, ".start").ok().and_then(position), json::query(range, ".end").ok().and_then(position))
        {
            edit = Some((start, end));
        }
        match json::query(text_edit, ".newText") {
            Ok(Value::Str(text)) => text.clone(),
            _ => label.clone(),
        }
    } else {
        match json::query(value, ".insertText") {
            Ok(Value::Str(text)) => text.clone(),
            _ => label.clone(),
        }
    };
    // `sanitize_insert`, not `sanitize`: this is text destined for the
    // buffer, and a multi-line snippet collapsed to one line is not the
    // completion the server offered.
    let insert = sanitize_insert(&insert);
    Some(Completion {
        label: label.clone(),
        kind: match json::query(value, ".kind") {
            Ok(Value::Number(k)) => completion_kind(*k),
            _ => String::new(),
        },
        detail: match json::query(value, ".detail") {
            Ok(Value::Str(detail)) => sanitize(detail),
            _ => String::new(),
        },
        insert: if insert.is_empty() { label.clone() } else { insert },
        edit,
        sort: match json::query(value, ".sortText") {
            Ok(Value::Str(sort)) => sort.clone(),
            _ => label,
        },
        snippet,
    })
}

/// The most characters a single completion may insert.
///
/// A whole file arriving as one completion is a server malfunctioning
/// or lying; a real snippet is a handful of lines.
const MAX_INSERT: usize = 4096;

/// Text on its way into the buffer, rather than into a one-line
/// message: newlines and tabs survive, since a snippet is routinely
/// several lines and indented, and everything else that cannot be
/// displayed is dropped.
///
/// This is the middle setting between `sanitize` (which collapses all
/// whitespace, for text that has to fit on one line) and no cleaning at
/// all (`text_edits`, where the server is rewriting the file and every
/// byte is deliberate).
fn sanitize_insert(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_INSERT));
    for c in text.chars() {
        if out.chars().count() >= MAX_INSERT {
            break;
        }
        if c.is_control() && c != '\n' && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------

/// One entry from `textDocument/documentSymbol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// LSP's `SymbolKind`, 1..=26, as its own name -- "function",
    /// "struct". Empty for a number outside the table, which is a
    /// server using an extension we have no name for.
    pub kind: String,
    /// How deeply nested, for indenting an outline. Always 0 for a
    /// server answering with the flat `SymbolInformation` form.
    pub depth: usize,
    pub uri: String,
    pub start: (usize, usize),
}

/// `SymbolKind` by its wire number. A plain table because that is what
/// the spec is; the names are the spec's own, lowercased.
fn symbol_kind(kind: f64) -> String {
    const KINDS: [&str; 26] = [
        "file", "module", "namespace", "package", "class", "method", "property", "field", "constructor", "enum", "interface", "function",
        "variable", "constant", "string", "number", "boolean", "array", "object", "key", "null", "enum-member", "struct", "event",
        "operator", "type-parameter",
    ];
    let index = kind as usize;
    if (1..=KINDS.len()).contains(&index) { KINDS[index - 1].to_string() } else { String::new() }
}

/// Reads a `textDocument/documentSymbol` answer, flattened depth-first
/// with each entry's nesting recorded so an outline can indent it.
///
/// Two shapes again, both in use: the hierarchical `DocumentSymbol`
/// (which carries `children` and no uri, since it is always about the
/// document that was asked) and the older flat `SymbolInformation`
/// (which carries a full `location`). `fallback_uri` is what the first
/// form's entries belong to.
pub fn symbols(result: &Value, fallback_uri: &str) -> Vec<Symbol> {
    let Value::Array(items) = result else { return Vec::new() };
    let mut out = Vec::new();
    for item in items {
        collect_symbol(item, fallback_uri, 0, &mut out);
    }
    out
}

fn collect_symbol(value: &Value, fallback_uri: &str, depth: usize, out: &mut Vec<Symbol>) {
    let Ok(Value::Str(name)) = json::query(value, ".name") else { return };
    let kind = match json::query(value, ".kind") {
        Ok(Value::Number(k)) => symbol_kind(*k),
        _ => String::new(),
    };
    // `SymbolInformation` puts the place in `location`; `DocumentSymbol`
    // has `selectionRange` (the name itself) and `range` (the whole
    // declaration), and the name is what an outline should jump to.
    let (uri, range) = match json::query(value, ".location") {
        Ok(location @ Value::Object(_)) => {
            let uri = match json::query(location, ".uri") {
                Ok(Value::Str(uri)) => uri.clone(),
                _ => fallback_uri.to_string(),
            };
            (uri, json::query(location, ".range").ok())
        }
        _ => {
            let range = match json::query(value, ".selectionRange") {
                Ok(range @ Value::Object(_)) => Some(range),
                _ => json::query(value, ".range").ok(),
            };
            (fallback_uri.to_string(), range)
        }
    };
    if let Some(range) = range
        && let Some(start) = json::query(range, ".start").ok().and_then(position)
    {
        out.push(Symbol { name: sanitize(name), kind, depth, uri, start });
    }
    if let Ok(Value::Array(children)) = json::query(value, ".children") {
        for child in children {
            collect_symbol(child, fallback_uri, depth + 1, out);
        }
    }
}

// ---------------------------------------------------------------------
// Languages
// ---------------------------------------------------------------------

/// bish's own language name (`fileeditor::language_of`) as the
/// `languageId` a server expects on `textDocument/didOpen`.
///
/// LSP defines a vocabulary for this, and it is a real vocabulary
/// rather than a free-for-all: a server decides whether a document is
/// any of its business by comparing this string, so `bash` where the
/// spec says `shellscript` means a shell language server silently
/// ignores every file bish opens.
///
/// Almost all of bish's names already are the spec's, which is why this
/// is a short table of exceptions over an identity fallback rather than
/// a full map. A name with no LSP equivalent at all (`roff`, `dotenv`,
/// `csv`) passes through unchanged: no server speaks them today, and
/// inventing a translation for a conversation nobody is having would be
/// guessing.
pub fn language_id(language: &str) -> &str {
    match language {
        // The one that actually bites. Every shell server (bash-language-
        // server, shellcheck-ls) matches on `shellscript`.
        "bash" => "shellscript",
        // The spec's name for "no particular language", which is what
        // `language_of` returns `text` for.
        "text" => "plaintext",
        other => other,
    }
}

// ---------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------

/// How a server counts the `character` half of a position.
///
/// LSP's historical default is UTF-16 code units, which is a genuine
/// hazard for this codebase: bish counts `char`s (Unicode scalar values)
/// absolutely everywhere -- `TextBuffer`'s lines are `Vec<char>`, and
/// `HighlightSpan`/`lint::Diagnostic` offsets index them. The two agree
/// exactly until a line contains a non-BMP character (an emoji, most
/// commonly), at which point every column after it is off by one per
/// such character.
///
/// LSP 3.17 made this negotiable, and `Utf32` *is* bish's own counting,
/// so that's what gets asked for first. The other two are what we accept
/// when a server can't do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// The wire names, in the order bish prefers them: our own counting
    /// first, then the one every server supports.
    pub const PREFERRED: [PositionEncoding; 3] = [PositionEncoding::Utf32, PositionEncoding::Utf16, PositionEncoding::Utf8];

    pub fn wire_name(self) -> &'static str {
        match self {
            PositionEncoding::Utf8 => "utf-8",
            PositionEncoding::Utf16 => "utf-16",
            PositionEncoding::Utf32 => "utf-32",
        }
    }

    pub fn from_wire_name(name: &str) -> Option<PositionEncoding> {
        match name {
            "utf-8" => Some(PositionEncoding::Utf8),
            "utf-16" => Some(PositionEncoding::Utf16),
            "utf-32" => Some(PositionEncoding::Utf32),
            _ => None,
        }
    }

    fn units(self, c: char) -> usize {
        match self {
            PositionEncoding::Utf8 => c.len_utf8(),
            PositionEncoding::Utf16 => c.len_utf16(),
            PositionEncoding::Utf32 => 1,
        }
    }
}

/// A bish column (a count of `char`s into `line`) as the server counts
/// it. Clamped to the line's end, so a column past it -- which the
/// editor's own "cursor one past the last character" convention produces
/// routinely -- maps to the position just past the last character rather
/// than being rejected.
pub fn to_server_column(line: &[char], char_col: usize, encoding: PositionEncoding) -> usize {
    if encoding == PositionEncoding::Utf32 {
        return char_col.min(line.len());
    }
    line.iter().take(char_col).map(|c| encoding.units(*c)).sum()
}

/// The inverse. A column that lands *inside* a character (only possible
/// from a server that miscounted, or from one using an encoding where a
/// character spans several units) resolves to the start of that
/// character rather than failing: an off-by-a-fraction position is still
/// pointing at a real place, and refusing it would mean discarding an
/// otherwise-good diagnostic.
pub fn from_server_column(line: &[char], server_col: usize, encoding: PositionEncoding) -> usize {
    if encoding == PositionEncoding::Utf32 {
        return server_col.min(line.len());
    }
    let mut units = 0usize;
    for (i, c) in line.iter().enumerate() {
        let next = units + encoding.units(*c);
        // `<`, not `<=`: a column landing anywhere strictly inside this
        // character -- including on its second UTF-16 code unit -- is
        // this character, so it rounds *down* to where the character
        // starts. Rounding up instead would push a diagnostic's start
        // past the very thing it is pointing at.
        if server_col < next {
            return i;
        }
        units = next;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<Result<Message, String>> {
        let mut decoder = Decoder::new();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.feed(chunk);
            while let Some(m) = decoder.take_message() {
                out.push(m);
            }
        }
        out
    }

    fn request(id: i64, method: &str) -> Message {
        Message::Request {
            id: Id::Number(id),
            method: method.to_string(),
            params: Value::Object(vec![("k".to_string(), Value::Str("v".to_string()))]),
        }
    }

    #[test]
    fn a_framed_message_round_trips() {
        let original = request(1, "initialize");
        let bytes = encode(&original);
        assert_eq!(decode_all(&[&bytes]), vec![Ok(original)]);
    }

    // Content-Length is a byte count, and every string in this codebase
    // has been through `char`-based handling to get here. A body with
    // multi-byte characters in it is the case that catches the mistake.
    #[test]
    fn content_length_counts_bytes_not_characters() {
        let message = Message::Notification {
            method: "window/logMessage".to_string(),
            params: Value::Str("héllo 🌍".to_string()),
        };
        let bytes = encode(&message);
        let header_end = find(&bytes, b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let declared: usize = header.strip_prefix("Content-Length: ").unwrap().parse().unwrap();
        let body = &bytes[header_end + 4..];
        assert_eq!(declared, body.len());
        assert!(body.len() > body.iter().filter(|b| **b < 0x80).count(), "the test body should have multi-byte characters in it");
        assert_eq!(decode_all(&[&bytes]), vec![Ok(message)]);
    }

    // The property the whole state machine exists for: a message split
    // across reads decodes identically to one that arrived whole. Byte
    // at a time is the worst case, so it is the one tested.
    #[test]
    fn a_stream_split_one_byte_at_a_time_decodes_the_same_messages() {
        let stream: Vec<u8> = [request(1, "initialize"), Message::Notification { method: "initialized".to_string(), params: Value::Null }, request(2, "shutdown")]
            .iter()
            .flat_map(encode)
            .collect();
        let whole = decode_all(&[&stream]);
        assert_eq!(whole.len(), 3);
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(decode_all(&chunks), whole);
        // ...and at a few other split sizes, since one byte at a time
        // never exercises a chunk that straddles a boundary.
        for size in [2, 7, 13, 64] {
            let chunks: Vec<&[u8]> = stream.chunks(size).collect();
            assert_eq!(decode_all(&chunks), whole, "chunk size {size}");
        }
    }

    #[test]
    fn an_incomplete_message_yields_nothing_until_the_rest_arrives() {
        let bytes = encode(&request(1, "initialize"));
        let mut decoder = Decoder::new();
        decoder.feed(&bytes[..bytes.len() - 1]);
        assert!(decoder.take_message().is_none());
        decoder.feed(&bytes[bytes.len() - 1..]);
        assert!(matches!(decoder.take_message(), Some(Ok(_))));
    }

    #[test]
    fn the_three_message_shapes_are_told_apart_by_id_and_method() {
        let response = Message::Response { id: Id::Number(7), result: Ok(Value::Bool(true)) };
        let error = Message::Response { id: Id::Str("abc".to_string()), result: Err(ResponseError { code: -32601, message: "Method not found".to_string() }) };
        let notification = Message::Notification { method: "textDocument/publishDiagnostics".to_string(), params: Value::Null };
        for message in [response, error, notification, request(1, "initialize")] {
            let bytes = encode(&message);
            assert_eq!(decode_all(&[&bytes]), vec![Ok(message.clone())], "{message:?}");
        }
    }

    // A server's own request may use a string id, and the reply has to
    // echo back exactly what arrived rather than a number we found
    // easier to store.
    #[test]
    fn a_string_id_survives_a_round_trip_as_a_string() {
        let raw = br#"{"jsonrpc":"2.0","id":"req-1","method":"workspace/configuration","params":{}}"#;
        let framed = [format!("Content-Length: {}\r\n\r\n", raw.len()).into_bytes(), raw.to_vec()].concat();
        let decoded = decode_all(&[&framed]);
        let Ok(Message::Request { id, .. }) = &decoded[0] else { panic!("{decoded:?}") };
        assert_eq!(*id, Id::Str("req-1".to_string()));
    }

    #[test]
    fn extra_headers_are_ignored_and_content_length_is_matched_case_insensitively() {
        let body = r#"{"jsonrpc":"2.0","method":"initialized","params":null}"#;
        let framed = format!("content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{body}", body.len());
        assert_eq!(decode_all(&[framed.as_bytes()]), vec![Ok(Message::Notification { method: "initialized".to_string(), params: Value::Null })]);
    }

    // A bad *body* leaves the stream synchronized -- we still know where
    // the next message starts, so decoding continues.
    #[test]
    fn a_malformed_body_is_reported_without_losing_the_next_message() {
        let bad = "{not json";
        let good = encode(&request(2, "shutdown"));
        let stream = [format!("Content-Length: {}\r\n\r\n{bad}", bad.len()).into_bytes(), good].concat();
        let decoded = decode_all(&[&stream]);
        assert_eq!(decoded.len(), 2);
        assert!(decoded[0].is_err(), "{decoded:?}");
        assert_eq!(decoded[1], Ok(request(2, "shutdown")));
    }

    // A bad *header*, by contrast, means we no longer know where any
    // boundary is -- so it is terminal rather than something to try to
    // resynchronize from.
    #[test]
    fn a_framing_error_is_terminal() {
        let good = encode(&request(2, "shutdown"));
        let stream = [b"Content-Length: not-a-number\r\n\r\n".to_vec(), good].concat();
        let mut decoder = Decoder::new();
        decoder.feed(&stream);
        assert!(decoder.take_message().unwrap().is_err());
        assert!(decoder.is_failed());
        assert!(decoder.take_message().is_none(), "nothing more should come out of an abandoned stream");
    }

    #[test]
    fn an_oversized_content_length_is_refused_rather_than_allocated() {
        let mut decoder = Decoder::new();
        decoder.feed(format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1).as_bytes());
        let error = decoder.take_message().unwrap().unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert!(decoder.is_failed());
    }

    // The whole reason `positionEncoding` gets negotiated: an astral-
    // plane character is one `char` to bish and two UTF-16 code units to
    // a server that never heard of the negotiation.
    #[test]
    fn columns_convert_between_bish_chars_and_each_server_encoding() {
        let line: Vec<char> = "a🌍b".chars().collect();
        assert_eq!(line.len(), 3);
        for (encoding, expected) in [(PositionEncoding::Utf32, 2), (PositionEncoding::Utf16, 3), (PositionEncoding::Utf8, 5)] {
            // The column just after the emoji: char 2 for us, further
            // along for anyone counting smaller units.
            assert_eq!(to_server_column(&line, 2, encoding), expected, "{encoding:?}");
            assert_eq!(from_server_column(&line, expected, encoding), 2, "{encoding:?}");
        }
        // Utf32 is bish's own counting, so it is the identity -- which
        // is the point of asking for it first.
        for col in 0..=line.len() {
            assert_eq!(to_server_column(&line, col, PositionEncoding::Utf32), col);
            assert_eq!(from_server_column(&line, col, PositionEncoding::Utf32), col);
        }
    }

    #[test]
    fn a_column_past_the_end_or_inside_a_character_still_lands_somewhere_real() {
        let line: Vec<char> = "a🌍".chars().collect();
        // Past the end (the editor's own "one past the last character"
        // cursor) clamps rather than overshooting.
        assert_eq!(to_server_column(&line, 99, PositionEncoding::Utf32), 2);
        assert_eq!(from_server_column(&line, 99, PositionEncoding::Utf16), 2);
        // Mid-character resolves to that character's own start.
        assert_eq!(from_server_column(&line, 2, PositionEncoding::Utf16), 1);
        assert_eq!(from_server_column(&line, 3, PositionEncoding::Utf8), 1);
    }

    #[test]
    fn every_shape_a_definition_may_come_back_in_is_read() {
        let one = json::parse(r#"{"uri":"file:///a.rs","range":{"start":{"line":3,"character":4},"end":{"line":3,"character":9}}}"#).unwrap();
        assert_eq!(locations(&one), vec![Location { uri: "file:///a.rs".to_string(), start: (3, 4), end: (3, 9) }]);

        let many = json::parse(
            r#"[{"uri":"file:///a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                {"uri":"file:///b.rs","range":{"start":{"line":9,"character":2},"end":{"line":9,"character":5}}}]"#,
        )
        .unwrap();
        assert_eq!(locations(&many).len(), 2);
        assert_eq!(locations(&many)[1].uri, "file:///b.rs");

        // A LocationLink names its target differently and carries two
        // ranges; the selection range is the identifier itself, which is
        // where a jump should land, rather than the whole declaration.
        let link = json::parse(
            r#"[{"targetUri":"file:///c.rs",
                "targetRange":{"start":{"line":10,"character":0},"end":{"line":20,"character":1}},
                "targetSelectionRange":{"start":{"line":10,"character":7},"end":{"line":10,"character":11}}}]"#,
        )
        .unwrap();
        assert_eq!(locations(&link), vec![Location { uri: "file:///c.rs".to_string(), start: (10, 7), end: (10, 11) }]);

        // ...falling back to the full range when that is all there is.
        let link_only = json::parse(
            r#"[{"targetUri":"file:///c.rs","targetRange":{"start":{"line":10,"character":0},"end":{"line":20,"character":1}}}]"#,
        )
        .unwrap();
        assert_eq!(locations(&link_only)[0].start, (10, 0));
    }

    #[test]
    fn a_server_that_does_not_know_where_something_is_says_so_with_null() {
        assert!(locations(&Value::Null).is_empty());
        assert!(locations(&json::parse("[]").unwrap()).is_empty());
        // One malformed entry does not cost the rest.
        let mixed = json::parse(r#"[{"nonsense":1},{"uri":"file:///a.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}]"#).unwrap();
        assert_eq!(mixed_len(&mixed), 1);
        fn mixed_len(v: &Value) -> usize {
            locations(v).len()
        }
    }

    #[test]
    fn both_shapes_of_a_completion_answer_are_read() {
        let bare = json::parse(r#"[{"label":"push","kind":2,"detail":"fn(&mut self, T)"}]"#).unwrap();
        let found = completions(&bare);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].label.as_str(), found[0].kind.as_str()), ("push", "method"));
        assert_eq!(found[0].detail, "fn(&mut self, T)");
        // Nothing said about what to insert: the label is it.
        assert_eq!(found[0].insert, "push");
        // Nothing said about what to replace either -- the caller
        // falls back to the word being typed.
        assert_eq!(found[0].edit, None);
        // And nothing said about ordering: sort by what is shown.
        assert_eq!(found[0].sort, "push");

        // The list form, whose `isIncomplete` is deliberately ignored.
        let list = json::parse(r#"{"isIncomplete":true,"items":[{"label":"a"},{"label":"b"}]}"#).unwrap();
        assert_eq!(completions(&list).len(), 2);
        assert!(completions(&Value::Null).is_empty());
    }

    // A textEdit is the server saying exactly what to replace, which
    // beats guessing at a word -- it is how a dotted path or an import
    // gets completed at all.
    #[test]
    fn a_text_edit_carries_both_the_text_and_the_range_to_replace() {
        let with_edit = json::parse(
            r#"[{"label":"std::fmt","textEdit":{"range":{"start":{"line":3,"character":4},"end":{"line":3,"character":9}},"newText":"std::fmt"}}]"#,
        )
        .unwrap();
        let found = completions(&with_edit);
        assert_eq!(found[0].edit, Some(((3, 4), (3, 9))));
        assert_eq!(found[0].insert, "std::fmt");

        // InsertReplaceEdit names two ranges; `insert` is the
        // conservative one, since it does not eat what follows.
        let insert_replace = json::parse(
            r#"[{"label":"x","textEdit":{"insert":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}},
                "replace":{"start":{"line":1,"character":0},"end":{"line":1,"character":9}},"newText":"xy"}}]"#,
        )
        .unwrap();
        assert_eq!(completions(&insert_replace)[0].edit, Some(((1, 0), (1, 2))));
    }

    // A snippet is kept in its own notation and flagged, so the editor
    // can splice it in tentatively (see `bishedit::snippet`) rather than
    // guessing at what the user meant to fill in.
    #[test]
    fn a_snippet_completion_keeps_its_tabstops_and_says_so() {
        let snippet = json::parse(r#"[{"label":"foo","insertText":"foo(${1:a}, ${2:b})$0","insertTextFormat":2}]"#).unwrap();
        let found = completions(&snippet);
        assert!(found[0].snippet);
        assert_eq!(found[0].insert, "foo(${1:a}, ${2:b})$0");
        // Without the format flag it is literal text -- and unflagged,
        // so nothing downstream reads the braces as tabstops.
        let literal = json::parse(r#"[{"label":"foo","insertText":"foo(${1:a})"}]"#).unwrap();
        assert!(!completions(&literal)[0].snippet);
        assert_eq!(completions(&literal)[0].insert, "foo(${1:a})");
    }

    // What goes into a buffer is cleaned differently from what goes into
    // a one-line message: a multi-line snippet collapsed to one line is
    // not the completion the server offered.
    #[test]
    fn insert_text_keeps_its_newlines_and_indentation() {
        let multi = json::parse(r#"[{"label":"fn","insertText":"fn $1() {\n\t$0\n}","insertTextFormat":2}]"#).unwrap();
        assert_eq!(completions(&multi)[0].insert, "fn $1() {\n\t$0\n}");
        // Everything else that cannot be displayed still goes.
        let control = json::parse(r#"[{"label":"x","insertText":"a\u0007b"}]"#).unwrap();
        assert_eq!(completions(&control)[0].insert, "ab");
    }

    #[test]
    fn a_completion_with_no_label_is_not_one() {
        let nameless = json::parse(r#"[{"kind":2},{"label":""},{"label":"real"}]"#).unwrap();
        let found = completions(&nameless);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].label, "real");
    }

    #[test]
    fn both_shapes_of_document_symbol_are_read_and_flattened() {
        // The hierarchical form: no uri of its own, and children nested.
        let tree = json::parse(
            r#"[{"name":"Outer","kind":23,
                 "range":{"start":{"line":0,"character":0},"end":{"line":9,"character":1}},
                 "selectionRange":{"start":{"line":0,"character":7},"end":{"line":0,"character":12}},
                 "children":[{"name":"inner","kind":6,
                   "range":{"start":{"line":2,"character":2},"end":{"line":4,"character":3}},
                   "selectionRange":{"start":{"line":2,"character":6},"end":{"line":2,"character":11}}}]}]"#,
        )
        .unwrap();
        let found = symbols(&tree, "file:///a.rs");
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].name.as_str(), found[0].kind.as_str(), found[0].depth), ("Outer", "struct", 0));
        // The *name*, not the whole declaration -- an outline should
        // jump to the identifier.
        assert_eq!(found[0].start, (0, 7));
        assert_eq!((found[1].name.as_str(), found[1].kind.as_str(), found[1].depth), ("inner", "method", 1));
        assert_eq!(found[1].uri, "file:///a.rs", "the hierarchical form names no uri of its own");

        // The flat form carries a full location.
        let flat = json::parse(
            r#"[{"name":"main","kind":12,"location":{"uri":"file:///b.rs","range":{"start":{"line":5,"character":3},"end":{"line":5,"character":7}}}}]"#,
        )
        .unwrap();
        let found = symbols(&flat, "file:///a.rs");
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].name.as_str(), found[0].kind.as_str(), found[0].uri.as_str()), ("main", "function", "file:///b.rs"));
        assert_eq!(found[0].depth, 0, "nothing nested in the flat form");
    }

    #[test]
    fn a_symbol_answer_that_says_nothing_useful_yields_nothing() {
        assert!(symbols(&Value::Null, "file:///a").is_empty());
        assert!(symbols(&json::parse("[]").unwrap(), "file:///a").is_empty());
        // No name, or no range at all: dropped, without costing the
        // rest of the answer.
        let mixed = json::parse(
            r#"[{"kind":12},{"name":"ok","kind":12,"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}]"#,
        )
        .unwrap();
        assert_eq!(symbols(&mixed, "file:///a").len(), 1);
        // A kind outside the table is a server extension we have no
        // name for, not a reason to drop the symbol.
        let odd = json::parse(r#"[{"name":"x","kind":99,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}]"#).unwrap();
        assert_eq!(symbols(&odd, "file:///a")[0].kind, "");
    }

    #[test]
    fn a_text_edit_list_is_read_with_its_text_left_exactly_as_sent() {
        let edits = json::parse(
            r#"[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4}},"newText":"\tif x {\n"},
                {"range":{"start":{"line":9,"character":2},"end":{"line":9,"character":2}},"newText":""}]"#,
        )
        .unwrap();
        let found = text_edits(&edits);
        assert_eq!(found.len(), 2);
        // A formatter's output *is* whitespace decisions, so tabs and
        // newlines must survive -- this is the one place server text is
        // not sanitized.
        assert_eq!(found[0].text, "\tif x {\n");
        assert_eq!((found[0].start, found[0].end), ((0, 0), (0, 4)));
        // An empty replacement over an empty range is a legal no-op and
        // still parses.
        assert_eq!(found[1].text, "");

        assert!(text_edits(&Value::Null).is_empty(), "a server with nothing to change");
        // One malformed entry does not cost the rest.
        let mixed = json::parse(r#"[{"newText":"no range"},{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}},"newText":"x"}]"#).unwrap();
        assert_eq!(text_edits(&mixed).len(), 1);
    }

    #[test]
    fn a_workspace_edit_is_read_from_either_shape() {
        let one_edit = r#"{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}},"newText":"new"}"#;
        // The older map form.
        let changes = json::parse(&format!(r#"{{"changes":{{"file:///a.rs":[{one_edit}],"file:///b.rs":[{one_edit}]}}}}"#)).unwrap();
        let edit = workspace_edit(&changes);
        assert_eq!(edit.changes.len(), 2);
        assert!(edit.unsupported.is_empty());
        assert_eq!(edit.changes[0].1[0].text, "new");

        // documentChanges, which wins when both are present.
        let both = json::parse(&format!(
            r#"{{"changes":{{"file:///ignored.rs":[{one_edit}]}},
                 "documentChanges":[{{"textDocument":{{"uri":"file:///c.rs","version":3}},"edits":[{one_edit}]}}]}}"#
        ))
        .unwrap();
        let edit = workspace_edit(&both);
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].0, "file:///c.rs");
    }

    // The case that must not be silently half-applied: rust-analyzer
    // sends a RenameFile when the renamed thing owns a module file, and
    // doing only the text half leaves the project broken.
    #[test]
    fn resource_operations_are_reported_rather_than_ignored() {
        let with_rename = json::parse(
            r#"{"documentChanges":[
                {"textDocument":{"uri":"file:///a.rs","version":1},
                 "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"x"}]},
                {"kind":"rename","oldUri":"file:///old.rs","newUri":"file:///new.rs"}]}"#,
        )
        .unwrap();
        let edit = workspace_edit(&with_rename);
        assert_eq!(edit.changes.len(), 1, "the text half is still read");
        assert_eq!(edit.unsupported, vec!["rename".to_string()], "...and so is the half we cannot do");
    }

    #[test]
    fn an_empty_or_absent_workspace_edit_is_empty_not_an_error() {
        assert_eq!(workspace_edit(&Value::Null), WorkspaceEdit::default());
        assert_eq!(workspace_edit(&json::parse(r#"{"changes":{}}"#).unwrap()), WorkspaceEdit::default());
        // A file listed with no edits contributes nothing.
        assert!(workspace_edit(&json::parse(r#"{"changes":{"file:///a":[]}}"#).unwrap()).changes.is_empty());
    }

    #[test]
    fn code_actions_are_read_with_their_edit_or_the_reason_there_is_none() {
        let one_edit = r#"{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}},"newText":"fixed"}"#;
        let actions = json::parse(&format!(
            r#"[{{"title":"Fix this","kind":"quickfix","edit":{{"changes":{{"file:///a.rs":[{one_edit}]}}}}}},
                {{"title":"Run a thing","command":{{"title":"t","command":"rust-analyzer.runSingle"}}}},
                {{"title":"Later","data":{{"id":7}}}},
                {{"title":"Not here","disabled":{{"reason":"no trait in scope"}}}}]"#
        ))
        .unwrap();
        let found = code_actions(&actions);
        assert_eq!(found.len(), 4);

        // Carries its edit up front.
        assert_eq!(found[0].kind, "quickfix");
        assert_eq!(found[0].edit.as_ref().unwrap().changes.len(), 1);

        // A command, which bish cannot run -- kept so the refusal can
        // say what it is rather than "nothing happened".
        assert_eq!(found[1].command.as_deref(), Some("rust-analyzer.runSingle"));
        assert!(found[1].edit.is_none());

        // No edit and no command: the server means to compute one only
        // if this action is chosen, which is what `unresolved` is for.
        assert!(found[2].edit.is_none() && found[2].command.is_none());
        assert_eq!(json::query(&found[2].unresolved, ".data.id"), Ok(&Value::Number(7.0)));

        // Offered but not applicable, with the server's own reason.
        assert_eq!(found[3].disabled.as_deref(), Some("no trait in scope"));
    }

    #[test]
    fn the_older_bare_command_form_is_read_too() {
        // Pre-3.8 servers answer with `Command`s, whose `command` is a
        // string at the top level rather than an object.
        let bare = json::parse(r#"[{"title":"Do it","command":"server.doIt"}]"#).unwrap();
        let found = code_actions(&bare);
        assert_eq!(found[0].command.as_deref(), Some("server.doIt"));
        assert!(code_actions(&Value::Null).is_empty());
        // A titleless entry is not an action anyone could pick.
        assert!(code_actions(&json::parse(r#"[{"kind":"quickfix"}]"#).unwrap()).is_empty());
    }

    fn hover(json_text: &str) -> Option<Vec<String>> {
        hover_lines(&json::parse(json_text).unwrap())
    }

    // All three shapes are in the wild, and handling only the current
    // one would mean no hover at all from a number of real servers.
    #[test]
    fn every_shape_a_server_may_return_hover_contents_in_is_read() {
        // MarkupContent -- the current form.
        assert_eq!(hover(r#"{"contents":{"kind":"markdown","value":"a line"}}"#), Some(vec!["a line".to_string()]));
        // A bare MarkedString.
        assert_eq!(hover(r#"{"contents":"a line"}"#), Some(vec!["a line".to_string()]));
        // A `{language, value}` MarkedString, told apart from
        // MarkupContent only by which fields it has -- both carry
        // `value`, which is what makes reading that field the right
        // rule for either.
        assert_eq!(hover(r#"{"contents":{"language":"rust","value":"fn f()"}}"#), Some(vec!["fn f()".to_string()]));
        // An array of them, joined.
        assert_eq!(
            hover(r#"{"contents":["one",{"language":"rust","value":"two"}]}"#),
            Some(vec!["one".to_string(), "two".to_string()])
        );
    }

    #[test]
    fn a_server_with_nothing_to_say_says_so_rather_than_showing_an_empty_popup() {
        assert_eq!(hover(r#"{"contents":null}"#), None);
        assert_eq!(hover("null"), None);
        assert_eq!(hover(r#"{"contents":{"kind":"markdown","value":""}}"#), None);
        // Whitespace and fences only: nothing left once cleaned up.
        assert_eq!(hover("{\"contents\":\"```rust\\n```\\n\\n\"}"), None);
    }

    // A hover is most often exactly one fenced signature, and leaving
    // the ``` in makes the useful line look like debris.
    #[test]
    fn markdown_is_flattened_to_lines_without_becoming_a_renderer() {
        let lines = hover("{\"contents\":{\"kind\":\"markdown\",\"value\":\"```rust\\nfn f() -> i32\\n```\\n\\n---\\n\\nReturns **a number**.\\n\\n\\n\"}}").unwrap();
        assert_eq!(
            lines,
            vec!["fn f() -> i32".to_string(), String::new(), "---".to_string(), String::new(), "Returns **a number**.".to_string()]
        );
        // Runs of blank lines collapse and trailing ones go, but a
        // single separating blank line is worth keeping. `**bold**` is
        // left exactly as written -- half-rendering it would be worse
        // than being honest about what it is.
    }

    #[test]
    fn hover_text_is_bounded_and_stripped_of_anything_a_terminal_would_act_on() {
        let value = format!("line\\u001b[31mred\\ttabbed\\n{}", "y\\n".repeat(100));
        let lines = hover(&format!(r#"{{"contents":"{value}"}}"#)).unwrap();
        assert_eq!(lines[0], "line[31mred tabbed", "an escape must not reach the terminal, and a tab becomes a space");
        assert!(lines.len() <= MAX_HOVER_LINES + 1, "{} lines", lines.len());
        assert_eq!(lines.last().unwrap(), "\u{2026}");

        let wide = "z".repeat(500);
        let lines = hover(&format!(r#"{{"contents":"{wide}"}}"#)).unwrap();
        assert!(lines[0].chars().count() <= MAX_HOVER_WIDTH + 1);
        assert!(lines[0].ends_with('\u{2026}'));
    }

    fn published(json_text: &str) -> Publication {
        publication(&json::parse(json_text).unwrap()).expect("a publication")
    }

    #[test]
    fn a_publication_reads_the_document_its_revision_and_every_finding() {
        let p = published(
            r#"{"uri":"file:///p/x.rs","version":7,"diagnostics":[
                 {"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":9}},
                  "severity":1,"code":"E0308","source":"rustc","message":"mismatched types"},
                 {"range":{"start":{"line":5,"character":0},"end":{"line":6,"character":1}},
                  "severity":4,"message":"unused"}]}"#,
        );
        assert_eq!(p.uri, "file:///p/x.rs");
        assert_eq!(p.version, Some(7));
        assert_eq!(p.findings.len(), 2);
        assert_eq!(
            p.findings[0],
            Finding {
                start: (2, 4),
                end: (2, 9),
                severity: 1,
                code: "E0308".to_string(),
                source: Some("rustc".to_string()),
                message: "mismatched types".to_string(),
            }
        );
        assert_eq!(p.findings[1].source, None);
        assert_eq!(p.findings[1].code, "");
    }

    // Everything here is text from another process on its way to a
    // terminal. The control-character case is the same bug class
    // `url::is_safe` exists for; the newline case is subtler and just as
    // real, since every place these are drawn is one line per finding.
    #[test]
    fn text_from_a_server_is_made_safe_to_draw() {
        let p = published(
            "{\"uri\":\"file:///x\",\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":1}},\
             \"message\":\"expected \\u001b[31mint\\u001b[0m,\\n  found str\\there\",\"source\":\"a\\nb\",\"code\":\"c\\u0007d\"}]}",
        );
        let f = &p.findings[0];
        assert_eq!(f.message, "expected [31mint[0m, found str here");
        assert!(!f.message.contains('\u{1b}'), "an escape would be spliced straight into the terminal");
        assert!(!f.message.contains('\n') && !f.message.contains('\t'));
        assert_eq!(f.source.as_deref(), Some("a b"));
        assert_eq!(f.code, "cd");
    }

    #[test]
    fn a_very_long_message_is_cut_rather_than_drawn_whole() {
        let long = "x".repeat(5000);
        let p = published(&format!(
            r#"{{"uri":"file:///x","diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}},"message":"{long}"}}]}}"#
        ));
        let message = &p.findings[0].message;
        assert!(message.chars().count() <= MAX_MESSAGE + 1, "{}", message.chars().count());
        assert!(message.ends_with('\u{2026}'));
    }

    #[test]
    fn the_awkward_shapes_a_real_server_actually_sends() {
        // A numeric code (tsserver, and every compiler with numbered
        // errors) is as common as a string one.
        let p = published(
            r#"{"uri":"file:///x","diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"code":2304,"message":"m"}]}"#,
        );
        assert_eq!(p.findings[0].code, "2304");
        // No severity at all: read as the loudest, because a hint shown
        // as an error is an annoyance and the reverse is a missed
        // problem.
        assert_eq!(p.findings[0].severity, 1);
        // No version: the server isn't saying, so nothing can be gated
        // on it.
        assert_eq!(p.version, None);

        // An end before its start would underline a negative width;
        // read as an empty range at the start, keeping the finding.
        let backwards = published(
            r#"{"uri":"file:///x","diagnostics":[{"range":{"start":{"line":3,"character":8},"end":{"line":1,"character":0}},"message":"m"}]}"#,
        );
        assert_eq!(backwards.findings[0].start, (3, 8));
        assert_eq!(backwards.findings[0].end, (3, 8));

        // A finding with no range at all is dropped, but the ones
        // around it survive -- one malformed entry must not cost the
        // whole publication.
        let mixed = published(
            r#"{"uri":"file:///x","diagnostics":[{"message":"no range"},{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"message":"good"}]}"#,
        );
        assert_eq!(mixed.findings.len(), 1);
        assert_eq!(mixed.findings[0].message, "good");
    }

    #[test]
    fn something_that_is_not_a_publication_is_not_read_as_one() {
        assert_eq!(publication(&json::parse(r#"{"diagnostics":[]}"#).unwrap()), None, "no uri");
        assert_eq!(publication(&json::parse(r#"{"uri":"file:///x"}"#).unwrap()), None, "no diagnostics");
        assert_eq!(publication(&Value::Null), None);
        // An empty list is a real publication: it is how a server
        // withdraws everything it previously reported.
        assert_eq!(published(r#"{"uri":"file:///x","diagnostics":[]}"#).findings, vec![]);
    }

    // The one that actually bites: a shell server matches on
    // `shellscript`, so bish's own `bash` would make it ignore every
    // file the editor opens.
    #[test]
    fn a_language_name_is_translated_only_where_lsp_uses_a_different_one() {
        assert_eq!(language_id("bash"), "shellscript");
        assert_eq!(language_id("text"), "plaintext");
        // Almost everything already agrees, which is why this is a table
        // of exceptions over an identity fallback.
        for same in ["rust", "python", "typescript", "json", "yaml", "markdown", "ruby", "elixir"] {
            assert_eq!(language_id(same), same);
        }
        // A name LSP has no equivalent for passes through rather than
        // being guessed at.
        assert_eq!(language_id("roff"), "roff");
    }

    #[test]
    fn encoding_names_round_trip_and_utf32_is_preferred() {
        assert_eq!(PositionEncoding::PREFERRED[0], PositionEncoding::Utf32);
        for encoding in PositionEncoding::PREFERRED {
            assert_eq!(PositionEncoding::from_wire_name(encoding.wire_name()), Some(encoding));
        }
        assert_eq!(PositionEncoding::from_wire_name("utf-7"), None);
    }
}
