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
    fn encoding_names_round_trip_and_utf32_is_preferred() {
        assert_eq!(PositionEncoding::PREFERRED[0], PositionEncoding::Utf32);
        for encoding in PositionEncoding::PREFERRED {
            assert_eq!(PositionEncoding::from_wire_name(encoding.wire_name()), Some(encoding));
        }
        assert_eq!(PositionEncoding::from_wire_name("utf-7"), None);
    }
}
