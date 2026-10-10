// The Language Server Protocol as a wire format: JSON-RPC 2.0 over the
// stdio framing, and the position arithmetic at the boundary, since bish
// and LSP count columns differently. No process, editor or `Shell`, so
// it's tested by handing it bytes.
//
// Messages are `json::Value`, not a typed model of LSP: a full one is
// thousands of lines of optional fields varying by server and version,
// and `json::query` reaches into a reply without committing to its shape
// (`query(&result, ".contents.value")`). Types appear only where bish
// builds messages. The allow is because this describes the whole
// format, not just what the client uses.
#![allow(dead_code)]

use crate::json::{self, Value};

// The framing and its size cap are shared with DAP, in framing.rs.

/// A JSON-RPC id. Ours are numbers; a server's own requests
/// (`workspace/configuration`, `client/registerCapability`) may use
/// strings, and a reply must echo what arrived.
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

/// One JSON-RPC message in either direction, told apart as the spec does:
/// `method` and `id` is a request, `method` alone a notification, `id`
/// with `result` or `error` a response.
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

    /// The inverse. `Err` for anything not clearly one of the three shapes:
    /// a server sending that is broken, and guessing would hide it.
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
        // `params` is optional; absent is `Null`, as `json::query` reads
        // absent fields.
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
                    // Neither `result` nor `error` is malformed, but reads
                    // harmlessly as success with nothing, like a `null`
                    // result.
                    _ => Ok(field("result").cloned().unwrap_or(Value::Null)),
                };
                Ok(Message::Response { id, result })
            }
            (None, None) => Err("message has neither `method` nor `id`".to_string()),
        }
    }
}

/// One message framed for a server's stdin. `Content-Length` counts
/// bytes, not chars.
pub fn encode(message: &Message) -> Vec<u8> {
    crate::framing::encode(&json::compact_print(&message.to_value()))
}

/// The receiving half: bytes in, in whatever pieces non-blocking reads
/// give, whole messages out. A state machine, since a message can arrive
/// split and half a header must be kept.
pub struct Decoder {
    frames: crate::framing::Frames,
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder { frames: crate::framing::Frames::new("LSP") }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.frames.feed(bytes);
    }

    /// The next complete message, if the bytes fed so far hold one; `None`
    /// until more is fed.
    pub fn take_message(&mut self) -> Option<Result<Message, String>> {
        match self.frames.take_body()? {
            Err(e) => Some(Err(e)),
            // A body that isn't JSON isn't a framing error: the frame was
            // fine, so the next message is still findable.
            Ok(body) => Some(match json::parse(&body) {
                Err(e) => Err(format!("malformed JSON body: {e}")),
                Ok(value) => Message::from_value(&value),
            }),
        }
    }

    /// Whether a framing error stopped this decoder; the server's owner
    /// treats it as a dead connection.
    pub fn is_failed(&self) -> bool {
        self.frames.is_failed()
    }
}

/// One finding from `textDocument/publishDiagnostics`, in the protocol's
/// coordinates: `(line, character)` in the negotiated encoding, where
/// `lint::Diagnostic` has flat char offsets. Only something holding the
/// text can convert, hence the separate name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// LSP's 1..=4, kept as the number; the enum it maps to is `lint`'s.
    pub severity: u8,
    pub code: String,
    pub source: Option<String>,
    pub message: String,
}

/// A `publishDiagnostics` payload: the document, its revision, the
/// findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub uri: String,
    /// The document version these describe, if given. Optional in LSP;
    /// absent means unknown.
    pub version: Option<u64>,
    pub findings: Vec<Finding>,
}

