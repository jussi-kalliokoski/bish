// `bish tool lsp-server` -- the other side of lspclient.rs. A Language
// Server Protocol server for bash, spoken over stdio, so that any
// editor that talks LSP gets bish's own understanding of a shell
// script: its diagnostics, its formatter, its documentation hovers, and
// the fixes it knows how to make.
//
// Everything it serves already existed. `bishedit::lint::BashLinter`
// and `bishedit::format::BashFormatter` are the rule engines, and
// `bish tool check` and `bish tool format` are their command-line
// front ends; `docs::hover_lines_at` is what `K` answers with in the
// editor. This module is a third front end onto the same answers, and
// deliberately contains no rules of its own -- a finding that differs
// between `bish tool check` and this would be a bug in one of them, and
// there is nowhere here for such a difference to come from.
//
// The transport is `lsp.rs`, unchanged: it was written as a wire format
// with no notion of which end it is, which is why it needed nothing
// added for this. What is new is the direction. Every helper there
// takes a server's reply apart (`hover_lines(result)`,
// `completions(result)`); a server has to put those payloads together
// instead, which is what this module does.
//
// **Blocking reads, and no threads at all.** The client is the hard
// case -- it drives a subprocess while an editor is being typed into,
// so it needs non-blocking pipes, a bounded outgoing queue, and polling
// from an event loop that must never stall. A server is the easy one:
// it is a dedicated process whose only job is to answer, so it reads
// until a message arrives and answers it. Reaching for the client's
// machinery here would be copying a solution to a problem this does not
// have.
//
// Sync is full-document (`textDocumentSync: 1`). Incremental sync is
// the client's own optimisation for not re-sending a large file on
// every keystroke, and it is the *client* that has to implement it; a
// server that asks for it is asking for range arithmetic it can decline
// to need. Everything here re-lints a whole document anyway, since the
// linter is a whole-document pass.
#![allow(dead_code)]

use crate::bishedit::format::BashFormatter;
use crate::bishedit::lint::{BashLinter, Diagnostic, Fix, Linter, Severity};
use crate::docs::DocIndex;
use crate::json::{self, Value};
use crate::lsp::{self, Message, PositionEncoding, ResponseError};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;

/// One open document, as the editor last told us it was.
struct Document {
    text: String,
    /// Kept as chars once rather than re-split per request: every
    /// position that arrives and every span that leaves is measured in
    /// them.
    chars: Vec<char>,
}

impl Document {
    fn new(text: String) -> Document {
        let chars = text.chars().collect();
        Document { text, chars }
    }
}

struct Server {
    documents: HashMap<String, Document>,
    /// What the client and this agreed to count columns in. bish's own
    /// unit is the `char`, which is utf-32 -- so that is what it asks
    /// for, and anything else costs a conversion at the boundary.
    encoding: PositionEncoding,
    /// Set by `shutdown`, checked by `exit`: a client that exits
    /// without shutting down first is a client that crashed, and the
    /// spec asks a server to say so through its exit code rather than
    /// pretend it was orderly.
    shutdown_requested: bool,
}

/// `bish tool lsp-server`: read messages until stdin ends or `exit`
/// arrives, answering each. The return value is this process's exit
/// code.
pub fn run(args: &[String]) -> i32 {
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: bish tool lsp-server");
                println!();
                println!("Serves the Language Server Protocol for bash over stdin/stdout, so any");
                println!("editor that speaks LSP gets bish's diagnostics, formatting, hovers and");
                println!("quick fixes. Not meant to be run by hand -- an editor starts it.");
                return 0;
            }
            other => {
                eprintln!("bish tool lsp-server: unrecognized option '{other}'");
                return 2;
            }
        }
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    Server::new().serve(&mut stdin.lock(), &mut stdout.lock())
}

impl Server {
    fn new() -> Server {
        Server { documents: HashMap::new(), encoding: PositionEncoding::Utf16, shutdown_requested: false }
    }

    /// The loop, over any reader and writer rather than over stdin and
    /// stdout specifically -- which is what lets the tests drive a whole
    /// session by handing it bytes, exactly as `lsp.rs`'s own tests do.
    fn serve(&mut self, input: &mut impl Read, output: &mut impl Write) -> i32 {
        let mut decoder = lsp::Decoder::new();
        let mut buf = [0u8; 8192];
        loop {
            // Everything already framed is answered before another read,
            // since one read can carry several messages and an editor
            // that sent `initialize` and `didOpen` together is entitled
            // to both answers.
            while let Some(message) = decoder.take_message() {
                let message = match message {
                    Ok(message) => message,
                    // A malformed frame is not recoverable: the stream's
                    // own position is no longer known, so there is
                    // nothing to resynchronise to.
                    Err(e) => {
                        eprintln!("bish tool lsp-server: {e}");
                        return 1;
                    }
                };
                if let Some(code) = self.handle(message, output) {
                    return code;
                }
            }
            match input.read(&mut buf) {
                // Stdin closed without an `exit`. The editor is gone;
                // there is nobody left to answer.
                Ok(0) => return i32::from(!self.shutdown_requested),
                Ok(n) => decoder.feed(&buf[..n]),
                Err(e) => {
                    eprintln!("bish tool lsp-server: error reading stdin: {e}");
                    return 1;
                }
            }
        }
    }

