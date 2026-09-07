// The Debug Adapter Protocol, as far as it is a *wire format*: the
// envelope, and typed readings of the payloads a debugger client
// actually needs. Deliberately nothing else -- no process, no editor,
// no debugger. This module can be exercised entirely by handing it
// bytes and reading back messages, which is the same split, and the
// same reasoning, as `lsp.rs`.
//
// DAP is LSP's sibling: same people, same `Content-Length` framing
// (shared, see framing.rs), same job of putting a protocol between an
// editor and a tool so that M editors and N tools cost M + N instead
// of M x N. Inside the frame it is *not* JSON-RPC. Every message
// carries its own `seq`; a response names the `request_seq` it answers
// and carries `success` as a boolean rather than an error object; and
// an event is its own third kind rather than a request with no id.
// Close enough to LSP to be confusing, different enough that sharing
// the envelope would have been wrong.
//
// Messages are built on `json::Value` rather than a typed model, for
// the reason `lsp.rs` gives at length: a full one is thousands of
// optional fields that vary by adapter, and `json::query` already
// reaches into a reply whose exact shape we don't want to commit to.
// Typed structs earn their place where the *client* has to understand
// something, which is a much smaller set -- and every one of the ones
// below was written against what gdb actually sends, not against a
// reading of the specification.
#![allow(dead_code)]

use crate::json::{self, Value};

// ---------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Client to adapter. `seq` is ours and increases; the adapter
    /// echoes it back as `request_seq`.
    Request { seq: i64, command: String, arguments: Value },
    /// Adapter to client, answering one request.
    ///
    /// `Err` carries the adapter's own short message. DAP also allows a
    /// structured `body.error`, which nothing here needs: the string is
    /// what an adapter fills in and what a user can act on.
    Response { request_seq: i64, command: String, result: Result<Value, String> },
    /// Adapter to client, unprompted -- which is most of what a debug
    /// session is. `stopped` is the important one.
    Event { event: String, body: Value },
}

impl Message {
    pub fn to_value(&self) -> Value {
        match self {
            Message::Request { seq, command, arguments } => {
                let mut fields = vec![
                    ("seq".to_string(), Value::Number(*seq as f64)),
                    ("type".to_string(), Value::Str("request".to_string())),
                    ("command".to_string(), Value::Str(command.clone())),
                ];
                // Omitted rather than sent as null: an adapter that
                // reads `arguments` without checking gets an object or
                // nothing, never a null to trip over.
                if *arguments != Value::Null {
                    fields.push(("arguments".to_string(), arguments.clone()));
                }
                Value::Object(fields)
            }
            Message::Response { request_seq, command, result } => {
                let mut fields = vec![
                    ("type".to_string(), Value::Str("response".to_string())),
                    ("request_seq".to_string(), Value::Number(*request_seq as f64)),
                    ("command".to_string(), Value::Str(command.clone())),
                    ("success".to_string(), Value::Bool(result.is_ok())),
                ];
                match result {
                    Ok(body) if *body != Value::Null => fields.push(("body".to_string(), body.clone())),
                    Ok(_) => {}
                    Err(message) => fields.push(("message".to_string(), Value::Str(message.clone()))),
                }
                Value::Object(fields)
            }
            Message::Event { event, body } => {
                let mut fields = vec![("type".to_string(), Value::Str("event".to_string())), ("event".to_string(), Value::Str(event.clone()))];
                if *body != Value::Null {
                    fields.push(("body".to_string(), body.clone()));
                }
                Value::Object(fields)
            }
        }
    }