/// Reads a `publishDiagnostics` payload, or `None` without a uri or a
/// `diagnostics` array. Every string comes from another process and will
/// be drawn in a terminal, so all go through `sanitize` here, at the
/// boundary, as `url::is_safe` does a layer down.
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
    // A range ending before it starts is a server bug; read as empty at
    // `start`, keeping the finding.
    let end = if end < start { start } else { end };
    Some(Finding {
        start,
        end,
        severity: match json::query(value, ".severity") {
            Ok(Value::Number(s)) if (1.0..=4.0).contains(s) => *s as u8,
            // "If omitted it is up to the client to interpret": an error,
            // since a hint shown as an error is an annoyance and the
            // reverse a missed problem.
            _ => 1,
        },
        // A code may be a string or a number.
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

/// How long a message may be: a compiler's can include a worked example,
/// and a gutter row can't.
const MAX_MESSAGE: usize = 400;

/// Text from a server made safe to draw. Control characters would go
/// straight into the terminal's escape stream (what `url::is_safe`
/// guards against), and newlines and tabs would break one finding per
/// line (`rustc`'s span several). So whitespace runs collapse to a space,
/// other control characters go, and the result is capped.
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

/// How many hover lines to keep, and how wide: a signature with every
/// bound spelled out can't float over the code.
const MAX_HOVER_LINES: usize = 30;
const MAX_HOVER_WIDTH: usize = 120;

/// A `textDocument/hover` result as lines, or `None` when the server had
/// nothing (`null`, or empty once cleaned), the caller's cue to fall
/// back. All three shapes LSP allows are in use: `MarkupContent`, the
/// deprecated `MarkedString` (bare or `{language, value}`), and an array
/// of those.
pub fn hover_lines(result: &Value) -> Option<Vec<String>> {
    let contents = json::query(result, ".contents").ok()?;
    let raw = match contents {
        // `MarkupContent` and `{language, value}` both carry `value`; their
        // other fields tell them apart.
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

/// Server markdown as plain lines, not rendered: code fences are dropped
/// (a hover is often one fenced signature), everything else left as
/// written, since half-rendered `**bold**` is worse than honest.
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
        // Text from another process on its way to a terminal, as for
        // diagnostics, but keeping lines: only control characters within a
        // line go, a tab becoming a space.
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
        // One blank line between paragraphs is kept; runs and ones at the
        // ends aren't.
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

/// Somewhere in some file, as `textDocument/definition` answers. Positions
/// are the server's `(line, character)`, as `Finding`'s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// Text the server wants drawn between two characters of a line
/// (`textDocument/inlayHint`): a parameter name before an argument, an
/// inferred type after a binding. Positions are the server's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub line: usize,
    pub character: usize,
    /// Already padded per `paddingLeft`/`paddingRight`: a hint is spliced in
    /// verbatim.
    pub label: String,
}

/// How many hints one request may produce. The request is for the
/// visible range, so this is generous, and stops a whole generated file
/// from making the splice the expensive part of a redraw.
const MAX_INLAY_HINTS: usize = 500;

/// How long a hint may be, so a fully qualified generic type doesn't
/// push the code off screen.
const MAX_HINT_LABEL: usize = 60;

/// Reads an `inlayHint` result. The label is a string, or an array of
/// parts with their own `value` (carrying links and tooltips bish doesn't
/// draw), concatenated.
pub fn inlay_hints(result: &Value) -> Vec<InlayHint> {
    let Value::Array(items) = result else { return Vec::new() };
    items
        .iter()
        .take(MAX_INLAY_HINTS)
        .filter_map(|item| {
            let (line, character) = position(json::query(item, ".position").ok()?)?;
            let label = match json::query(item, ".label") {
                Ok(Value::Str(text)) => sanitize(text),
                Ok(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(|part| match json::query(part, ".value") {
                        Ok(Value::Str(text)) => Some(sanitize(text)),
                        _ => None,
                    })
                    .collect(),
                _ => return None,
            };
            // An empty hint would still take a cell.
            if label.trim().is_empty() {
                return None;
            }
            let pad = |key: &str| matches!(json::query(item, key), Ok(Value::Bool(true)));
            let mut label = format!("{}{}{}", if pad(".paddingLeft") { " " } else { "" }, label, if pad(".paddingRight") { " " } else { "" });
            if label.chars().count() > MAX_HINT_LABEL {
                label = label.chars().take(MAX_HINT_LABEL - 1).chain(std::iter::once('…')).collect();
            }
            Some(InlayHint { line, character, label })
        })
        .collect()
}

/// How many lines a signature popup may take: one per overload, plus
/// attached documentation. Past this it covers the code it's about.
const MAX_SIGNATURE_LINES: usize = 8;

/// A `textDocument/signatureHelp` result as popup lines: the active
/// overload first and marked, then the rest, since a server's pick while
/// arguments are half-typed is a guess. The active parameter isn't
/// marked: resolving its offsets or label against text this doesn't own
/// could mark the wrong argument.
pub fn signature_help(result: &Value) -> Vec<String> {
    let Ok(Value::Array(signatures)) = json::query(result, ".signatures") else { return Vec::new() };
    if signatures.is_empty() {
        return Vec::new();
    }
    let active = match json::query(result, ".activeSignature") {
        Ok(Value::Number(n)) if *n >= 0.0 => (*n as usize).min(signatures.len() - 1),
        _ => 0,
    };
    // The active one first, then the rest in order.
    let order = std::iter::once(active).chain((0..signatures.len()).filter(|&i| i != active));
    let mut out = Vec::new();
    for i in order {
        let Ok(Value::Str(label)) = json::query(&signatures[i], ".label") else { continue };
        let marker = if i == active && signatures.len() > 1 { "> " } else { "" };
        out.push(format!("{marker}{}", sanitize(label)));
        if out.len() >= MAX_SIGNATURE_LINES {
            break;
        }
    }
    out
}

/// One occurrence of the thing under the cursor in this file, from
/// `textDocument/documentHighlight`. No uri: it's all one document.
/// `write` marks occurrences that assign (kind 3), apart from reads
/// (kinds 1 and 2, and anything invented).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub write: bool,
}

/// How many occurrences to mark: a name used everywhere in a generated
/// file can have thousands, and past a screenful they mean nothing.
const MAX_HIGHLIGHTS: usize = 512;

/// Reads a `documentHighlight` result. `null` is the usual answer on
/// whitespace or a comment, so it's empty, not an error.
pub fn highlights(result: &Value) -> Vec<Highlight> {
    let Value::Array(items) = result else { return Vec::new() };
    items
        .iter()
        .take(MAX_HIGHLIGHTS)
        .filter_map(|item| {
            let (Ok(start), Ok(end)) = (json::query(item, ".range.start"), json::query(item, ".range.end")) else {
                return None;
            };
            Some(Highlight {
                start: position(start)?,
                end: position(end)?,
                write: matches!(json::query(item, ".kind"), Ok(Value::Number(k)) if *k == 3.0),
            })
        })
        .collect()
}

/// Reads a definition-shaped result, empty when the server had nothing
/// (`null`). Three shapes in use: a `Location`, an array of them, or an
/// array of `LocationLink` (`targetUri`, and the narrower
/// `targetSelectionRange`, the identifier, which is where to jump).
pub fn locations(result: &Value) -> Vec<Location> {
    match result {
        Value::Object(_) => location(result).into_iter().collect(),
        Value::Array(items) => items.iter().filter_map(location).collect(),
        _ => Vec::new(),
    }
}

fn location(value: &Value) -> Option<Location> {
    // A `LocationLink` names its target differently and prefers the
    // narrower range.
    if let Ok(Value::Str(uri)) = json::query(value, ".targetUri") {
        let range = match json::query(value, ".targetSelectionRange") {
            Ok(range @ Value::Object(_)) => range,
            _ => json::query(value, ".targetRange").ok()?,
        };
        return Some(Location {
            uri: uri.clone(),
            start: position(json::query(range, ".start").ok()?)?,
            end: position(json::query(range, ".end").ok()?)?,
        });
    }
    let Ok(Value::Str(uri)) = json::query(value, ".uri") else { return None };
    let range = json::query(value, ".range").ok()?;
    Some(Location { uri: uri.clone(), start: position(json::query(range, ".start").ok()?)?, end: position(json::query(range, ".end").ok()?)? })
}

/// One replacement: `text` in place of `start..end`, in the server's
/// positions. LSP's `TextEdit`, which formatting, `rename` and code
/// actions' `WorkspaceEdit` are all made of. `lint::Fix` is a single
/// range, and these come in batches applied together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub text: String,
}

/// Reads a `TextEdit[]`, from `textDocument/formatting` or a
/// `WorkspaceEdit` entry. `null` (nothing to change) is an empty list.
pub fn text_edits(result: &Value) -> Vec<TextEdit> {
    let Value::Array(items) = result else { return Vec::new() };
    items.iter().filter_map(text_edit).collect()
}

fn text_edit(value: &Value) -> Option<TextEdit> {
    let range = json::query(value, ".range").ok()?;
    let start = position(json::query(range, ".start").ok()?)?;
    let end = position(json::query(range, ".end").ok()?)?;
    // A backwards range is read as empty at the start, as for diagnostics.
    let end = if end < start { start } else { end };
    let Ok(Value::Str(text)) = json::query(value, ".newText") else { return None };
    // Not sanitized: this text goes into the buffer, where tabs and
    // newlines mean something, and a formatter's output is whitespace
    // decisions.
    Some(TextEdit { start, end, text: text.clone() })
}

/// A change to a file itself rather than its text: a resource operation
/// in `documentChanges`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOperation {
    Create { uri: String, overwrite: bool, ignore_if_exists: bool },
    Rename { old_uri: String, new_uri: String, overwrite: bool, ignore_if_exists: bool },
    Delete { uri: String, recursive: bool, ignore_if_not_exists: bool },
}

/// A change to a whole project: the files, and what to do to each.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceEdit {
    /// One entry per file, in the server's order.
    pub changes: Vec<(String, Vec<TextEdit>)>,
    /// Files to create, rename or delete, each with how many of `changes`
    /// come before it: `documentChanges` is ordered, and rust-analyzer
    /// renames a module's file and then edits it under the new name.
    pub operations: Vec<(usize, FileOperation)>,
    /// Operations that couldn't be read (an unknown kind, a missing URI),
    /// named so the caller can say what it refuses. Applying the rest without
    /// them would leave the project broken, so a caller seeing any must do
    /// nothing.
    pub unsupported: Vec<String>,
}

