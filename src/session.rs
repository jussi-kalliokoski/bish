// Detachable `bish session` support -- the client/server split described
// in ../bish-detachable-sessions.md and the approved implementation plan
// (~/.claude/plans/melodic-sauteeing-boot.md). This module owns the
// pieces that don't need a running daemon to exist or be tested: where a
// session's socket/pidfile live, the framed wire protocol spoken over
// that socket, and verifying a connecting peer's real UID before
// trusting anything it sends. Daemonization, the actual accept loop, and
// `main.rs`'s `bish session <subcommand>` dispatch land in a follow-up
// commit once this foundation is in place.
//
// Same zero-external-dependency, hand-roll-it philosophy as the rest of
// this project: `std::os::unix::net::{UnixListener, UnixStream}` is
// already in the standard library, so the only real FFI this needs is
// `SO_PEERCRED` (Linux-only -- this codebase's pty.rs/term.rs are
// already explicitly scoped to Linux x86_64, same stance here, not a
// new boundary).
#![allow(dead_code)]

use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

unsafe extern "C" {
    fn getuid() -> u32;
    fn mkdir(path: *const i8, mode: u32) -> i32;
    fn getsockopt(sockfd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut u32) -> i32;
}

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;

// Matches glibc's `struct ucred` (Linux x86_64) field for field -- same
// "hand it straight to the syscall" reasoning term.rs's own Termios
// struct doc comment gives.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Ucred {
    pid: i32,
    uid: u32,
    gid: u32,
}

// The connecting peer's real UID for an already-`accept`ed UNIX-domain
// stream socket -- checked once per new connection, before trusting any
// byte it sends. Defense in depth on top of the socket directory's own
// `0700` permissions (see `socket_dir`), not a replacement for them: a
// misconfigured or shared runtime directory could otherwise let another
// local user's process connect at all; this is what actually refuses to
// speak to it once it has.
pub fn peer_uid(stream: &std::os::unix::net::UnixStream) -> io::Result<u32> {
    let mut cred = Ucred::default();
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe { getsockopt(stream.as_raw_fd(), SOL_SOCKET, SO_PEERCRED, &mut cred as *mut Ucred as *mut u8, &mut len) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

fn current_uid() -> u32 {
    unsafe { getuid() }
}

// Where every session's socket/pidfile lives: `$XDG_RUNTIME_DIR/bish`
// when set (the modern, tmpfs-backed, per-user-isolated convention --
// cleared automatically on logout, never accumulates stale files across
// reboots the way a `/tmp`-based one can), falling back to
// `/tmp/bish-<uid>` otherwise. The directory itself is created mode
// `0700` by `ensure_socket_dir` -- this function alone just computes the
// path, so it stays testable without touching the filesystem.
pub fn socket_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("bish");
    }
    PathBuf::from(format!("/tmp/bish-{}", current_uid()))
}

pub fn socket_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.sock", name))
}

pub fn pidfile_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.pid", name))
}