    /// The inverse. `Err` for anything that isn't recognisably one of
    /// the three shapes -- an adapter that sends such a thing is
    /// broken, and guessing which was meant would only move the failure
    /// somewhere less obvious.
    pub fn from_value(v: &Value) -> Result<Message, String> {
        let field = |name: &str| json::query(v, &format!(".{name}")).ok().filter(|v| **v != Value::Null).cloned();
        let string = |name: &str| match field(name) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        };
        let number = |name: &str| match field(name) {
            Some(Value::Number(n)) => Some(n as i64),
            _ => None,
        };
        match string("type").as_deref() {
            Some("request") => Ok(Message::Request {
                seq: number("seq").unwrap_or(0),
                command: string("command").ok_or("request without a command")?,
                arguments: field("arguments").unwrap_or(Value::Null),
            }),
            Some("response") => {
                let success = matches!(field("success"), Some(Value::Bool(true)));
                let result = match success {
                    true => Ok(field("body").unwrap_or(Value::Null)),
                    // An adapter that fails without saying why still
                    // has to say *something*, or the failure reaches
                    // the user as a blank.
                    false => Err(string("message").unwrap_or_else(|| "the adapter refused, without saying why".to_string())),
                };
                Ok(Message::Response {
                    request_seq: number("request_seq").ok_or("response without a request_seq")?,
                    command: string("command").unwrap_or_default(),
                    result,
                })
            }
            Some("event") => Ok(Message::Event { event: string("event").ok_or("event without a name")?, body: field("body").unwrap_or(Value::Null) }),
            Some(other) => Err(format!("message of unknown type {other:?}")),
            None => Err("message with no type".to_string()),
        }
    }
}

pub fn encode(message: &Message) -> Vec<u8> {
    crate::framing::encode(&json::compact_print(&message.to_value()))
}

/// The receiving half: bytes in (in whatever sizes a non-blocking read
/// happens to produce), whole messages out.
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
        Decoder { frames: crate::framing::Frames::new("DAP") }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.frames.feed(bytes);
    }

    pub fn take_message(&mut self) -> Option<Result<Message, String>> {
        match self.frames.take_body()? {
            Err(e) => Some(Err(e)),
            Ok(body) => Some(match json::parse(&body) {
                Err(e) => Err(format!("malformed JSON body: {e}")),
                Ok(value) => Message::from_value(&value),
            }),
        }
    }

    pub fn is_failed(&self) -> bool {
        self.frames.is_failed()
    }
}

// ---------------------------------------------------------------------
// The payloads a client has to understand
// ---------------------------------------------------------------------

/// Why the program stopped, and which thread did.
///
/// The single most important message in a debug session: everything the
/// user then sees -- the line the cursor lands on, the variables, the
/// call stack -- is fetched in response to one of these.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Stopped {
    /// `breakpoint`, `step`, `exception`, `pause`, `entry`... An
    /// adapter may invent its own, so this stays a string.
    pub reason: String,
    /// Which thread stopped. Absent in principle; in practice every
    /// adapter says, and a session with no thread id has nothing to
    /// ask about.
    pub thread_id: Option<i64>,
    /// Set when the reason deserves more than one word -- an exception
    /// message, most usefully.
    pub description: Option<String>,
    pub text: Option<String>,
    /// Which breakpoints were hit, when that is the reason.
    pub hit_breakpoint_ids: Vec<i64>,
    /// Whether the whole program stopped or only this thread.
    pub all_threads_stopped: bool,
}

pub fn stopped(body: &Value) -> Stopped {
    Stopped {
        reason: string(body, ".reason").unwrap_or_default(),
        thread_id: number(body, ".threadId"),
        description: string(body, ".description"),
        text: string(body, ".text"),
        hit_breakpoint_ids: match json::query(body, ".hitBreakpointIds") {
            Ok(Value::Array(ids)) => ids.iter().filter_map(|id| if let Value::Number(n) = id { Some(*n as i64) } else { None }).collect(),
            _ => Vec::new(),
        },
        all_threads_stopped: matches!(json::query(body, ".allThreadsStopped"), Ok(Value::Bool(true))),
    }
}

/// One frame of the call stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackFrame {
    /// The adapter's own handle for it, which `scopes` is asked in
    /// terms of. Not an index: gdb numbers from 0 and others do not,
    /// and inventing our own numbering would break `scopes`.
    pub id: i64,
    pub name: String,
    /// Where it is, when the adapter knows -- a frame inside a library
    /// with no debug info has a name and nothing else, and that is
    /// worth showing rather than hiding.
    pub source_path: Option<String>,
    pub line: usize,
    pub column: usize,
}

pub fn stack_frames(body: &Value) -> Vec<StackFrame> {
    let Ok(Value::Array(frames)) = json::query(body, ".stackFrames") else { return Vec::new() };
    frames
        .iter()
        .map(|f| StackFrame {
            id: number(f, ".id").unwrap_or(0),
            name: string(f, ".name").unwrap_or_default(),
            source_path: string(f, ".source.path"),
            line: number(f, ".line").unwrap_or(0).max(0) as usize,
            column: number(f, ".column").unwrap_or(0).max(0) as usize,
        })
        .collect()
}