/// Reads a `WorkspaceEdit` in either shape: the older `changes` map from
/// uri to edits, or `documentChanges`, an array mixing
/// `TextDocumentEdit`s with resource operations, which wins when both are
/// present, as the spec says.
pub fn workspace_edit(result: &Value) -> WorkspaceEdit {
    let mut edit = WorkspaceEdit::default();
    if let Ok(Value::Array(items)) = json::query(result, ".documentChanges") {
        for item in items {
            // A resource operation names its kind; a `TextDocumentEdit`
            // doesn't.
            if let Ok(Value::Str(kind)) = json::query(item, ".kind") {
                match file_operation(kind, item) {
                    Some(operation) => edit.operations.push((edit.changes.len(), operation)),
                    None => edit.unsupported.push(kind.clone()),
                }
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

// One resource operation, or `None` for an unknown kind or missing
// URIs. An omitted option is false, per the spec.
fn file_operation(kind: &str, item: &Value) -> Option<FileOperation> {
    let uri = |at: &str| match json::query(item, at) {
        Ok(Value::Str(uri)) => Some(uri.clone()),
        _ => None,
    };
    let option = |name: &str| matches!(json::query(item, &format!(".options.{name}")), Ok(Value::Bool(true)));
    match kind {
        "create" => Some(FileOperation::Create { uri: uri(".uri")?, overwrite: option("overwrite"), ignore_if_exists: option("ignoreIfExists") }),
        "rename" => Some(FileOperation::Rename {
            old_uri: uri(".oldUri")?,
            new_uri: uri(".newUri")?,
            overwrite: option("overwrite"),
            ignore_if_exists: option("ignoreIfExists"),
        }),
        "delete" => {
            Some(FileOperation::Delete { uri: uri(".uri")?, recursive: option("recursive"), ignore_if_not_exists: option("ignoreIfNotExists") })
        }
        _ => None,
    }
}

/// Something a server offers to do to the code here: a quick fix, a
/// refactor, an import.
// No `Eq`: `unresolved` is a `json::Value`, which holds `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeAction {
    /// What the picker shows.
    pub title: String,
    /// `quickfix`, `refactor.extract`, and so on; often empty for plain
    /// commands.
    pub kind: String,
    /// The change to make, when the server sent it up front.
    pub edit: Option<WorkspaceEdit>,
    /// The action verbatim, for `codeAction/resolve`: a server may send
    /// actions without edits and compute one only for the chosen action, as
    /// rust-analyzer does, so a missing `edit` doesn't mean nothing to do.
    pub unresolved: Value,
    /// The server-side command's name, for explanations.
    pub command: Option<String>,
    /// That command verbatim, `{command, arguments}`, for
    /// `workspace/executeCommand`. Not sanitized: the arguments are the
    /// server's opaque payload (rust-analyzer puts whole edits there), and
    /// anything but echoing them back is corruption.
    pub invocation: Option<Value>,
    /// A server can offer an action as not applicable here, with a reason:
    /// shown, and refused if chosen.
    pub disabled: Option<String>,
}

/// Reads a `textDocument/codeAction` answer: `CodeAction`s mixed with
/// bare `Command`s, the older form.
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
    // A bare `Command` has a string `command` at the top; a `CodeAction`'s
    // `command` is an object holding one.
    let (command, invocation) = match json::query(value, ".command") {
        // A bare `Command`: the whole object is the command.
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

/// One candidate from `textDocument/completion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// What the list shows.
    pub label: String,
    /// `CompletionItemKind` by name ("function", "variable"); empty for an
    /// unknown number.
    pub kind: String,
    /// The server's one-line detail, a signature or type; empty if none.
    pub detail: String,
    /// What to actually insert.
    pub insert: String,
    /// The range the server says to replace, in its coordinates. `None`: the
    /// caller replaces the word being typed, as clients do, which is a
    /// guess.
    pub edit: Option<((usize, usize), (usize, usize))>,
    /// The sort key: `sortText`, how servers put likely answers first, else
    /// the label.
    pub sort: String,
    /// `insertTextFormat: 2`: `insert` is a snippet in `bishedit::snippet`'s
    /// notation, inserted live with the caret in its first tabstop.
    pub snippet: bool,
}

impl Completion {
    /// Shown beside the label: the server's detail, else the kind, so a row
    /// is never bare when something is true of it.
    pub fn detail_or_kind(&self) -> String {
        if self.detail.is_empty() { self.kind.clone() } else { self.detail.clone() }
    }
}

fn completion_kind(kind: f64) -> String {
    const KINDS: [&str; 25] = [
        "text",
        "method",
        "function",
        "constructor",
        "field",
        "variable",
        "class",
        "interface",
        "module",
        "property",
        "unit",
        "value",
        "enum",
        "keyword",
        "snippet",
        "color",
        "file",
        "reference",
        "folder",
        "enum-member",
        "constant",
        "struct",
        "event",
        "operator",
        "type-parameter",
    ];
    let index = kind as usize;
    if (1..=KINDS.len()).contains(&index) { KINDS[index - 1].to_string() } else { String::new() }
}

/// Reads a `textDocument/completion` answer: a bare `CompletionItem[]` or
/// a `CompletionList { isIncomplete, items }`. `isIncomplete` (ask again
/// as the user types) is ignored: this client fetches once and filters.
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
    // `insertTextFormat` 2: the text is a snippet with `$1`/`${1:name}`
    // placeholders and a final `$0`.
    let snippet = matches!(json::query(value, ".insertTextFormat"), Ok(Value::Number(f)) if *f == 2.0);
    let mut edit = None;
    // A `textEdit` says exactly what to replace, better than guessing the
    // word under the cursor, which is how a server completes a dotted path
    // or an import.
    let text_edit = json::query(value, ".textEdit").ok().filter(|v| matches!(v, Value::Object(_)));
    let insert = if let Some(text_edit) = text_edit {
        // `InsertReplaceEdit` has `insert` and `replace` instead of `range`;
        // `insert`, which doesn't eat what follows the cursor, is taken.
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
    // `sanitize_insert`, not `sanitize`: this goes into the buffer, and a
    // multi-line snippet on one line isn't what was offered.
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

/// The most characters one completion may insert: a whole file means a
/// malfunctioning server; a real snippet is a few lines.
const MAX_INSERT: usize = 4096;

/// Text going into the buffer: newlines and tabs survive (snippets span
/// indented lines), anything else undisplayable is dropped. Between
/// `sanitize` (one line) and no cleaning (`text_edits`, where every byte
/// is deliberate).
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

/// The names a server gave its token types and modifiers. Tokens arrive
/// as numbers with the meanings sent once in the `initialize` result, so
/// this is kept from there: read against the wrong legend, every token is
/// confidently the wrong colour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticLegend {
    pub types: Vec<String>,
    pub modifiers: Vec<String>,
}

impl SemanticLegend {
    /// Reads the legend out of a server's `semanticTokensProvider`.
    pub fn parse(capabilities: &Value) -> SemanticLegend {
        let names = |path: &str| match json::query(capabilities, path) {
            Ok(Value::Array(items)) => items
                .iter()
                .map(|item| match item {
                    Value::Str(name) => sanitize(name),
                    _ => String::new(),
                })
                .collect(),
            _ => Vec::new(),
        };
        SemanticLegend {
            types: names(".semanticTokensProvider.legend.tokenTypes"),
            modifiers: names(".semanticTokensProvider.legend.tokenModifiers"),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// One token in the server's coordinates: a run on one line, with the
/// legend's name for its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticToken {
    pub line: usize,
    /// In the negotiated position encoding, like every other column.
    pub start: usize,
    pub length: usize,
    /// The legend's type name ("function", "keyword", "parameter"). Empty
    /// for a number outside the legend, which the caller drops.
    pub kind: String,
    /// The modifier names set, in legend order: "declaration", "readonly".
    pub modifiers: Vec<String>,
}

/// The most tokens read from one answer: a large file's worth is normal,
/// several times that is a lost server, and each costs a span.
const MAX_SEMANTIC_TOKENS: usize = 20_000;

/// Decodes `textDocument/semanticTokens/full`: a flat array of 5-tuples
/// `[deltaLine, deltaStart, length, type, modifiers]`, each relative to
/// the previous token, `deltaStart` relative on the same line and
/// absolute when the line moved.
pub fn semantic_tokens(result: &Value, legend: &SemanticLegend) -> Vec<SemanticToken> {
    let Ok(Value::Array(data)) = json::query(result, ".data") else { return Vec::new() };
    let mut out = Vec::new();
    let mut line = 0usize;
    let mut start = 0usize;
    for group in data.chunks(5) {
        if group.len() < 5 || out.len() >= MAX_SEMANTIC_TOKENS {
            break;
        }
        let number = |value: &Value| match value {
            // Negative or fractional is malformed; 0 is harmless, and the
            // tokens around it are kept.
            Value::Number(n) if *n >= 0.0 => *n as usize,
            _ => 0,
        };
        let delta_line = number(&group[0]);
        let delta_start = number(&group[1]);
        line += delta_line;
        start = if delta_line == 0 { start + delta_start } else { delta_start };
        let length = number(&group[2]);
        if length == 0 {
            continue;
        }
        let kind = legend.types.get(number(&group[3])).cloned().unwrap_or_default();
        if kind.is_empty() {
            continue;
        }
        let bits = number(&group[4]);
        let modifiers = legend
            .modifiers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < usize::BITS as usize && bits & (1 << i) != 0)
            .map(|(_, name)| name.clone())
            .collect();
        out.push(SemanticToken { line, start, length, kind, modifiers });
    }
    out
}

/// One entry from `textDocument/documentSymbol`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// LSP's `SymbolKind` (1..=26) by name, "function", "struct"; empty for
    /// a number outside it, a server extension.
    pub kind: String,
    /// Nesting depth, for indenting an outline; 0 for a flat
    /// `SymbolInformation` answer.
    pub depth: usize,
    pub uri: String,
    pub start: (usize, usize),
}

/// `SymbolKind` by wire number, the spec's names lowercased.
fn symbol_kind(kind: f64) -> String {
    const KINDS: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum-member",
        "struct",
        "event",
        "operator",
        "type-parameter",
    ];
    let index = kind as usize;
    if (1..=KINDS.len()).contains(&index) { KINDS[index - 1].to_string() } else { String::new() }
}

/// Reads a `textDocument/documentSymbol` answer, flattened depth first
/// with each depth recorded. Two shapes: hierarchical `DocumentSymbol`
/// (`children`, no uri, so `fallback_uri` applies) and the older flat
/// `SymbolInformation` (a full `location`).
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
    // `SymbolInformation` has `location`; `DocumentSymbol` has
    // `selectionRange` (the name, where an outline jumps) and `range`.
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

/// bish's language name (`fileeditor::language_of`) as the `languageId`
/// `textDocument/didOpen` expects. Servers decide which documents are
/// theirs by this string, so `bash` where the spec says `shellscript`
/// would be ignored. Most of bish's names are the spec's, so this lists
/// exceptions; a name with no LSP equivalent (`roff`, `dotenv`, `csv`)
/// passes through.
pub fn language_id(language: &str) -> &str {
    match language {
        // Shell servers (bash-language-server, shellcheck-ls) match
        // `shellscript`.
        "bash" => "shellscript",
        // The spec's "no particular language", for `language_of`'s `text`.
        "text" => "plaintext",
        other => other,
    }
}

/// How a server counts a position's `character`. LSP's default is UTF-16
/// code units, while bish counts `char`s everywhere (`TextBuffer` lines
/// are `Vec<char>`, and offsets index them); they agree until a non-BMP
/// character, after which each one shifts the columns by one. LSP 3.17
/// negotiates this, and `Utf32` is bish's counting, so it's asked for
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    /// The wire names in bish's order of preference: its own counting, then
    /// the one every server supports.
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

/// A bish column (chars into `line`) as the server counts it, clamped to
/// the line's end, so the editor's one-past-the-end cursor maps to just
/// past the last character.
pub fn to_server_column(line: &[char], char_col: usize, encoding: PositionEncoding) -> usize {
    if encoding == PositionEncoding::Utf32 {
        return char_col.min(line.len());
    }
    line.iter().take(char_col).map(|c| encoding.units(*c)).sum()
}

/// The inverse. A column inside a character (a miscounting server, or a
/// multi-unit character) resolves to that character's start: it still
/// points somewhere real, and refusing it would drop a good diagnostic.
pub fn from_server_column(line: &[char], server_col: usize, encoding: PositionEncoding) -> usize {
    if encoding == PositionEncoding::Utf32 {
        return server_col.min(line.len());
    }
    let mut units = 0usize;
    for (i, c) in line.iter().enumerate() {
        let next = units + encoding.units(*c);
        // `<`: a column anywhere inside this character, its second UTF-16
        // unit included, rounds down to its start, so a diagnostic doesn't
        // start past what it points at.
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

    fn hints_of(text: &str) -> Vec<InlayHint> {
        inlay_hints(&json::parse(text).unwrap())
    }

    #[test]
    fn both_shapes_of_an_inlay_hint_label_are_read() {
        let found = hints_of(
            r#"[{"position":{"line":3,"character":9},"label":": u32"},
                {"position":{"line":4,"character":2},"label":[{"value":"Vec<"},{"value":"u8"},{"value":">"}]}]"#,
        );
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].line, found[0].character, found[0].label.as_str()), (3, 9, ": u32"));
        // A composite label's parts mean their concatenation.
        assert_eq!(found[1].label, "Vec<u8>");
    }

    #[test]
    fn padding_is_baked_into_the_label_because_that_is_what_gets_spliced() {
        let found = hints_of(
            r#"[{"position":{"line":0,"character":0},"label":"name:","paddingRight":true},
                {"position":{"line":0,"character":5},"label":"=>","paddingLeft":true,"paddingRight":true}]"#,
        );
        assert_eq!(found[0].label, "name: ");
        assert_eq!(found[1].label, " => ");
    }

    #[test]
    fn a_hint_with_nothing_in_it_is_not_a_hint() {
        assert!(hints_of("null").is_empty());
        assert!(hints_of(r#"[{"position":{"line":0,"character":0},"label":"  "}]"#).is_empty());
        assert!(hints_of(r#"[{"label":": u32"}]"#).is_empty(), "no position, nowhere to put it");
        assert!(hints_of(r#"[{"position":{"line":0,"character":0}}]"#).is_empty(), "no label");
    }

    #[test]
    fn an_enormous_hint_is_cut_rather_than_pushing_the_code_off_the_screen() {
        let long = "x".repeat(MAX_HINT_LABEL * 2);
        let text = format!(r#"[{{"position":{{"line":0,"character":0}},"label":"{long}"}}]"#);
        let found = hints_of(&text);
        assert_eq!(found[0].label.chars().count(), MAX_HINT_LABEL);
        assert!(found[0].label.ends_with('\u{2026}'));
    }

    #[test]
    fn a_server_answering_about_a_whole_generated_file_is_capped() {
        let one = r#"{"position":{"line":0,"character":0},"label":"h"}"#;
        let many = format!("[{}]", vec![one; MAX_INLAY_HINTS * 2].join(","));
        assert_eq!(hints_of(&many).len(), MAX_INLAY_HINTS);
    }

    fn signature_of(text: &str) -> Vec<String> {
        signature_help(&json::parse(text).unwrap())
    }

    #[test]
    fn the_active_overload_comes_first_and_is_marked() {
        let lines = signature_of(r#"{"signatures":[{"label":"fn one(a: u32)"},{"label":"fn two(a: u32, b: u32)"}],"activeSignature":1}"#);
        assert_eq!(lines, vec!["> fn two(a: u32, b: u32)".to_string(), "fn one(a: u32)".to_string()]);
    }

    #[test]
    fn a_lone_signature_is_not_marked_since_there_is_nothing_to_choose_between() {
        assert_eq!(signature_of(r#"{"signatures":[{"label":"fn one(a: u32)"}]}"#), vec!["fn one(a: u32)".to_string()]);
        // No `activeSignature` means the first, as servers that omit it
        // intend.
        let two = signature_of(r#"{"signatures":[{"label":"a()"},{"label":"b()"}]}"#);
        assert_eq!(two[0], "> a()");
    }

    #[test]
    fn nothing_to_show_is_empty_rather_than_a_box_with_nothing_in_it() {
        assert!(signature_of("null").is_empty());
        assert!(signature_of(r#"{"signatures":[]}"#).is_empty());
        // A signature with no label isn't one.
        assert!(signature_of(r#"{"signatures":[{"documentation":"hi"}]}"#).is_empty());
    }

    #[test]
    fn an_out_of_range_active_signature_does_not_pick_one_that_is_not_there() {
        let lines = signature_of(r#"{"signatures":[{"label":"a()"},{"label":"b()"}],"activeSignature":99}"#);
        assert_eq!(lines[0], "> b()", "clamped to the last, not panicking or dropping everything");
        let negative = signature_of(r#"{"signatures":[{"label":"a()"}],"activeSignature":-1}"#);
        assert_eq!(negative, vec!["a()".to_string()]);
    }

    #[test]
    fn a_server_with_an_absurd_number_of_overloads_does_not_cover_the_screen() {
        let many = format!(r#"{{"signatures":[{}]}}"#, vec![r#"{"label":"f()"}"#; MAX_SIGNATURE_LINES * 3].join(","));
        assert_eq!(signature_of(&many).len(), MAX_SIGNATURE_LINES);
    }

    // A label is drawn in a popup, so terminal-active characters go.
    #[test]
    fn a_signature_label_is_made_safe_to_draw() {
        let label = "fn f(\u{1b}[31mred\u{1b}[0m: u32)";
        let json_text = format!("{{\"signatures\":[{{\"label\":{}}}]}}", json::compact_print(&json::Value::Str(label.to_string())));
        let lines = signature_help(&json::parse(&json_text).unwrap());
        assert!(!lines[0].contains('\u{1b}'), "{lines:?}");
    }

    fn highlights_of(text: &str) -> Vec<Highlight> {
        highlights(&json::parse(text).unwrap())
    }

    #[test]
    fn document_highlights_carry_their_ranges_and_which_one_writes() {
        let found = highlights_of(
            r#"[{"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":7}},"kind":1},
                {"range":{"start":{"line":9,"character":0},"end":{"line":9,"character":3}},"kind":3},
                {"range":{"start":{"line":11,"character":8},"end":{"line":11,"character":11}}}]"#,
        );
        assert_eq!(found.len(), 3);
        assert_eq!((found[0].start, found[0].end), ((2, 4), (2, 7)));
        assert!(!found[0].write, "kind 1 is a plain reference");
        assert!(found[1].write, "kind 3 is a write");
        // `kind` is optional and absent reads as a read: marking a non-write
        // as an assignment sends people to the wrong line.
        assert!(!found[2].write);
    }

    #[test]
    fn a_null_or_malformed_highlight_answer_is_simply_empty() {
        // `null` is the usual answer on whitespace.
        assert!(highlights_of("null").is_empty());
        assert!(highlights_of(r#"{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}"#).is_empty());
        // An entry without a usable range is dropped, not its neighbours.
        let mixed = highlights_of(
            r#"[{"kind":1},
                {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}]"#,
        );
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn an_absurd_number_of_highlights_is_capped() {
        let one = r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}"#;
        let many = format!("[{}]", vec![one; MAX_HIGHLIGHTS * 2].join(","));
        assert_eq!(highlights_of(&many).len(), MAX_HIGHLIGHTS);
    }

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

    // Content-Length counts bytes; multi-byte characters catch a char count.
    #[test]
    fn content_length_counts_bytes_not_characters() {
        let message = Message::Notification { method: "window/logMessage".to_string(), params: Value::Str("héllo 🌍".to_string()) };
        let bytes = encode(&message);
        let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let declared: usize = header.strip_prefix("Content-Length: ").unwrap().parse().unwrap();
        let body = &bytes[header_end + 4..];
        assert_eq!(declared, body.len());
        assert!(body.len() > body.iter().filter(|b| **b < 0x80).count(), "the test body should have multi-byte characters in it");
        assert_eq!(decode_all(&[&bytes]), vec![Ok(message)]);
    }

    // A message split across reads decodes as one that came whole; a byte
    // at a time is the worst case.
    #[test]
    fn a_stream_split_one_byte_at_a_time_decodes_the_same_messages() {
        let stream: Vec<u8> =
            [request(1, "initialize"), Message::Notification { method: "initialized".to_string(), params: Value::Null }, request(2, "shutdown")]
                .iter()
                .flat_map(encode)
                .collect();
        let whole = decode_all(&[&stream]);
        assert_eq!(whole.len(), 3);
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(decode_all(&chunks), whole);
        // And other sizes, since single bytes never straddle a boundary
        // within a chunk.
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
        let error = Message::Response {
            id: Id::Str("abc".to_string()),
            result: Err(ResponseError { code: -32601, message: "Method not found".to_string() }),
        };
        let notification = Message::Notification { method: "textDocument/publishDiagnostics".to_string(), params: Value::Null };
        for message in [response, error, notification, request(1, "initialize")] {
            let bytes = encode(&message);
            assert_eq!(decode_all(&[&bytes]), vec![Ok(message.clone())], "{message:?}");
        }
    }

    // A server's string id is echoed back exactly.
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

    // A bad body leaves the stream synchronized, so decoding continues.
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

    // A bad header loses every boundary, so it's terminal.
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
        decoder.feed(format!("Content-Length: {}\r\n\r\n", crate::framing::MAX_CONTENT_LENGTH + 1).as_bytes());
        let error = decoder.take_message().unwrap().unwrap_err();
        assert!(error.contains("exceeds"), "{error}");
        assert!(decoder.is_failed());
    }

    // An astral character is one `char` to bish and two UTF-16 units to a
    // server.
    #[test]
    fn columns_convert_between_bish_chars_and_each_server_encoding() {
        let line: Vec<char> = "a🌍b".chars().collect();
        assert_eq!(line.len(), 3);
        for (encoding, expected) in [(PositionEncoding::Utf32, 2), (PositionEncoding::Utf16, 3), (PositionEncoding::Utf8, 5)] {
            // The column after the emoji: char 2 for us, further on in
            // smaller units.
            assert_eq!(to_server_column(&line, 2, encoding), expected, "{encoding:?}");
            assert_eq!(from_server_column(&line, expected, encoding), 2, "{encoding:?}");
        }
        // `Utf32` is bish's counting: the identity.
        for col in 0..=line.len() {
            assert_eq!(to_server_column(&line, col, PositionEncoding::Utf32), col);
            assert_eq!(from_server_column(&line, col, PositionEncoding::Utf32), col);
        }
    }

    #[test]
    fn a_column_past_the_end_or_inside_a_character_still_lands_somewhere_real() {
        let line: Vec<char> = "a🌍".chars().collect();
        // Past the end clamps.
        assert_eq!(to_server_column(&line, 99, PositionEncoding::Utf32), 2);
        assert_eq!(from_server_column(&line, 99, PositionEncoding::Utf16), 2);
        // Mid-character resolves to its start.
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

        // A `LocationLink` has two ranges; the selection range, the
        // identifier, is where a jump lands.
        let link = json::parse(
            r#"[{"targetUri":"file:///c.rs",
                "targetRange":{"start":{"line":10,"character":0},"end":{"line":20,"character":1}},
                "targetSelectionRange":{"start":{"line":10,"character":7},"end":{"line":10,"character":11}}}]"#,
        )
        .unwrap();
        assert_eq!(locations(&link), vec![Location { uri: "file:///c.rs".to_string(), start: (10, 7), end: (10, 11) }]);

        // Falling back to the full range alone.
        let link_only =
            json::parse(r#"[{"targetUri":"file:///c.rs","targetRange":{"start":{"line":10,"character":0},"end":{"line":20,"character":1}}}]"#)
                .unwrap();
        assert_eq!(locations(&link_only)[0].start, (10, 0));
    }

    #[test]
    fn a_server_that_does_not_know_where_something_is_says_so_with_null() {
        assert!(locations(&Value::Null).is_empty());
        assert!(locations(&json::parse("[]").unwrap()).is_empty());
        // One malformed entry doesn't cost the rest.
        let mixed =
            json::parse(r#"[{"nonsense":1},{"uri":"file:///a.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}]"#)
                .unwrap();
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
        // No insert text: the label.
        assert_eq!(found[0].insert, "push");
        // No range: the caller replaces the word being typed.
        assert_eq!(found[0].edit, None);
        // No sort text: the label.
        assert_eq!(found[0].sort, "push");

        // The list form, `isIncomplete` ignored.
        let list = json::parse(r#"{"isIncomplete":true,"items":[{"label":"a"},{"label":"b"}]}"#).unwrap();
        assert_eq!(completions(&list).len(), 2);
        assert!(completions(&Value::Null).is_empty());
    }

    // A `textEdit` gives the exact range, how dotted paths and imports
    // complete.
    #[test]
    fn a_text_edit_carries_both_the_text_and_the_range_to_replace() {
        let with_edit = json::parse(
            r#"[{"label":"std::fmt","textEdit":{"range":{"start":{"line":3,"character":4},"end":{"line":3,"character":9}},"newText":"std::fmt"}}]"#,
        )
        .unwrap();
        let found = completions(&with_edit);
        assert_eq!(found[0].edit, Some(((3, 4), (3, 9))));
        assert_eq!(found[0].insert, "std::fmt");

        // `InsertReplaceEdit`: `insert`, which doesn't eat what follows.
        let insert_replace = json::parse(
            r#"[{"label":"x","textEdit":{"insert":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}},
                "replace":{"start":{"line":1,"character":0},"end":{"line":1,"character":9}},"newText":"xy"}}]"#,
        )
        .unwrap();
        assert_eq!(completions(&insert_replace)[0].edit, Some(((1, 0), (1, 2))));
    }

    // A snippet is kept in its notation and flagged, for the editor to
    // insert live (`bishedit::snippet`).
    #[test]
    fn a_snippet_completion_keeps_its_tabstops_and_says_so() {
        let snippet = json::parse(r#"[{"label":"foo","insertText":"foo(${1:a}, ${2:b})$0","insertTextFormat":2}]"#).unwrap();
        let found = completions(&snippet);
        assert!(found[0].snippet);
        assert_eq!(found[0].insert, "foo(${1:a}, ${2:b})$0");
        // Without the flag it's literal text, and unflagged.
        let literal = json::parse(r#"[{"label":"foo","insertText":"foo(${1:a})"}]"#).unwrap();
        assert!(!completions(&literal)[0].snippet);
        assert_eq!(completions(&literal)[0].insert, "foo(${1:a})");
    }

    // Text for the buffer keeps its lines, unlike a message.
    #[test]
    fn insert_text_keeps_its_newlines_and_indentation() {
        let multi = json::parse(r#"[{"label":"fn","insertText":"fn $1() {\n\t$0\n}","insertTextFormat":2}]"#).unwrap();
        assert_eq!(completions(&multi)[0].insert, "fn $1() {\n\t$0\n}");
        // Undisplayable characters still go.
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

    fn legend() -> SemanticLegend {
        SemanticLegend {
            types: vec!["keyword".to_string(), "function".to_string(), "parameter".to_string()],
            modifiers: vec!["declaration".to_string(), "readonly".to_string()],
        }
    }

    // Each tuple is relative to the previous, and `deltaStart`'s meaning
    // depends on whether the line moved.
    #[test]
    fn semantic_tokens_are_decoded_from_their_deltas() {
        let data = json::parse(
            r#"{"data":[
                 0,0,3,0,0,
                 0,4,4,1,1,
                 2,2,5,2,0]}"#,
        )
        .unwrap();
        let found = semantic_tokens(&data, &legend());
        assert_eq!(found.len(), 3);
        // Line 0, column 0.
        assert_eq!((found[0].line, found[0].start, found[0].length), (0, 0, 3));
        assert_eq!(found[0].kind, "keyword");
        assert!(found[0].modifiers.is_empty());
        // Same line: start relative to the first.
        assert_eq!((found[1].line, found[1].start, found[1].length), (0, 4, 4));
        assert_eq!(found[1].kind, "function");
        assert_eq!(found[1].modifiers, vec!["declaration".to_string()], "bit 0 of the modifier mask");
        // Two lines down: start absolute again.
        assert_eq!((found[2].line, found[2].start, found[2].length), (2, 2, 5));
        assert_eq!(found[2].kind, "parameter");
    }

    #[test]
    fn a_token_bish_cannot_name_is_dropped_rather_than_shifting_the_rest() {
        // Type 9 is outside the legend and a zero-length token paints
        // nothing, but both advance the position.
        let data = json::parse(r#"{"data":[0,0,3,9,0, 0,4,0,0,0, 0,2,2,0,0]}"#).unwrap();
        let found = semantic_tokens(&data, &legend());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].start, found[0].length), (0, 6, 2));
    }

    #[test]
    fn a_truncated_or_malformed_stream_keeps_what_it_can() {
        // A trailing partial tuple: everything before it stands.
        let data = json::parse(r#"{"data":[0,0,3,0,0, 0,4,4]}"#).unwrap();
        assert_eq!(semantic_tokens(&data, &legend()).len(), 1);
        // Nothing, and the wrong shape.
        assert!(semantic_tokens(&json::parse(r#"{"data":[]}"#).unwrap(), &legend()).is_empty());
        assert!(semantic_tokens(&json::parse(r#"{"resultId":"1"}"#).unwrap(), &legend()).is_empty());
        // An empty legend names nothing.
        assert!(semantic_tokens(&json::parse(r#"{"data":[0,0,3,0,0]}"#).unwrap(), &SemanticLegend::default()).is_empty());
    }

    // The legend comes from the handshake, once.
    #[test]
    fn the_legend_comes_out_of_the_servers_own_capabilities() {
        let capabilities = json::parse(
            r#"{"semanticTokensProvider":{"legend":{
                 "tokenTypes":["namespace","type"],"tokenModifiers":["static"]},"full":true}}"#,
        )
        .unwrap();
        let legend = SemanticLegend::parse(&capabilities);
        assert_eq!(legend.types, vec!["namespace".to_string(), "type".to_string()]);
        assert_eq!(legend.modifiers, vec!["static".to_string()]);
        assert!(!legend.is_empty());
        // A server without semantic tokens has no legend.
        assert!(SemanticLegend::parse(&json::parse(r#"{"hoverProvider":true}"#).unwrap()).is_empty());
    }

    #[test]
    fn both_shapes_of_document_symbol_are_read_and_flattened() {
        // Hierarchical: no uri, children nested.
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
        // The name, not the declaration: where an outline jumps.
        assert_eq!(found[0].start, (0, 7));
        assert_eq!((found[1].name.as_str(), found[1].kind.as_str(), found[1].depth), ("inner", "method", 1));
        assert_eq!(found[1].uri, "file:///a.rs", "the hierarchical form names no uri of its own");

        // The flat form has a full location.
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
        // Entries with no name or range are dropped, not their neighbours.
        let mixed = json::parse(r#"[{"kind":12},{"name":"ok","kind":12,"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}}}]"#)
            .unwrap();
        assert_eq!(symbols(&mixed, "file:///a").len(), 1);
        // An unknown kind is a server extension, not a reason to drop it.
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
        // A formatter's tabs and newlines survive: the one unsanitized case.
        assert_eq!(found[0].text, "\tif x {\n");
        assert_eq!((found[0].start, found[0].end), ((0, 0), (0, 4)));
        // An empty replacement of an empty range is a legal no-op.
        assert_eq!(found[1].text, "");

        assert!(text_edits(&Value::Null).is_empty(), "a server with nothing to change");
        // One malformed entry doesn't cost the rest.
        let mixed =
            json::parse(r#"[{"newText":"no range"},{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}},"newText":"x"}]"#)
                .unwrap();
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

        // `documentChanges` wins when both are present.
        let both = json::parse(&format!(
            r#"{{"changes":{{"file:///ignored.rs":[{one_edit}]}},
                 "documentChanges":[{{"textDocument":{{"uri":"file:///c.rs","version":3}},"edits":[{one_edit}]}}]}}"#
        ))
        .unwrap();
        let edit = workspace_edit(&both);
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes[0].0, "file:///c.rs");
    }

    // rust-analyzer renames a module's file then edits it under the new
    // name, so operations keep their place among the edits. An unreadable
    // operation is reported: doing the rest without it would break the
    // project.
    #[test]
    fn file_operations_are_read_in_their_place_among_the_text_edits() {
        let edit = workspace_edit(
            &json::parse(
                r#"{"documentChanges":[
                {"kind":"create","uri":"file:///new.rs","options":{"ignoreIfExists":true}},
                {"textDocument":{"uri":"file:///a.rs","version":1},
                 "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"x"}]},
                {"kind":"rename","oldUri":"file:///old.rs","newUri":"file:///moved.rs","options":{"overwrite":true}},
                {"kind":"delete","uri":"file:///gone","options":{"recursive":true}},
                {"kind":"chmod","uri":"file:///x"},
                {"kind":"rename","oldUri":"file:///only-one-side.rs"}]}"#,
            )
            .unwrap(),
        );
        assert_eq!(edit.changes.len(), 1, "the text half is read as before");
        assert_eq!(
            edit.operations,
            vec![
                (0, FileOperation::Create { uri: "file:///new.rs".into(), overwrite: false, ignore_if_exists: true }),
                (
                    1,
                    FileOperation::Rename {
                        old_uri: "file:///old.rs".into(),
                        new_uri: "file:///moved.rs".into(),
                        overwrite: true,
                        ignore_if_exists: false
                    }
                ),
                (1, FileOperation::Delete { uri: "file:///gone".into(), recursive: true, ignore_if_not_exists: false }),
            ]
        );
        assert_eq!(edit.unsupported, vec!["chmod".to_string(), "rename".to_string()], "a kind never heard of, and a rename missing its target");
    }

    #[test]
    fn an_empty_or_absent_workspace_edit_is_empty_not_an_error() {
        assert_eq!(workspace_edit(&Value::Null), WorkspaceEdit::default());
        assert_eq!(workspace_edit(&json::parse(r#"{"changes":{}}"#).unwrap()), WorkspaceEdit::default());
        // A file with no edits contributes nothing.
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

        // Its edit up front.
        assert_eq!(found[0].kind, "quickfix");
        assert_eq!(found[0].edit.as_ref().unwrap().changes.len(), 1);

        // A command bish can't run, kept so the refusal can name it.
        assert_eq!(found[1].command.as_deref(), Some("rust-analyzer.runSingle"));
        assert!(found[1].edit.is_none());

        // No edit or command: computed only if chosen, hence `unresolved`.
        assert!(found[2].edit.is_none() && found[2].command.is_none());
        assert_eq!(json::query(&found[2].unresolved, ".data.id"), Ok(&Value::Number(7.0)));

        // Not applicable, with the server's reason.
        assert_eq!(found[3].disabled.as_deref(), Some("no trait in scope"));
    }

    #[test]
    fn the_older_bare_command_form_is_read_too() {
        // Pre-3.8 servers answer with `Command`s, a string at the top level.
        let bare = json::parse(r#"[{"title":"Do it","command":"server.doIt"}]"#).unwrap();
        let found = code_actions(&bare);
        assert_eq!(found[0].command.as_deref(), Some("server.doIt"));
        assert!(code_actions(&Value::Null).is_empty());
        // A titleless entry isn't pickable.
        assert!(code_actions(&json::parse(r#"[{"kind":"quickfix"}]"#).unwrap()).is_empty());
    }

    fn hover(json_text: &str) -> Option<Vec<String>> {
        hover_lines(&json::parse(json_text).unwrap())
    }

    // All three shapes are in the wild.
    #[test]
    fn every_shape_a_server_may_return_hover_contents_in_is_read() {
        // MarkupContent.
        assert_eq!(hover(r#"{"contents":{"kind":"markdown","value":"a line"}}"#), Some(vec!["a line".to_string()]));
        // A bare MarkedString.
        assert_eq!(hover(r#"{"contents":"a line"}"#), Some(vec!["a line".to_string()]));
        // `{language, value}`, which also carries `value`.
        assert_eq!(hover(r#"{"contents":{"language":"rust","value":"fn f()"}}"#), Some(vec!["fn f()".to_string()]));
        // An array, joined.
        assert_eq!(hover(r#"{"contents":["one",{"language":"rust","value":"two"}]}"#), Some(vec!["one".to_string(), "two".to_string()]));
    }

    #[test]
    fn a_server_with_nothing_to_say_says_so_rather_than_showing_an_empty_popup() {
        assert_eq!(hover(r#"{"contents":null}"#), None);
        assert_eq!(hover("null"), None);
        assert_eq!(hover(r#"{"contents":{"kind":"markdown","value":""}}"#), None);
        // Whitespace and fences only: nothing.
        assert_eq!(hover("{\"contents\":\"```rust\\n```\\n\\n\"}"), None);
    }

    #[test]
    fn markdown_is_flattened_to_lines_without_becoming_a_renderer() {
        let lines =
            hover("{\"contents\":{\"kind\":\"markdown\",\"value\":\"```rust\\nfn f() -> i32\\n```\\n\\n---\\n\\nReturns **a number**.\\n\\n\\n\"}}")
                .unwrap();
        assert_eq!(lines, vec!["fn f() -> i32".to_string(), String::new(), "---".to_string(), String::new(), "Returns **a number**.".to_string()]);
        // Blank-line runs collapse and trailing ones go; one separator stays.
        // `**bold**` is left as written.
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

    // Server text on its way to a terminal: control characters are what
    // `url::is_safe` guards against, and newlines would break one finding
    // per line.
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
        // A numeric code (tsserver, compilers) is as common as a string.
        let p = published(
            r#"{"uri":"file:///x","diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"code":2304,"message":"m"}]}"#,
        );
        assert_eq!(p.findings[0].code, "2304");
        // No severity: an error, the loudest reading.
        assert_eq!(p.findings[0].severity, 1);
        // No version: nothing can be gated on it.
        assert_eq!(p.version, None);

        // An end before its start: an empty range at the start.
        let backwards = published(
            r#"{"uri":"file:///x","diagnostics":[{"range":{"start":{"line":3,"character":8},"end":{"line":1,"character":0}},"message":"m"}]}"#,
        );
        assert_eq!(backwards.findings[0].start, (3, 8));
        assert_eq!(backwards.findings[0].end, (3, 8));

        // A finding with no range is dropped, not the rest.
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
        // An empty list withdraws everything previously reported.
        assert_eq!(published(r#"{"uri":"file:///x","diagnostics":[]}"#).findings, vec![]);
    }

    // A shell server matches `shellscript`, so `bash` would be ignored.
    #[test]
    fn a_language_name_is_translated_only_where_lsp_uses_a_different_one() {
        assert_eq!(language_id("bash"), "shellscript");
        assert_eq!(language_id("text"), "plaintext");
        // Most names already agree.
        for same in ["rust", "python", "typescript", "json", "yaml", "markdown", "ruby", "elixir"] {
            assert_eq!(language_id(same), same);
        }
        // A name with no LSP equivalent passes through.
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