// Creates `socket_dir()` (and any missing parent, e.g. a not-yet-existing
// `$XDG_RUNTIME_DIR/bish`) mode `0700` if it doesn't already exist --
// `mkdir`'s own `-p`-style idempotence (EEXIST on an existing directory
// is success, not an error) via `std::fs::create_dir_all`'s directory
// walk, but the final component's own permissions are set explicitly via
// a raw `mkdir(2)` call rather than trusted to the umask `create_dir_all`
// would otherwise apply -- this directory holds a socket only this user
// should ever be able to connect to, so its mode must never depend on
// whatever umask happened to be active.
pub fn ensure_socket_dir() -> io::Result<PathBuf> {
    let dir = socket_dir();
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let rc = unsafe { mkdir(c_path.as_ptr(), 0o700) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    // Belt-and-suspenders: force the mode even if the directory already
    // existed with something looser (e.g. left over from before this
    // function existed, or a stale `/tmp` fallback from a previous OS
    // install) -- cheap, and closes exactly the class of gap GNU
    // screen's own socket-directory CVE history warns against (see
    // ../bish-detachable-sessions.md's tmux/screen research).
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

// The framed wire protocol spoken over a session socket. Kept
// deliberately minimal -- a type byte plus a big-endian u32 length
// prefix plus payload, no JSON/external framing crate, matching this
// project's own `json.rs`/`diff.rs` precedent of "small hand-rolled
// format, tested directly" -- four kinds, covering everything the
// client/server split (bish-detachable-sessions.md §5) actually needs:
// raw terminal I/O bytes in either direction (the hot path -- client
// keystrokes one way, the server's already-rendered ANSI frames the
// other, both just opaque bytes at this layer), the attach handshake a
// client sends once right after connecting, a resize notification a
// client sends whenever its own real terminal resizes, and an OSC-52-
// style passthrough message the server sends when something running in
// a pane wants the attached client's *real* terminal to see an escape
// sequence directly (clipboard writes today, see registers.rs's
// write_osc52 and bish-detachable-sessions.md §4 -- this module doesn't
// interpret the payload, just carries it).
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Bytes(Vec<u8>),
    Handshake { rows: u16, cols: u16, term: String, colorterm: String },
    Resize { rows: u16, cols: u16 },
    Passthrough(Vec<u8>),
}

const KIND_BYTES: u8 = 0;
const KIND_HANDSHAKE: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_PASSTHROUGH: u8 = 3;

impl Message {
    // Header (5 bytes: 1 kind + 4 big-endian length) followed by that
    // many payload bytes -- big-endian purely by convention (this
    // protocol has no other endianness-sensitive consumer to match), not
    // because anything here is genuinely cross-architecture-sensitive.
    pub fn encode(&self) -> Vec<u8> {
        let (kind, payload) = match self {
            Message::Bytes(b) => (KIND_BYTES, b.clone()),
            Message::Resize { rows, cols } => {
                let mut p = Vec::with_capacity(4);
                p.extend_from_slice(&rows.to_be_bytes());
                p.extend_from_slice(&cols.to_be_bytes());
                (KIND_RESIZE, p)
            }
            Message::Passthrough(b) => (KIND_PASSTHROUGH, b.clone()),
            Message::Handshake { rows, cols, term, colorterm } => {
                let mut p = Vec::new();
                p.extend_from_slice(&rows.to_be_bytes());
                p.extend_from_slice(&cols.to_be_bytes());
                encode_short_string(&mut p, term);
                encode_short_string(&mut p, colorterm);
                (KIND_HANDSHAKE, p)
            }
        };
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

// A `String` field inside a Handshake payload: one length byte (every
// real $TERM/$COLORTERM value is a handful of ASCII bytes -- 255 is
// generous headroom, not a real constraint) followed by that many UTF-8
// bytes.
fn encode_short_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(255) as u8;
    out.push(len);
    out.extend_from_slice(&bytes[..len as usize]);
}

fn decode_short_string(buf: &[u8], pos: &mut usize) -> Option<String> {
    let len = *buf.get(*pos)? as usize;
    *pos += 1;
    let bytes = buf.get(*pos..*pos + len)?;
    *pos += len;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

// Accumulates bytes arriving from a stream socket (which, unlike a
// single `read()` off a pty, offers no guarantee a whole Message arrives
// in one chunk -- a large paste or a big rendered frame can easily
// straddle two `read()`s) and yields complete Messages as enough bytes
// have accumulated for one. Mirrors this codebase's other incremental
// byte-stream consumers in shape (vt100.rs's own parser survives a
// control sequence split across two `feed()` calls the same way) --
// `feed` never blocks or does I/O itself, just appends; `next_message`
// is what actually tries to parse.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder { buf: Vec::new() }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    // `Ok(None)` means "not enough bytes yet for a whole message" (call
    // `feed` again and retry) -- not an error. `Err` is a genuinely
    // malformed stream (an unrecognized kind byte, or a Handshake
    // payload too short for its own declared string lengths) -- this
    // should never happen against this module's own `encode`, so in
    // practice only means "the peer on the other end isn't speaking this
    // protocol," worth closing the connection over, not retrying.
    pub fn next_message(&mut self) -> Result<Option<Message>, String> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let kind = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        let msg = match kind {
            KIND_BYTES => Message::Bytes(payload),
            KIND_PASSTHROUGH => Message::Passthrough(payload),
            KIND_RESIZE => {
                if payload.len() != 4 {
                    return Err(format!("malformed Resize payload: {} bytes, expected 4", payload.len()));
                }
                Message::Resize { rows: u16::from_be_bytes([payload[0], payload[1]]), cols: u16::from_be_bytes([payload[2], payload[3]]) }
            }
            KIND_HANDSHAKE => {
                if payload.len() < 4 {
                    return Err(format!("malformed Handshake payload: {} bytes, expected at least 4", payload.len()));
                }
                let rows = u16::from_be_bytes([payload[0], payload[1]]);
                let cols = u16::from_be_bytes([payload[2], payload[3]]);
                let mut pos = 4;
                let term = decode_short_string(&payload, &mut pos).ok_or("malformed Handshake: truncated term string")?;
                let colorterm = decode_short_string(&payload, &mut pos).ok_or("malformed Handshake: truncated colorterm string")?;
                Message::Handshake { rows, cols, term, colorterm }
            }
            other => return Err(format!("unrecognized message kind byte: {}", other)),
        };
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn socket_dir_prefers_xdg_runtime_dir_when_set() {
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1234") };
        assert_eq!(socket_dir(), PathBuf::from("/run/user/1234/bish"));
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn socket_dir_falls_back_to_tmp_when_xdg_runtime_dir_unset() {
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        let dir = socket_dir();
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => {}
        }
        // `Path::starts_with` compares whole path *components*, not
        // string prefixes -- "/tmp/bish-1000" doesn't component-wise
        // start with "/tmp/bish-" (its last component is "bish-1000",
        // not "bish-"), so this deliberately checks the rendered string
        // instead.
        assert!(dir.to_string_lossy().starts_with("/tmp/bish-"), "expected a /tmp/bish-<uid> fallback, got {:?}", dir);
    }

    #[test]
    fn socket_path_and_pidfile_path_are_named_consistently() {
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1234") };
        assert_eq!(socket_path("work"), PathBuf::from("/run/user/1234/bish/work.sock"));
        assert_eq!(pidfile_path("work"), PathBuf::from("/run/user/1234/bish/work.pid"));
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn ensure_socket_dir_creates_it_mode_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = std::env::temp_dir().join(format!("bish-session-test-{}", std::process::id()));
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        let dir = ensure_socket_dir().expect("ensure_socket_dir");
        let mode = std::fs::metadata(&dir).expect("metadata").permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&tmp);
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        assert_eq!(mode, 0o700, "expected the session socket dir to be mode 0700, got {:o}", mode);
    }

    #[test]
    fn ensure_socket_dir_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("bish-session-test-idem-{}", std::process::id()));
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &tmp) };
        ensure_socket_dir().expect("first call");
        let result = ensure_socket_dir();
        let _ = std::fs::remove_dir_all(&tmp);
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
        assert!(result.is_ok(), "calling ensure_socket_dir a second time should succeed, got {:?}", result);
    }

    #[test]
    fn peer_uid_reports_this_processs_own_real_uid_over_a_socketpair() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let uid = peer_uid(&a).expect("peer_uid");
        assert_eq!(uid, current_uid(), "a socketpair's peer is this same process");
    }

    #[test]
    fn bytes_message_round_trips() {
        let msg = Message::Bytes(b"hello, world".to_vec());
        let mut dec = Decoder::new();
        dec.feed(&msg.encode());
        assert_eq!(dec.next_message().unwrap(), Some(msg));
        assert_eq!(dec.next_message().unwrap(), None);
    }

    #[test]
    fn resize_message_round_trips() {
        let msg = Message::Resize { rows: 40, cols: 120 };
        let mut dec = Decoder::new();
        dec.feed(&msg.encode());
        assert_eq!(dec.next_message().unwrap(), Some(msg));
    }

    #[test]
    fn passthrough_message_round_trips() {
        let msg = Message::Passthrough(b"\x1b]52;c;aGVsbG8=\x07".to_vec());
        let mut dec = Decoder::new();
        dec.feed(&msg.encode());
        assert_eq!(dec.next_message().unwrap(), Some(msg));
    }

    #[test]
    fn handshake_message_round_trips() {
        let msg = Message::Handshake { rows: 24, cols: 80, term: "xterm-256color".to_string(), colorterm: "truecolor".to_string() };
        let mut dec = Decoder::new();
        dec.feed(&msg.encode());
        assert_eq!(dec.next_message().unwrap(), Some(msg));
    }

    #[test]
    fn decoder_waits_for_a_message_split_across_two_feeds() {
        let msg = Message::Bytes(b"a longer payload that we'll split mid-stream".to_vec());
        let encoded = msg.encode();
        let (first, second) = encoded.split_at(encoded.len() / 2);
        let mut dec = Decoder::new();
        dec.feed(first);
        assert_eq!(dec.next_message().unwrap(), None, "shouldn't parse a message from a partial buffer");
        dec.feed(second);
        assert_eq!(dec.next_message().unwrap(), Some(msg));
    }

    #[test]
    fn decoder_yields_several_messages_fed_in_one_batch() {
        let a = Message::Bytes(b"first".to_vec());
        let b = Message::Resize { rows: 10, cols: 20 };
        let c = Message::Bytes(b"third".to_vec());
        let mut batch = a.encode();
        batch.extend_from_slice(&b.encode());
        batch.extend_from_slice(&c.encode());
        let mut dec = Decoder::new();
        dec.feed(&batch);
        assert_eq!(dec.next_message().unwrap(), Some(a));
        assert_eq!(dec.next_message().unwrap(), Some(b));
        assert_eq!(dec.next_message().unwrap(), Some(c));
        assert_eq!(dec.next_message().unwrap(), None);
    }

    #[test]
    fn decoder_rejects_an_unrecognized_kind_byte() {
        let mut dec = Decoder::new();
        dec.feed(&[99, 0, 0, 0, 0]); // kind 99, zero-length payload
        assert!(dec.next_message().is_err());
    }

    #[test]
    fn decoder_rejects_a_malformed_resize_payload() {
        let mut dec = Decoder::new();
        dec.feed(&[KIND_RESIZE, 0, 0, 0, 2, 0, 0]); // 2-byte payload, needs 4
        assert!(dec.next_message().is_err());
    }

    #[test]
    fn empty_term_and_colorterm_strings_round_trip() {
        let msg = Message::Handshake { rows: 24, cols: 80, term: String::new(), colorterm: String::new() };
        let mut dec = Decoder::new();
        dec.feed(&msg.encode());
        assert_eq!(dec.next_message().unwrap(), Some(msg));
    }
}