    /// Answers one message. `Some(code)` means stop and exit with it.
    fn handle(&mut self, message: Message, output: &mut impl Write) -> Option<i32> {
        match message {
            Message::Request { id, method, params } => {
                let result = self.request(&method, &params);
                self.send(output, Message::Response { id, result });
                None
            }
            Message::Notification { method, params } => self.notification(&method, &params, output),
            // A server sends requests, so it can receive responses --
            // but this one asks the client for nothing, so anything
            // arriving as a response is answering a question that was
            // never put.
            Message::Response { .. } => None,
        }
    }

    fn send(&self, output: &mut impl Write, message: Message) {
        let bytes = lsp::encode(&message);
        // A write that fails means the editor is gone, which the next
        // read will discover; there is no useful recovery here and
        // nowhere to report it that the editor would see.
        let _ = output.write_all(&bytes);
        let _ = output.flush();
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, ResponseError> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "shutdown" => {
                self.shutdown_requested = true;
                Ok(Value::Null)
            }
            "textDocument/formatting" => Ok(self.formatting(params)),
            "textDocument/codeAction" => Ok(self.code_actions(params)),
            "textDocument/hover" => Ok(self.hover(params)),
            "textDocument/documentSymbol" => Ok(self.document_symbols(params)),
            // -32601, MethodNotFound. A client is entitled to ask for
            // anything; saying so plainly is how it learns not to.
            _ => Err(ResponseError { code: -32601, message: format!("bish serves no {method}") }),
        }
    }

    fn notification(&mut self, method: &str, params: &Value, output: &mut impl Write) -> Option<i32> {
        match method {
            "initialized" => None,
            "exit" => Some(i32::from(!self.shutdown_requested)),
            "textDocument/didOpen" => {
                let uri = uri_of(params)?;
                let text = string_at(params, ".textDocument.text").unwrap_or_default();
                self.documents.insert(uri.clone(), Document::new(text));
                self.publish(&uri, output);
                None
            }
            "textDocument/didChange" => {
                let uri = uri_of(params)?;
                // Full sync, so the last change in the list is the whole
                // document. A client that sent ranges anyway would be
                // ignoring what `initialize` said it could send.
                let text = match json::query(params, ".contentChanges") {
                    Ok(Value::Array(changes)) => changes.last().and_then(|c| string_at(c, ".text")),
                    _ => None,
                };
                if let Some(text) = text {
                    self.documents.insert(uri.clone(), Document::new(text));
                    self.publish(&uri, output);
                }
                None
            }
            "textDocument/didSave" => {
                let uri = uri_of(params)?;
                self.publish(&uri, output);
                None
            }
            "textDocument/didClose" => {
                let uri = uri_of(params)?;
                self.documents.remove(&uri);
                // An empty list, not silence: diagnostics belong to the
                // server until it says otherwise, so a closed document
                // whose findings were never withdrawn leaves them on
                // screen for ever.
                self.send_diagnostics(&uri, Vec::new(), output);
                None
            }
            _ => None,
        }
    }

    // -----------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------

    fn initialize(&mut self, params: &Value) -> Value {
        // The client lists what it can count in; the server picks. bish
        // measures in `char`s, which is utf-32 exactly, so that is the
        // first choice and the others are a conversion at the boundary.
        // Silence means utf-16, which is the protocol's own original
        // and only unit.
        let offered: Vec<String> = match json::query(params, ".capabilities.general.positionEncodings") {
            Ok(Value::Array(names)) => names.iter().filter_map(|n| if let Value::Str(s) = n { Some(s.clone()) } else { None }).collect(),
            _ => Vec::new(),
        };
        self.encoding = [PositionEncoding::Utf32, PositionEncoding::Utf8, PositionEncoding::Utf16]
            .into_iter()
            .find(|e| offered.iter().any(|name| name == e.wire_name()))
            .unwrap_or(PositionEncoding::Utf16);

        Value::Object(vec![
            (
                "capabilities".to_string(),
                Value::Object(vec![
                    ("positionEncoding".to_string(), Value::Str(self.encoding.wire_name().to_string())),
                    // 1 is full sync; see this module's own doc comment
                    // for why that is a decision and not a shortcut.
                    (
                        "textDocumentSync".to_string(),
                        Value::Object(vec![
                            ("openClose".to_string(), Value::Bool(true)),
                            ("change".to_string(), Value::Number(1.0)),
                            ("save".to_string(), Value::Bool(true)),
                        ]),
                    ),
                    ("documentFormattingProvider".to_string(), Value::Bool(true)),
                    ("codeActionProvider".to_string(), Value::Bool(true)),
                    ("hoverProvider".to_string(), Value::Bool(true)),
                    ("documentSymbolProvider".to_string(), Value::Bool(true)),
                ]),
            ),
            (
                "serverInfo".to_string(),
                Value::Object(vec![
                    ("name".to_string(), Value::Str("bish".to_string())),
                    ("version".to_string(), Value::Str(env!("CARGO_PKG_VERSION").to_string())),
                ]),
            ),
        ])
    }

    /// The whole document, reformatted -- one edit spanning everything.
    ///
    /// A formatter that returned a minimal edit script would let an
    /// editor preserve its own marks and folds better, and this does
    /// not: `BashFormatter` reports its findings as fixes over the
    /// original text, and `apply_fixes` already knows how to splice
    /// them. Handing back the result as one replacement is the same
    /// answer with less arithmetic between here and it.
    fn formatting(&self, params: &Value) -> Value {
        let Some(doc) = uri_of(params).and_then(|uri| self.documents.get(&uri)) else {
            return Value::Array(Vec::new());
        };
        // A script that does not parse is not reformatted. Same refusal
        // `bish tool format` makes, and for the same reason: guessing
        // at the shape of something broken is how a formatter eats an
        // afternoon's work.
        let Ok(findings) = BashFormatter.check(&doc.text) else {
            return Value::Array(Vec::new());
        };
        let (formatted, _, _) = apply_fixes(&doc.text, &findings);
        if formatted == doc.text {
            return Value::Array(Vec::new());
        }
        let end = self.position(&doc.chars, doc.chars.len());
        Value::Array(vec![Value::Object(vec![
            ("range".to_string(), range_value(self.position(&doc.chars, 0), end)),
            ("newText".to_string(), Value::Str(formatted)),
        ])])
    }

    /// Every fix bish knows how to make, for the findings inside the
    /// range the editor asked about.
    ///
    /// One `Fix` is one replacement of one span, which is exactly a
    /// `WorkspaceEdit` with a single `TextEdit` in it -- so a rule that
    /// carries a fix becomes a quick action with no translation beyond
    /// the coordinates. A rule that does not carry one produces no
    /// action at all rather than an action that does nothing.
    fn code_actions(&self, params: &Value) -> Value {
        let Some(uri) = uri_of(params) else { return Value::Array(Vec::new()) };
        let Some(doc) = self.documents.get(&uri) else { return Value::Array(Vec::new()) };
        let (from, to) = match self.range_of(&doc.chars, params, ".range") {
            Some(range) => range,
            None => (0, doc.chars.len()),
        };
        let actions: Vec<Value> = self
            .findings(doc)
            .into_iter()
            .filter(|d| d.start < to && d.end > from || (d.start == d.end && d.start >= from && d.start <= to))
            .filter_map(|d| {
                let fix = d.fix.as_ref()?;
                Some(Value::Object(vec![
                    ("title".to_string(), Value::Str(format!("{}: {}", d.label(), fix_title(fix)))),
                    ("kind".to_string(), Value::Str("quickfix".to_string())),
                    ("diagnostics".to_string(), Value::Array(vec![self.diagnostic_value(&doc.chars, &d)])),
                    (
                        "edit".to_string(),
                        Value::Object(vec![(
                            "changes".to_string(),
                            Value::Object(vec![(
                                uri.clone(),
                                Value::Array(vec![Value::Object(vec![
                                    ("range".to_string(), range_value(self.position(&doc.chars, fix.start), self.position(&doc.chars, fix.end))),
                                    ("newText".to_string(), Value::Str(fix.replacement.clone())),
                                ])]),
                            )]),
                        )]),
                    ),
                ]))
            })
            .collect();
        Value::Array(actions)
    }

    /// What `K` says in bish's own editor, in a hover popup instead.
    ///
    /// The index is built from the document's own text every time
    /// rather than cached, which is what makes a function's doc comment
    /// appear as it is typed. It is also what makes this correct for a
    /// script that `source`s another: `DocIndex` follows those on its
    /// own, off disk, exactly as the editor's own hover does.
    fn hover(&self, params: &Value) -> Value {
        let Some(uri) = uri_of(params) else { return Value::Null };
        let Some(doc) = self.documents.get(&uri) else { return Value::Null };
        let Some(offset) = self.offset_of(&doc.chars, params, ".position") else { return Value::Null };
        let path = crate::url::to_file_path(&uri).unwrap_or_else(|| PathBuf::from("."));
        let index = DocIndex::build_from_source(&doc.text, &path);
        let (line_start, line_end) = line_bounds(&doc.chars, offset);
        let line_text: String = doc.chars[line_start..line_end].iter().collect();
        // The man page this hover is about is fetched *before* asking
        // for the hover, on this thread. In the editor the same lookup
        // happens in the background and the first `K` says "press K
        // again in a moment", because a redraw must not stop to read a
        // file. Here there is no second `K` and nothing else to do:
        // warming the cache first means the shared code below finds the
        // page ready and says what it actually knows.
        warm_man_page(&line_text, offset - line_start);
        // No live shell to ask what a variable currently holds -- that
        // is the editor's own hover talking to its own session, and
        // there is no session here. Everything else a hover says is a
        // fact about the text.
        let lines = crate::docs::hover_lines_at(&doc.chars[line_start..line_end], offset - line_start, &line_text, &index, |_| None);
        if lines.is_empty() {
            return Value::Null;
        }
        Value::Object(vec![(
            "contents".to_string(),
            Value::Object(vec![("kind".to_string(), Value::Str("markdown".to_string())), ("value".to_string(), Value::Str(lines.join("\n")))]),
        )])
    }

    /// The functions this script defines, for an outline.
    ///
    /// `docs::functions_in` rather than `DocIndex`: the index records
    /// only documented symbols, which is right for a hover and wrong
    /// here -- an outline wants every function, and an undocumented one
    /// is exactly the one somebody is looking for.
    fn document_symbols(&self, params: &Value) -> Value {
        let Some(uri) = uri_of(params) else { return Value::Array(Vec::new()) };
        let Some(doc) = self.documents.get(&uri) else { return Value::Array(Vec::new()) };
        let values = crate::docs::functions_in(&doc.text)
            .into_iter()
            .map(|(name, line, doc)| {
                // `line` is 1-based and LSP counts from 0. The range
                // is the definition's own line and no more: the scan
                // records where a function starts and not how far it
                // runs, and claiming a range past that would be
                // inventing it.
                let at = line.saturating_sub(1) as f64;
                let range = Value::Object(vec![
                    (
                        "start".to_string(),
                        Value::Object(vec![("line".to_string(), Value::Number(at)), ("character".to_string(), Value::Number(0.0))]),
                    ),
                    ("end".to_string(), Value::Object(vec![("line".to_string(), Value::Number(at)), ("character".to_string(), Value::Number(0.0))])),
                ]);
                let mut fields = vec![
                    ("name".to_string(), Value::Str(name)),
                    // 12 is SymbolKind.Function.
                    ("kind".to_string(), Value::Number(12.0)),
                    ("range".to_string(), range.clone()),
                    ("selectionRange".to_string(), range),
                ];
                if !doc.is_empty() {
                    fields.push(("detail".to_string(), Value::Str(doc.join(" "))));
                }
                Value::Object(fields)
            })
            .collect();
        Value::Array(values)
    }

    // -----------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------

    /// Everything bish has to say about a document: the linter's
    /// findings and the formatter's, in that order.
    ///
    /// A script that does not parse gets the linter's findings and a
    /// single finding for the parse error itself, rather than nothing:
    /// "this does not parse" is the most useful thing anyone can be
    /// told about a file that does not parse, and it is exactly what
    /// the formatter's `Err` says.
    fn findings(&self, doc: &Document) -> Vec<Diagnostic> {
        let mut findings = BashLinter.check(&doc.text);
        match BashFormatter.check(&doc.text) {
            Ok(formatting) => findings.extend(formatting),
            Err(message) => findings.push(Diagnostic {
                start: 0,
                end: doc.chars.len().min(1),
                severity: Severity::Error,
                code: std::borrow::Cow::Borrowed("parse-error"),
                source: None,
                message,
                fix: None,
            }),
        }
        findings
    }

    fn publish(&self, uri: &str, output: &mut impl Write) {
        let Some(doc) = self.documents.get(uri) else { return };
        let findings = self.findings(doc);
        let values = findings.iter().map(|d| self.diagnostic_value(&doc.chars, d)).collect();
        self.send_diagnostics(uri, values, output);
    }

    fn send_diagnostics(&self, uri: &str, diagnostics: Vec<Value>, output: &mut impl Write) {
        self.send(
            output,
            Message::Notification {
                method: "textDocument/publishDiagnostics".to_string(),
                params: Value::Object(vec![("uri".to_string(), Value::Str(uri.to_string())), ("diagnostics".to_string(), Value::Array(diagnostics))]),
            },
        );
    }

    fn diagnostic_value(&self, chars: &[char], d: &Diagnostic) -> Value {
        let mut fields = vec![
            ("range".to_string(), range_value(self.position(chars, d.start), self.position(chars, d.end))),
            ("severity".to_string(), Value::Number(severity_number(d.severity))),
            ("code".to_string(), Value::Str(d.code.to_string())),
            ("message".to_string(), Value::Str(d.message.clone())),
        ];
        // `source` names who found it, and everything here was found by
        // bish -- including the findings relayed from somewhere else,
        // which carry their own origin in `Diagnostic::source` and keep
        // it.
        fields.push(("source".to_string(), Value::Str(d.source.clone().unwrap_or_else(|| "bish".to_string()))));
        Value::Object(fields)
    }

    // -----------------------------------------------------------------
    // Positions
    // -----------------------------------------------------------------

    /// A char offset as an LSP position, in whatever unit was
    /// negotiated. `lsp::to_server_column` does the column half, which
    /// is the half that differs between the two ends.
    fn position(&self, chars: &[char], offset: usize) -> (usize, usize) {
        let offset = offset.min(chars.len());
        let line = chars[..offset].iter().filter(|&&c| c == '\n').count();
        let line_start = chars[..offset].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let (_, line_end) = line_bounds(chars, line_start);
        (line, lsp::to_server_column(&chars[line_start..line_end], offset - line_start, self.encoding))
    }

    /// The inverse, for a position that arrived from the editor.
    fn offset(&self, chars: &[char], line: usize, column: usize) -> usize {
        let mut start = 0;
        for _ in 0..line {
            match chars[start..].iter().position(|&c| c == '\n') {
                Some(at) => start += at + 1,
                // Past the end of the document: clamped rather than
                // refused, since an editor and a server can disagree by
                // one keystroke about how long a file is.
                None => return chars.len(),
            }
        }
        let (_, end) = line_bounds(chars, start);
        start + lsp::from_server_column(&chars[start..end], column, self.encoding)
    }

    fn offset_of(&self, chars: &[char], params: &Value, path: &str) -> Option<usize> {
        let line = number_at(params, &format!("{path}.line"))?;
        let character = number_at(params, &format!("{path}.character"))?;
        Some(self.offset(chars, line, character))
    }

    fn range_of(&self, chars: &[char], params: &Value, path: &str) -> Option<(usize, usize)> {
        let start = self.offset_of(chars, params, &format!("{path}.start"))?;
        let end = self.offset_of(chars, params, &format!("{path}.end"))?;
        Some((start, end))
    }
}