/// A named group of variables in a frame -- gdb calls them "Arguments"
/// and "Locals"; other adapters add "Registers", "Globals".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub name: String,
    /// The handle to ask `variables` for.
    pub variables_reference: i64,
    /// The adapter warning that fetching this is slow enough to be
    /// worth not doing until asked. Honoured rather than ignored: a
    /// pause that stops to read every global is a pause that hangs.
    pub expensive: bool,
}

pub fn scopes(body: &Value) -> Vec<Scope> {
    let Ok(Value::Array(scopes)) = json::query(body, ".scopes") else { return Vec::new() };
    scopes
        .iter()
        .map(|s| Scope {
            name: string(s, ".name").unwrap_or_default(),
            variables_reference: number(s, ".variablesReference").unwrap_or(0),
            expensive: matches!(json::query(s, ".expensive"), Ok(Value::Bool(true))),
        })
        .collect()
}

/// One variable, as the adapter chose to render it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// Already a string, and deliberately not parsed: what a value
    /// *looks like* is the adapter's own decision -- a Rust `Vec`, a
    /// C++ `std::string`, a Python object all render through the
    /// debugger's own pretty-printers, and second-guessing that would
    /// mean showing something the debugger does not.
    pub value: String,
    pub type_name: Option<String>,
    /// Non-zero when this variable has children to expand -- a struct,
    /// an array, a pointer worth following. The handle to ask
    /// `variables` for, exactly as a scope's is.
    pub variables_reference: i64,
}

pub fn variables(body: &Value) -> Vec<Variable> {
    let Ok(Value::Array(vars)) = json::query(body, ".variables") else { return Vec::new() };
    vars.iter()
        .map(|v| Variable {
            name: string(v, ".name").unwrap_or_default(),
            value: string(v, ".value").unwrap_or_default(),
            type_name: string(v, ".type"),
            variables_reference: number(v, ".variablesReference").unwrap_or(0),
        })
        .collect()
}

/// What became of a breakpoint the client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakpoint {
    pub id: Option<i64>,
    /// Whether the adapter could actually place it. A breakpoint on a
    /// line with no code, or in a file the program does not include,
    /// comes back unverified -- and showing it as set anyway is a lie
    /// the user finds out about by the program not stopping.
    pub verified: bool,
    /// Where it actually landed, which is not always where it was
    /// asked for: a breakpoint on a blank line moves to the next
    /// statement.
    pub line: Option<usize>,
    pub source_path: Option<String>,
    /// Why it could not be placed, when the adapter says.
    pub message: Option<String>,
}

fn breakpoint(v: &Value) -> Breakpoint {
    Breakpoint {
        id: number(v, ".id"),
        verified: matches!(json::query(v, ".verified"), Ok(Value::Bool(true))),
        line: number(v, ".line").map(|n| n.max(0) as usize),
        source_path: string(v, ".source.path"),
        message: string(v, ".message").or_else(|| string(v, ".reason")),
    }
}

/// The `setBreakpoints` reply: one entry per breakpoint asked for, in
/// the same order.
pub fn breakpoints(body: &Value) -> Vec<Breakpoint> {
    let Ok(Value::Array(items)) = json::query(body, ".breakpoints") else { return Vec::new() };
    items.iter().map(breakpoint).collect()
}

/// The `breakpoint` *event*: an adapter revising what it said earlier.
///
/// Not an optional extra. gdb answers `setBreakpoints` before the
/// program is running, when it cannot yet know whether a location
/// exists, so every breakpoint comes back `verified: false` and is
/// upgraded by one of these once the program loads. A client that read
/// only the reply would show every breakpoint as broken for ever.
pub fn breakpoint_event(body: &Value) -> Option<Breakpoint> {
    let bp = json::query(body, ".breakpoint").ok()?;
    Some(breakpoint(bp))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

pub fn threads(body: &Value) -> Vec<Thread> {
    let Ok(Value::Array(items)) = json::query(body, ".threads") else { return Vec::new() };
    items.iter().map(|t| Thread { id: number(t, ".id").unwrap_or(0), name: string(t, ".name").unwrap_or_default() }).collect()
}

/// The program's own output, relayed by the adapter.
///
/// `category` separates the program's `stdout` and `stderr` from the
/// adapter's own chatter (`console`, `important`) -- worth keeping,
/// since showing gdb's copyright banner as if the program had printed
/// it would be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub category: String,
    pub output: String,
}

