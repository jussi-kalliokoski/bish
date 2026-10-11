// `Content-Length: N\r\n\r\n<body>` framing, shared by the Language
// Server Protocol and the Debug Adapter Protocol; the bodies differ
// (JSON-RPC 2.0 for LSP, DAP's own envelope).
//
// The parts worth getting right: a header split across reads mustn't be
// half-consumed; a `Content-Length` that's missing, unparseable or huge
// is refused, not allocated; and a framing error is terminal, since the
// framing is the only record of message boundaries. Knows nothing about
// JSON.

/// A larger body is refused rather than allocated. Real messages (a
/// `didChange` with a whole file, a big completion list) are far below
/// it, and the length comes from a process that may be malfunctioning.
pub const MAX_CONTENT_LENGTH: usize = 64 * 1024 * 1024;

pub struct Frames {
    /// Which protocol this frames, so an error names the subprocess.
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

    /// The next complete body, if the bytes fed so far hold one; `None`
    /// until more is fed. `Err` is either a framing error, which sets
    /// `is_failed` and stops everything, or a non-UTF-8 body in a well-formed
    /// frame, after which the next message is still findable.
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
            // Header names are case-insensitive; servers write
            // `Content-Length`, but the spec doesn't promise it.
            if name.trim().eq_ignore_ascii_case("content-length") {
                match value.trim().parse::<usize>() {
                    Ok(n) if n <= MAX_CONTENT_LENGTH => length = Some(n),
                    Ok(n) => return Some(Err(self.fail(&format!("Content-Length {n} exceeds the {MAX_CONTENT_LENGTH}-byte limit")))),
                    Err(_) => return Some(Err(self.fail(&format!("unparseable Content-Length: {:?}", value.trim())))),
                }
            }
            // Other headers (`Content-Type`, inventions) are ignored.
        }
        let Some(length) = length else {
            return Some(Err(self.fail("headers with no Content-Length")));
        };
        let body_start = header_end + 4;
        if self.buf.len() < body_start + length {
            // Header complete, body not: leave everything and re-parse the
            // header next time. Keeping no partial state is what makes this
            // right however the reads land.
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

    /// Whether a framing error has stopped this; the subprocess's owner
    /// treats it as a dead connection.
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

    // Losing the framing isn't recoverable; carrying on would invent
    // messages from one bad byte.
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

    // The frame was well-formed, so the next one is still findable.
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