// ---------------------------------------------------------------------
// Small shared pieces
// ---------------------------------------------------------------------

/// The line `offset` sits on, as `[start, end)` -- the newline itself
/// excluded, since a column is a position within a line's text.
fn line_bounds(chars: &[char], offset: usize) -> (usize, usize) {
    let offset = offset.min(chars.len());
    let start = chars[..offset].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
    let end = chars[start..].iter().position(|&c| c == '\n').map_or(chars.len(), |i| start + i);
    (start, end)
}

fn range_value(start: (usize, usize), end: (usize, usize)) -> Value {
    let position = |(line, character): (usize, usize)| {
        Value::Object(vec![("line".to_string(), Value::Number(line as f64)), ("character".to_string(), Value::Number(character as f64))])
    };
    Value::Object(vec![("start".to_string(), position(start)), ("end".to_string(), position(end))])
}

/// LSP's own numbering, which runs the other way from `Severity`'s.
fn severity_number(severity: Severity) -> f64 {
    match severity {
        Severity::Error => 1.0,
        Severity::Warning => 2.0,
        Severity::Info => 3.0,
        Severity::Hint => 4.0,
    }
}

/// What a quick fix offers to do, in the imperative an action title
/// wants. Deliberately describes the edit rather than repeating the
/// finding's own message, which the editor is already showing beside
/// it.
fn fix_title(fix: &Fix) -> String {
    match (fix.start == fix.end, fix.replacement.is_empty()) {
        (_, true) => "remove".to_string(),
        (true, false) => format!("insert `{}`", one_line(&fix.replacement)),
        (false, false) => format!("replace with `{}`", one_line(&fix.replacement)),
    }
}