pub fn output(body: &Value) -> Output {
    Output { category: string(body, ".category").unwrap_or_else(|| "console".to_string()), output: string(body, ".output").unwrap_or_default() }
}

/// What `evaluate` came back with -- a watch expression, or a hover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluated {
    pub result: String,
    pub type_name: Option<String>,
    pub variables_reference: i64,
}

pub fn evaluated(body: &Value) -> Evaluated {
    Evaluated {
        result: string(body, ".result").unwrap_or_default(),
        type_name: string(body, ".type"),
        variables_reference: number(body, ".variablesReference").unwrap_or(0),
    }
}

/// Whether the adapter said it can do something -- the `initialize`
/// reply is a bag of `supportsXRequest` booleans, and asking for
/// something an adapter does not have is how a session dies on a
/// refusal rather than a missing feature.
pub fn supports(capabilities: &Value, name: &str) -> bool {
    matches!(json::query(capabilities, &format!(".{name}")), Ok(Value::Bool(true)))
}

fn string(value: &Value, path: &str) -> Option<String> {
    match json::query(value, path) {
        Ok(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn number(value: &Value, path: &str) -> Option<i64> {
    match json::query(value, path) {
        Ok(Value::Number(n)) => Some(*n as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Message {
        Message::from_value(&json::parse(text).expect("valid JSON")).expect("a recognisable message")
    }

    fn body(text: &str) -> Value {
        json::parse(text).expect("valid JSON")
    }

    #[test]
    fn a_request_round_trips_through_its_own_envelope() {
        let m = Message::Request {
            seq: 7,
            command: "setBreakpoints".to_string(),
            arguments: body(r#"{"source":{"path":"/t.c"},"breakpoints":[{"line":2}]}"#),
        };
        let encoded = json::compact_print(&m.to_value());
        assert_eq!(Message::from_value(&json::parse(&encoded).unwrap()).unwrap(), m);
        assert!(encoded.contains(r#""type":"request""#), "{encoded}");
    }

    #[test]
    fn an_event_and_a_response_are_told_apart_by_their_type() {
        assert_eq!(
            parse(r#"{"type":"event","event":"stopped","body":{"reason":"breakpoint"}}"#),
            Message::Event { event: "stopped".to_string(), body: body(r#"{"reason":"breakpoint"}"#) }
        );
        assert_eq!(
            parse(r#"{"type":"response","request_seq":3,"command":"threads","success":true,"body":{"threads":[]}}"#),
            Message::Response { request_seq: 3, command: "threads".to_string(), result: Ok(body(r#"{"threads":[]}"#)) }
        );
    }

    // `success` is a boolean here, not the absence of an error object
    // -- the one place the envelope most resembles JSON-RPC and is not
    // it.
    #[test]
    fn a_failed_response_carries_the_adapters_own_words() {
        let m = parse(r#"{"type":"response","request_seq":4,"command":"evaluate","success":false,"message":"No symbol \"nope\""}"#);
        assert_eq!(m, Message::Response { request_seq: 4, command: "evaluate".to_string(), result: Err("No symbol \"nope\"".to_string()) });
    }

    #[test]
    fn a_failure_with_nothing_to_say_still_says_something() {
        let Message::Response { result, .. } = parse(r#"{"type":"response","request_seq":4,"command":"x","success":false}"#) else {
            panic!("a response");
        };
        assert!(result.unwrap_err().contains("without saying why"));
    }

    #[test]
    fn a_message_that_is_none_of_the_three_is_refused() {
        for text in [r#"{"type":"banana"}"#, r#"{"seq":1}"#, r#"{"type":"response","command":"x","success":true}"#] {
            let v = json::parse(text).unwrap();
            assert!(Message::from_value(&v).is_err(), "{text}");
        }
    }

    // Everything below was written against what gdb 17 actually sends,
    // captured from a real session -- not from a reading of the
    // specification. The two differ in the details that matter.

    #[test]
    fn a_stopped_event_says_why_and_which_thread() {
        let s = stopped(&body(r#"{"threadId":1,"allThreadsStopped":true,"hitBreakpointIds":[1],"reason":"breakpoint"}"#));
        assert_eq!(s.reason, "breakpoint");
        assert_eq!(s.thread_id, Some(1));
        assert_eq!(s.hit_breakpoint_ids, vec![1]);
        assert!(s.all_threads_stopped);
    }

    #[test]
    fn a_stack_frame_keeps_the_adapters_own_handle() {
        let frames = stack_frames(&body(
            r#"{"stackFrames":[{"id":0,"line":2,"column":0,"source":{"name":"prog.c","path":"/tmp/prog.c"},"name":"add"},
                              {"id":1,"line":5,"column":0,"name":"main"}]}"#,
        ));
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], StackFrame { id: 0, name: "add".into(), source_path: Some("/tmp/prog.c".into()), line: 2, column: 0 });
        // A frame with no source is still a frame worth showing.
        assert_eq!(frames[1].source_path, None);
        assert_eq!(frames[1].name, "main");
    }

    #[test]
    fn scopes_and_variables_carry_the_handles_they_are_asked_by() {
        let s = scopes(&body(
            r#"{"scopes":[{"variablesReference":1,"name":"Arguments","expensive":false},
                          {"variablesReference":2,"name":"Locals","expensive":true}]}"#,
        ));
        assert_eq!(s[0], Scope { name: "Arguments".into(), variables_reference: 1, expensive: false });
        assert!(s[1].expensive, "an adapter that says this means it");

        let v = variables(&body(
            r#"{"variables":[{"variablesReference":0,"name":"a","value":"0"},
                                                 {"variablesReference":5,"name":"p","value":"0x7fff","type":"char *"}]}"#,
        ));
        assert_eq!(v[0], Variable { name: "a".into(), value: "0".into(), type_name: None, variables_reference: 0 });
        assert_eq!(v[1].variables_reference, 5, "this one has children to expand");
        assert_eq!(v[1].type_name.as_deref(), Some("char *"));
    }

    // The reply gdb actually gives before the program is running: it
    // cannot know yet whether the location exists.
    #[test]
    fn a_breakpoint_starts_unverified_and_an_event_upgrades_it() {
        let first = breakpoints(&body(r#"{"breakpoints":[{"id":1,"verified":false,"reason":"pending"}]}"#));
        assert_eq!(first[0].id, Some(1));
        assert!(!first[0].verified);
        assert_eq!(first[0].message.as_deref(), Some("pending"));

        let later = breakpoint_event(&body(r#"{"reason":"changed","breakpoint":{"id":1,"verified":true,"source":{"path":"/tmp/prog.c"},"line":2}}"#))
            .expect("a breakpoint in it");
        assert!(later.verified);
        assert_eq!(later.line, Some(2));
        assert_eq!(later.source_path.as_deref(), Some("/tmp/prog.c"));
    }

    #[test]
    fn output_keeps_the_programs_own_words_apart_from_the_adapters() {
        assert_eq!(output(&body(r#"{"category":"stdout","output":"6\n"}"#)), Output { category: "stdout".into(), output: "6\n".into() });
        // gdb's banner arrives with no category at all, and defaulting
        // it to the program's own output would put the copyright notice
        // in the program's console.
        assert_eq!(output(&body(r#"{"output":"GNU gdb (GDB) 17.2\n"}"#)).category, "console");
    }

    #[test]
    fn capabilities_are_read_by_name() {
        let caps = body(r#"{"supportsTerminateRequest":true,"supportsRestartRequest":false}"#);
        assert!(supports(&caps, "supportsTerminateRequest"));
        assert!(!supports(&caps, "supportsRestartRequest"));
        assert!(!supports(&caps, "supportsSomethingNobodyHas"), "silence is not consent");
    }

    #[test]
    fn a_whole_session_decodes_from_the_bytes_of_one() {
        let mut decoder = Decoder::new();
        for m in [
            Message::Response { request_seq: 1, command: "initialize".to_string(), result: Ok(body(r#"{"supportsTerminateRequest":true}"#)) },
            Message::Event { event: "initialized".to_string(), body: Value::Null },
            Message::Event { event: "stopped".to_string(), body: body(r#"{"reason":"breakpoint","threadId":1}"#) },
        ] {
            decoder.feed(&encode(&m));
        }
        let mut seen = Vec::new();
        while let Some(m) = decoder.take_message() {
            seen.push(m.expect("well-formed"));
        }
        assert_eq!(seen.len(), 3);
        assert!(matches!(&seen[1], Message::Event { event, .. } if event == "initialized"));
    }
}
