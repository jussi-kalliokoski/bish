// `Content-Length: N\r\n\r\n<body>` -- the framing the Language Server
// Protocol and the Debug Adapter Protocol both use, and the only thing
// the two have in common. What rides inside it differs completely:
// LSP's bodies are JSON-RPC 2.0, DAP's are its own request/response/
// event envelope with its own sequence numbers.
//
// One implementation rather than two, because the parts worth getting
// right are not the obvious ones. A header that arrives split across
// two reads must not be half-consumed; a `Content-Length` that is
// absent, unparseable or enormous has to be refused rather than
// allocated; and a framing error is *terminal*, because the framing is
// the only thing that ever said where a message boundary was -- once it
// stops making sense there is nothing to resynchronise to, and emitting
// a cascade of nonsense from a stream we have lost our place in is
// worse than stopping.
//
// Deliberately knows nothing about JSON. It hands back bodies; what
// they mean is the protocol module's own business.
#![allow(dead_code)]

/// A body larger than this is refused outright rather than allocated.
/// Nothing legitimate comes close -- the biggest message in practice is
/// a `didChange` carrying a whole file, or a completion list a few
/// hundred KB wide -- and the length arrives as text from a process
/// that may be malfunctioning, so the one number that drives an
/// allocation gets a ceiling.
pub const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

pub struct Frames {
    /// Which protocol this is framing, so an error names it. The two
    /// look identical on the wire and a message that said only
    /// "framing error" would leave you guessing which subprocess died.
    protocol: &'static str,
    buf: Vec<u8>,
    failed: bool,
}

impl Frames {
    pub fn new(protocol: &'static str) -> Frames {
        Frames { protocol, buf: Vec::new(), failed: false }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        if !self.failed {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// The next complete body, if the bytes fed so far contain one.
    /// `None` means "not yet" -- call again after feeding more.
    ///
    /// `Err` is one of two quite different things, and the difference
    /// is whether `is_failed` is now set: a *framing* error is terminal
    /// and everything stops, while a body that is not UTF-8 arrived
    /// inside a frame that was well-formed, so the stream is still
    /// synchronised and the next message is still findable.
    pub fn take_body(&mut self) -> Option<Result<String, String>> {
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
            // between calls is what makes this correct regardless of
            // how the reads happened to land.
            return None;
        }
        let body = self.buf[body_start..body_start + length].to_vec();
        self.buf.drain(..body_start + length);
        Some(match String::from_utf8(body) {
            Ok(s) => Ok(s),
            Err(_) => Err("message body is not valid UTF-8".to_string()),
        })
    }

    fn fail(&mut self, why: &str) -> String {
        self.failed = true;
        self.buf.clear();
        format!("{} framing error, stream abandoned: {why}", self.protocol)
    }

    /// Whether a framing error has put this out of action. The owner of
    /// the subprocess treats this as "the connection is dead."
    pub fn is_failed(&self) -> bool {
        self.failed
    }
}

/// One framed message, ready to write.
pub fn encode(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_arrives_whole_however_the_reads_landed() {
        let framed = encode(r#"{"a":1}"#);
        // One byte at a time, which is the worst a pipe can do.
        let mut frames = Frames::new("test");
        for byte in &framed {
            assert!(frames.take_body().is_none(), "nothing is complete yet");
            frames.feed(&[*byte]);
        }
        assert_eq!(frames.take_body(), Some(Ok(r#"{"a":1}"#.to_string())));
        assert_eq!(frames.take_body(), None);
    }

    #[test]
    fn several_messages_in_one_read_all_come_out() {
        let mut frames = Frames::new("test");
        let mut bytes = encode("one");
        bytes.extend(encode("two"));
        frames.feed(&bytes);
        assert_eq!(frames.take_body(), Some(Ok("one".to_string())));
        assert_eq!(frames.take_body(), Some(Ok("two".to_string())));
        assert_eq!(frames.take_body(), None);
    }

    #[test]
    fn an_unknown_header_is_ignored_and_the_name_is_matched_loosely() {
        let mut frames = Frames::new("test");
        frames.feed(b"Content-Type: application/json\r\ncontent-length: 2\r\n\r\nhi");
        assert_eq!(frames.take_body(), Some(Ok("hi".to_string())));
    }

    // The framing is the only thing that ever said where a message
    // began, so losing it is not recoverable -- and pretending
    // otherwise would turn one bad byte into a stream of invented
    // messages.
    #[test]
    fn a_framing_error_stops_everything_after_it() {
        let mut frames = Frames::new("test");
        frames.feed(b"Content-Length: banana\r\n\r\nhi");
        let e = frames.take_body().expect("an error").expect_err("a framing error");
        assert!(e.contains("test framing error"), "{e}");
        assert!(e.contains("banana"), "{e}");
        assert!(frames.is_failed());
        frames.feed(&encode("a perfectly good message"));
        assert_eq!(frames.take_body(), None, "nothing is decoded after the stream is abandoned");
    }

    #[test]
    fn a_length_nothing_could_legitimately_send_is_refused_rather_than_allocated() {
        let mut frames = Frames::new("test");
        frames.feed(format!("Content-Length: {}\r\n\r\n", MAX_CONTENT_LENGTH + 1).as_bytes());
        let e = frames.take_body().expect("an error").expect_err("refused");
        assert!(e.contains("exceeds"), "{e}");
        assert!(frames.is_failed());
    }

    #[test]
    fn headers_with_no_length_are_a_framing_error() {
        let mut frames = Frames::new("test");
        frames.feed(b"Content-Type: application/json\r\n\r\nhi");
        assert!(frames.take_body().expect("an error").is_err());
        assert!(frames.is_failed());
    }

    // The frame was well-formed, so the next one is still findable --
    // which is exactly what makes this different from the errors above.
    #[test]
    fn a_body_that_is_not_utf8_is_reported_without_abandoning_the_stream() {
        let mut frames = Frames::new("test");
        frames.feed(b"Content-Length: 2\r\n\r\n\xff\xfe");
        assert!(frames.take_body().expect("an error").is_err());
        assert!(!frames.is_failed(), "the stream is still synchronised");
        frames.feed(&encode("and on we go"));
        assert_eq!(frames.take_body(), Some(Ok("and on we go".to_string())));
    }
}