/// A replacement spanning lines is not a title. The first line stands
/// for it, with an ellipsis so nobody reads it as the whole edit.
fn one_line(text: &str) -> String {
    match text.split_once('\n') {
        Some((first, _)) => format!("{first} ..."),
        None => text.to_string(),
    }
}

/// Fetches, synchronously, whichever man page a hover at this position
/// is going to want -- and nothing if it wants none. `classify_word` is
/// the same decision `hover_lines_at` makes a moment later, so the two
/// cannot disagree about which page that is.
fn warm_man_page(line_text: &str, col: usize) {
    use crate::docs::WordRole;
    let name = match crate::docs::classify_word(line_text, col) {
        WordRole::Command(name) => name,
        WordRole::Flag { command, .. } => command,
        WordRole::Subcommand { command, subcommand } => {
            // The subcommand hover tries `command-subcommand` first and
            // falls back to the command's own page, so both are wanted.
            crate::bishedit::manpages::query_blocking(&format!("{command}-{subcommand}"));
            command
        }
        _ => return,
    };
    crate::bishedit::manpages::query_blocking(&name);
}

fn uri_of(params: &Value) -> Option<String> {
    string_at(params, ".textDocument.uri")
}

fn string_at(value: &Value, path: &str) -> Option<String> {
    match json::query(value, path) {
        Ok(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn number_at(value: &Value, path: &str) -> Option<usize> {
    match json::query(value, path) {
        Ok(Value::Number(n)) if *n >= 0.0 => Some(*n as usize),
        _ => None,
    }
}

/// `tool.rs`'s own, and the same one: a batch of fixes spliced in
/// descending order of start so no edit moves another's offsets.
fn apply_fixes(text: &str, diagnostics: &[Diagnostic]) -> (String, usize, usize) {
    let mut candidates: Vec<&Fix> = diagnostics.iter().filter_map(|d| d.fix.as_ref()).collect();
    candidates.sort_by_key(|f| std::cmp::Reverse(f.start));
    let mut chars: Vec<char> = text.chars().collect();
    let mut applied = 0;
    let mut last_start = chars.len() + 1;
    for fix in &candidates {
        if fix.end > last_start {
            continue;
        }
        chars.splice(fix.start..fix.end, fix.replacement.chars());
        last_start = fix.start;
        applied += 1;
    }
    (chars.into_iter().collect(), applied, candidates.len() - applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole session, driven the way `lsp.rs`'s own tests drive the
    /// framing: bytes in, bytes out, no process and no pipes. `serve`
    /// takes a reader and a writer rather than stdin and stdout for
    /// exactly this.
    fn session(messages: &[Value]) -> (i32, Vec<Message>) {
        let mut input: Vec<u8> = Vec::new();
        for m in messages {
            let body = json::compact_print(m);
            input.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
            input.extend_from_slice(body.as_bytes());
        }
        let mut output: Vec<u8> = Vec::new();
        let code = Server::new().serve(&mut input.as_slice(), &mut output);
        let mut decoder = lsp::Decoder::new();
        decoder.feed(&output);
        let mut out = Vec::new();
        while let Some(message) = decoder.take_message() {
            out.push(message.expect("the server writes well-formed frames"));
        }
        (code, out)
    }

    fn object(fields: Vec<(&str, Value)>) -> Value {
        Value::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn request(id: f64, method: &str, params: Value) -> Value {
        object(vec![
            ("jsonrpc", Value::Str("2.0".to_string())),
            ("id", Value::Number(id)),
            ("method", Value::Str(method.to_string())),
            ("params", params),
        ])
    }

    fn notification(method: &str, params: Value) -> Value {
        object(vec![("jsonrpc", Value::Str("2.0".to_string())), ("method", Value::Str(method.to_string())), ("params", params)])
    }

    fn initialize(encodings: &[&str]) -> Value {
        let names = encodings.iter().map(|e| Value::Str((*e).to_string())).collect();
        request(
            1.0,
            "initialize",
            object(vec![("capabilities", object(vec![("general", object(vec![("positionEncodings", Value::Array(names))]))]))]),
        )
    }

    fn open(uri: &str, text: &str) -> Value {
        notification(
            "textDocument/didOpen",
            object(vec![(
                "textDocument",
                object(vec![
                    ("uri", Value::Str(uri.to_string())),
                    ("languageId", Value::Str("shellscript".to_string())),
                    ("version", Value::Number(1.0)),
                    ("text", Value::Str(text.to_string())),
                ]),
            )]),
        )
    }

    fn doc(uri: &str) -> Value {
        object(vec![("textDocument", object(vec![("uri", Value::Str(uri.to_string()))]))])
    }

    fn position(line: f64, character: f64) -> Value {
        object(vec![("line", Value::Number(line)), ("character", Value::Number(character))])
    }

    /// The result of the response to request `id`.
    fn result(messages: &[Message], id: f64) -> Value {
        for m in messages {
            if let Message::Response { id: lsp::Id::Number(n), result } = m
                && *n as f64 == id
            {
                return result.clone().expect("the request succeeded");
            }
        }
        panic!("no response to request {id}");
    }

    fn error(messages: &[Message], id: f64) -> ResponseError {
        for m in messages {
            if let Message::Response { id: lsp::Id::Number(n), result } = m
                && *n as f64 == id
            {
                return result.clone().expect_err("the request failed");
            }
        }
        panic!("no response to request {id}");
    }

    /// Every `publishDiagnostics` the session produced, in order.
    fn publications(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .filter_map(|m| match m {
                Message::Notification { method, params } if method == "textDocument/publishDiagnostics" => Some(params.clone()),
                _ => None,
            })
            .collect()
    }

    fn codes(publication: &Value) -> Vec<String> {
        match json::query(publication, ".diagnostics") {
            Ok(Value::Array(items)) => items.iter().filter_map(|d| string_at(d, ".code")).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn it_announces_what_it_can_do_and_agrees_on_a_unit_to_count_in() {
        let (_, out) = session(&[initialize(&["utf-8", "utf-16", "utf-32"])]);
        let result = result(&out, 1.0);
        // utf-32 is the `char`, which is what bish measures in -- so
        // agreeing on it means no conversion at the boundary at all.
        assert_eq!(string_at(&result, ".capabilities.positionEncoding").as_deref(), Some("utf-32"));
        for provider in [".capabilities.documentFormattingProvider", ".capabilities.codeActionProvider", ".capabilities.hoverProvider"] {
            assert_eq!(json::query(&result, provider), Ok(&Value::Bool(true)), "{provider}");
        }
        assert_eq!(string_at(&result, ".serverInfo.name").as_deref(), Some("bish"));
    }

    // The protocol's own original unit, and the one a client that says
    // nothing is entitled to assume.
    #[test]
    fn a_client_that_offers_nothing_gets_utf16() {
        let (_, out) = session(&[request(1.0, "initialize", object(vec![("capabilities", object(vec![]))]))]);
        assert_eq!(string_at(&result(&out, 1.0), ".capabilities.positionEncoding").as_deref(), Some("utf-16"));
    }

    #[test]
    fn opening_a_document_publishes_what_bish_thinks_of_it() {
        let (_, out) = session(&[initialize(&["utf-32"]), open("file:///t.sh", "x=1\necho $x\n")]);
        let published = publications(&out);
        assert_eq!(published.len(), 1);
        assert_eq!(string_at(&published[0], ".uri").as_deref(), Some("file:///t.sh"));
        assert!(codes(&published[0]).contains(&"unquoted-expansion".to_string()), "{:?}", codes(&published[0]));
    }

    // The findings must be the ones `bish tool check` reports, because
    // they come from the same linter -- this is what says so.
    #[test]
    fn the_findings_are_the_linters_own() {
        let text = "x=1\necho $x\n";
        let (_, out) = session(&[initialize(&["utf-32"]), open("file:///t.sh", text)]);
        let served = codes(&publications(&out)[0]);
        let direct: Vec<String> = BashLinter.check(text).into_iter().map(|d| d.code.to_string()).collect();
        for code in direct {
            assert!(served.contains(&code), "{code} is missing from what the server published: {served:?}");
        }
    }

    #[test]
    fn a_change_republishes_and_a_close_withdraws() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "echo $x\n"),
            notification(
                "textDocument/didChange",
                object(vec![
                    ("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string())), ("version", Value::Number(2.0))])),
                    ("contentChanges", Value::Array(vec![object(vec![("text", Value::Str("echo \"$x\"\n".to_string()))])])),
                ]),
            ),
            notification("textDocument/didClose", doc("file:///t.sh")),
        ]);
        let published = publications(&out);
        assert_eq!(published.len(), 3);
        assert!(codes(&published[0]).contains(&"unquoted-expansion".to_string()));
        assert!(!codes(&published[1]).contains(&"unquoted-expansion".to_string()), "the quoting fixed it");
        // An empty list, not silence: findings belong to the server
        // until it withdraws them, and a closed document that never did
        // leaves them on screen for ever.
        assert_eq!(codes(&published[2]), Vec::<String>::new());
    }

    #[test]
    fn formatting_returns_the_whole_document_rewritten() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "greet() {\n  echo hi\n}\n"),
            request(2.0, "textDocument/formatting", object(vec![("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string()))]))])),
        ]);
        let Value::Array(edits) = result(&out, 2.0) else { panic!("formatting returns a list of edits") };
        assert_eq!(edits.len(), 1);
        assert_eq!(string_at(&edits[0], ".newText").as_deref(), Some("greet() {\n\techo hi\n}\n"));
        assert_eq!(json::query(&edits[0], ".range.start.line"), Ok(&Value::Number(0.0)));
    }

    #[test]
    fn a_document_already_formatted_gets_no_edit() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "greet() {\n\techo hi\n}\n"),
            request(2.0, "textDocument/formatting", doc("file:///t.sh")),
        ]);
        assert_eq!(result(&out, 2.0), Value::Array(Vec::new()));
    }

    // A formatter that guesses at the shape of something broken is how
    // an afternoon's work gets eaten. Same refusal `bish tool format`
    // makes.
    #[test]
    fn a_script_that_does_not_parse_is_not_reformatted() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "if true; then\n"),
            request(2.0, "textDocument/formatting", doc("file:///t.sh")),
        ]);
        assert_eq!(result(&out, 2.0), Value::Array(Vec::new()));
        // ...but it is still reported, because "this does not parse" is
        // the most useful thing anyone can be told about a file that
        // does not parse.
        assert!(codes(&publications(&out)[0]).contains(&"parse-error".to_string()));
    }

    #[test]
    fn a_rule_with_a_fix_becomes_a_quick_action() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "echo $x\n"),
            request(
                2.0,
                "textDocument/codeAction",
                object(vec![
                    ("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string()))])),
                    ("range", object(vec![("start", position(0.0, 0.0)), ("end", position(0.0, 7.0))])),
                    ("context", object(vec![("diagnostics", Value::Array(Vec::new()))])),
                ]),
            ),
        ]);
        let Value::Array(actions) = result(&out, 2.0) else { panic!("code actions are a list") };
        let quoting = actions.iter().find(|a| string_at(a, ".title").is_some_and(|t| t.starts_with("unquoted-expansion"))).expect("no such action");
        assert_eq!(string_at(quoting, ".kind").as_deref(), Some("quickfix"));
        let edit = json::query(quoting, r#".edit.changes["file:///t.sh"][0]"#).expect("one edit for this document");
        assert_eq!(string_at(edit, ".newText").as_deref(), Some("\"$x\""));
    }

    #[test]
    fn hover_says_what_the_editors_own_k_key_says() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "# Greets somebody.\ngreet() {\n\techo hi\n}\ngreet world\n"),
            request(
                2.0,
                "textDocument/hover",
                object(vec![("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string()))])), ("position", position(4.0, 2.0))]),
            ),
        ]);
        let text = string_at(&result(&out, 2.0), ".contents.value").expect("a hover with something in it");
        assert!(text.contains("Greets somebody."), "the doc comment above the definition: {text:?}");
    }

    // The server's hover is the editor's `K`, including when the
    // answer is "nothing" -- a word inside a quoted string is prose and
    // not a command, and both front ends say so the same way. Being
    // consistent with `K` is the point; a server that quietly answered
    // differently would be a second opinion nobody asked for.
    #[test]
    fn hover_on_prose_says_what_the_k_key_says_about_prose() {
        let line = "echo \"please deploy the app\"";
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", &format!("{line}\n")),
            request(
                2.0,
                "textDocument/hover",
                object(vec![("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string()))])), ("position", position(0.0, 15.0))]),
            ),
        ]);
        let served = string_at(&result(&out, 2.0), ".contents.value").expect("an answer");
        let chars: Vec<char> = line.chars().collect();
        let index = DocIndex::build_from_source(line, std::path::Path::new("/t.sh"));
        let editor = crate::docs::hover_lines_at(&chars, 15, line, &index, |_| None).join("\n");
        assert_eq!(served, editor);
    }

    #[test]
    fn hovering_where_there_is_no_word_at_all_says_nothing() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "\n\n"),
            request(
                2.0,
                "textDocument/hover",
                object(vec![("textDocument", object(vec![("uri", Value::Str("file:///t.sh".to_string()))])), ("position", position(0.0, 0.0))]),
            ),
        ]);
        assert_eq!(result(&out, 2.0), Value::Null);
    }

    // An outline wants every function, and the undocumented one is
    // exactly the one somebody is looking for the definition of --
    // which is why this does not go through `DocIndex`.
    #[test]
    fn the_outline_lists_undocumented_functions_too() {
        let (_, out) = session(&[
            initialize(&["utf-32"]),
            open("file:///t.sh", "# Documented.\nalpha() { :; }\nbeta() { :; }\n"),
            request(2.0, "textDocument/documentSymbol", doc("file:///t.sh")),
        ]);
        let Value::Array(symbols) = result(&out, 2.0) else { panic!("symbols are a list") };
        let names: Vec<String> = symbols.iter().filter_map(|s| string_at(s, ".name")).collect();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(json::query(&symbols[0], ".range.start.line"), Ok(&Value::Number(1.0)), "lines count from zero here");
        assert_eq!(string_at(&symbols[0], ".detail").as_deref(), Some("Documented."));
        assert_eq!(string_at(&symbols[1], ".detail"), None, "nothing to say is better than an empty string");
    }

    // The whole reason `positionEncoding` is negotiated. An astral
    // character is one `char` to bish, two UTF-16 code units to a
    // client counting the old way -- so the column of a finding after
    // one differs, and saying the wrong number puts the underline under
    // the wrong text.
    #[test]
    fn a_findings_column_is_reported_in_the_unit_that_was_agreed() {
        let text = "echo \"\u{1f600}\" $x\n";
        let column = |encodings: &[&str]| {
            let (_, out) = session(&[initialize(encodings), open("file:///t.sh", text)]);
            let published = publications(&out);
            json::query(&published[0], ".diagnostics[0].range.start.character").expect("a finding with a column").clone()
        };
        assert_eq!(column(&["utf-32"]), Value::Number(9.0), "one char for the emoji");
        assert_eq!(column(&["utf-16"]), Value::Number(10.0), "two code units for it");
        assert_eq!(column(&["utf-8"]), Value::Number(12.0), "four bytes for it");
    }

    // ------------------------------------------------------------------
    // The two halves, talking to each other
    // ------------------------------------------------------------------

    /// `target/<profile>/bish`, the same way the bash corpus finds it
    /// -- and the same reason it is current: an integration test under
    /// `tests/` makes `cargo test` build the binary first. Skips rather
    /// than fails when it is not there.
    fn bish_binary() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.parent()?.join("bish");
        path.exists().then_some(path)
    }

    /// The real client from lspclient.rs, driving the real server in
    /// this file, over a real pipe.
    ///
    /// Everything above tests this server against a description of the
    /// protocol; this tests it against the other end of it. The two
    /// halves were written months apart against the same spec, and a
    /// disagreement between them is exactly the kind that a test
    /// written from the same misunderstanding as the code would miss.
    #[test]
    fn bishs_own_client_can_talk_to_bishs_own_server() {
        let Some(binary) = bish_binary() else { return };
        let dir = std::env::temp_dir().join(format!("bish-lsp-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sh");
        let text = "x=1\necho $x\n";
        std::fs::write(&path, text).unwrap();

        let command = vec![binary.display().to_string(), "tool".to_string(), "lsp-server".to_string()];
        let mut server = crate::lspclient::Server::start(1, &command, "bish tool lsp-server", &dir, crate::lspclient::ApplyEdits::Never, Value::Null)
            .expect("the server starts");

        // Polling with a deadline rather than reading once: this drives
        // a real process over a real pipe, and how quickly it answers
        // is a fact about the machine.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !server.is_ready() && std::time::Instant::now() < deadline {
            server.service();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(server.is_ready(), "the server never finished initializing: {:?}", server.state());
        // The negotiation really happened, over the wire, between the
        // two ends of this codebase.
        assert_eq!(server.encoding(), PositionEncoding::Utf32);
        assert!(server.provides("documentFormattingProvider"));
        assert!(server.provides("hoverProvider"));

        let uri = crate::url::from_file_path(&path);
        server.open_document(&uri, "shellscript", 1, text);
        let mut findings = None;
        while findings.is_none() && std::time::Instant::now() < deadline {
            server.service();
            findings = server.take_diagnostics(&uri).map(|p| p.findings.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let findings = findings.expect("the server published diagnostics the client understood");
        assert!(findings.iter().any(|d| d.code == "unquoted-expansion"), "the client decoded the server's own finding: {findings:?}");
        // ...and they are exactly what this shell says about the file
        // when nothing is between the two, having gone out as JSON and
        // come back through a pipe.
        let mut over_the_wire: Vec<String> = findings.iter().map(|d| d.code.to_string()).collect();
        let mut in_process: Vec<String> = BashLinter.check(text).into_iter().map(|d| d.code.to_string()).collect();
        in_process.extend(BashFormatter.check(text).unwrap_or_default().into_iter().map(|d| d.code.to_string()));
        over_the_wire.sort();
        in_process.sort();
        assert_eq!(over_the_wire, in_process);

        server.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_method_it_does_not_serve_is_refused_by_name() {
        let (_, out) = session(&[initialize(&["utf-32"]), request(2.0, "textDocument/rename", object(vec![]))]);
        let e = error(&out, 2.0);
        assert_eq!(e.code, -32601);
        assert!(e.message.contains("textDocument/rename"), "{}", e.message);
    }

    // The spec's own contract for the two of them: an editor that exits
    // without shutting down first is an editor that crashed, and a
    // server that pretended otherwise would hide it.
    #[test]
    fn shutdown_then_exit_is_orderly_and_exit_alone_is_not() {
        let (orderly, _) = session(&[initialize(&["utf-32"]), request(2.0, "shutdown", Value::Null), notification("exit", Value::Null)]);
        assert_eq!(orderly, 0);
        let (abrupt, _) = session(&[initialize(&["utf-32"]), notification("exit", Value::Null)]);
        assert_eq!(abrupt, 1);
        // Stdin simply ending is the same thing without the courtesy of
        // saying so.
        let (dropped, _) = session(&[initialize(&["utf-32"])]);
        assert_eq!(dropped, 1);
    }
}
