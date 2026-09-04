// Detachable `bish session` support -- the client/server split described
// in ../bish-detachable-sessions.md and the approved implementation plan
// (~/.claude/plans/melodic-sauteeing-boot.md). This module owns
// everything specific to that split: where a session's socket/pidfile
// live, the framed wire protocol spoken over that socket, verifying a
// connecting peer's real UID before trusting anything it sends, the
// daemon bootstrap (SessionBridge, run_daemon), and the client loop
// (run_client). `main.rs`'s actual `bish session <subcommand>` argv
// dispatch lands in a follow-up commit -- everything here is already
// directly callable, just not yet wired to a CLI entry point.
//
// The daemon side deliberately does *not* require any changes to
// repl::run or anything under it: run_daemon gives the daemon its own
// local pty pair (pty::attach_self_to_pty) and attaches the daemon's
// own fd 0/1/2 to the slave side, so every existing termios/ioctl/read/
// write call in this codebase keeps working completely unmodified --
// as far as repl::run is concerned, it has a real terminal, because it
// does. SessionBridge is the one new piece: a stateless byte relay
// between that pty's *master* side (which the daemon keeps) and
// whichever client socket is currently attached, if any -- no access
// to Shell/SessionState/any Rc<RefCell> state at all, so it's safe to
// service from inside the single-threaded on_idle mechanism every
// blocking loop in repl.rs already calls (service_background_jobs) via
// service_current_bridge below, without threading a new parameter
// through the half-dozen-plus functions between here and there. Same
// "this process is single-threaded, so plain global state describing
// what the whole process is doing right now is fine" reasoning
// exec.rs's own bare `static WINCH_FLAG` already relies on.
//
// Same zero-external-dependency, hand-roll-it philosophy as the rest of
// this project: `std::os::unix::net::{UnixListener, UnixStream}` is
// already in the standard library, so the only real FFI this needs is
// `SO_PEERCRED` (Linux-only -- this codebase's pty.rs/term.rs are
// already explicitly scoped to Linux x86_64, same stance here, not a
// new boundary).
#![allow(dead_code)]

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

unsafe extern "C" {
    fn getuid() -> u32;
    fn mkdir(path: *const i8, mode: u32) -> i32;
    fn getsockopt(sockfd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut u32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn flock(fd: i32, operation: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const SOL_SOCKET: i32 = 1;
const SO_PEERCRED: i32 = 17;
const LOCK_EX: i32 = 2;
const LOCK_UN: i32 = 8;
const LOCK_NB: i32 = 4;
const SIGTERM: i32 = 15;

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

// A session name becomes a filename inside the 0700 socket directory,
// so it has to be one component and nothing else. `bish session new
// ../../tmp/x` would otherwise write outside that directory, and `bish
// session attach ../../...` would connect to an arbitrary socket path.
//
// The user supplies their own argv today, so this is not a privilege
// boundary yet -- it becomes one the moment a name can come from
// anywhere else (a config file, a hook, `$BISH_SESSION`), and the
// cheapest time to draw the line is before that happens.
pub fn check_name(name: &str) -> io::Result<()> {
    let ok = !name.is_empty() && name != "." && name != ".." && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.');
    match ok {
        true => Ok(()),
        false => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid session name '{name}': use letters, digits, '-', '_' and '.'"))),
    }
}

pub fn socket_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.sock", name))
}

pub fn pidfile_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{}.pid", name))
}

// Opens (creating if needed) `name`'s pidfile, writes this process's own
// PID into it, and takes an exclusive, non-blocking `flock` on it. The
// returned File must simply stay alive for the rest of this process's
// life -- dropping it releases the lock, which is exactly what tells
// `is_daemon_alive` this daemon is gone. This is what actually lets
// `ls`/`kill` tell a live daemon apart from a stale leftover file: the
// file's own *content* (a PID number) can't do that on its own, since a
// PID can be reused by an unrelated process after a crash -- see GNU
// screen's own CVE-2023-24626 history (../bish-detachable-sessions.md's
// research) for exactly the class of bug that comes from trusting a raw
// PID number alone. Also doubles as the guard against two concurrent
// `bish session new` calls for the same name racing each other: the
// second one's own `flock` attempt fails here, before it ever touches
// the socket file.
fn acquire_pidfile_lock(name: &str) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(pidfile_path(name))?;
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    use std::io::Write as _;
    let mut f = &file;
    write!(f, "{}", std::process::id())?;
    Ok(file)
}

// True if `name`'s pidfile exists and is currently locked by a live
// daemon -- probes with its own non-blocking exclusive-lock attempt
// (immediately released either way, win or lose) rather than trusting
// the file's content, for the same PID-reuse reason
// acquire_pidfile_lock's own doc comment gives. A missing pidfile
// (never created, or already cleaned up) is simply "not alive," not an
// error.
fn is_daemon_alive(name: &str) -> bool {
    let file = match std::fs::File::open(pidfile_path(name)) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
        // Nobody else was holding it -- stale. Release our own probe
        // lock immediately; this function only ever answers a question,
        // it doesn't hold anything.
        unsafe { flock(file.as_raw_fd(), LOCK_UN) };
        false
    } else {
        true
    }
}

fn read_pid(name: &str) -> Option<i32> {
    // First line only: the daemon keeps a status line underneath it
    // (see write_status), and a pid is still the first thing anyone
    // reading this file wants.
    std::fs::read_to_string(pidfile_path(name)).ok()?.lines().next()?.trim().parse().ok()
}

/// How many clients are attached to `name` right now, as its daemon
/// last reported.
///
/// The daemon is the only thing that can know this, so it writes it
/// down: `ls` runs in a different process entirely and has no way to
/// ask. Rewritten only when the number actually changes, so an idle
/// session touches nothing. `None` means the daemon predates the status
/// line or has not written one yet, which reads as "unknown" rather
/// than "nobody".
fn read_attached(name: &str) -> Option<usize> {
    let text = std::fs::read_to_string(pidfile_path(name)).ok()?;
    text.lines().nth(1)?.strip_prefix("attached ")?.trim().parse().ok()
}

/// Rewrites `pidfile` as the pid line plus the current attached count.
///
/// Through the open descriptor rather than the path, which is what
/// makes `session rename` safe: the file can be renamed out from under
/// a running daemon and this keeps writing to the same inode.
fn write_status(pidfile: &mut std::fs::File, attached: usize) {
    use std::io::{Seek, SeekFrom};
    let text = format!("{}\nattached {}\n", std::process::id(), attached);
    let _ = pidfile.seek(SeekFrom::Start(0));
    let _ = pidfile.set_len(0);
    let _ = pidfile.write_all(text.as_bytes());
    let _ = pidfile.flush();
}

/// When this session was created, from the socket file's own mtime.
///
/// The socket is written once, at bind, and never again -- unlike the
/// pidfile, which the status line rewrites. So it is the one thing in
/// the runtime directory that still remembers when the session started.
fn session_age(name: &str) -> Option<std::time::Duration> {
    let created = std::fs::metadata(socket_path(name)).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(created).ok()
}

/// A duration as a person would say it: the largest unit that is not
/// zero, and nothing after it. "3h" is more useful in a list than
/// "3h07m12s".
fn short_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The directory a session is in, read out of `/proc`.
///
/// The daemon *is* the shell process, so its own cwd is the session's
/// -- a `cd` in the session moves it. Linux-only, like the rest of this
/// module's syscalls (see the file header); `None` wherever /proc is
/// not there or the link cannot be read.
fn session_cwd(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
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
        // EEXIST is not the same as "the directory I meant is already
        // there". On the `/tmp` fallback path anyone can pre-create
        // `/tmp/bish-<uid>` -- including as a symlink into a directory
        // they own -- and mkdir would report exactly this. The
        // set_permissions below follows symlinks, so accepting EEXIST
        // blindly means chmod'ing a stranger's directory 0700 and then
        // putting this session's socket and pidfile inside it.
        //
        // symlink_metadata does not follow, so this sees the entry
        // itself: it must be a real directory, and it must be ours.
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", dir.display())));
        }
        if meta.uid() != current_uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is owned by uid {}, not by you", dir.display(), meta.uid())));
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
    Handshake {
        rows: u16,
        cols: u16,
        term: String,
        colorterm: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Passthrough(Vec<u8>),
    /// `bish session capture <name>`: send me what the focused pane
    /// currently shows. Client -> server.
    CaptureRequest,
    /// The answer, as plain text with a newline between rows. Server ->
    /// client, and the only message a client both waits for and then
    /// exits on.
    CaptureReply(Vec<u8>),
}

const KIND_BYTES: u8 = 0;
const KIND_HANDSHAKE: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_PASSTHROUGH: u8 = 3;
const KIND_CAPTURE_REQUEST: u8 = 4;
const KIND_CAPTURE_REPLY: u8 = 5;

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
            Message::CaptureRequest => (KIND_CAPTURE_REQUEST, Vec::new()),
            Message::CaptureReply(b) => (KIND_CAPTURE_REPLY, b.clone()),
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
// The largest frame this protocol will accept. `try_accept` already
// drops any connection whose peer uid is not ours, so the peer is the
// same user -- but a session socket outlives the process that made it,
// and same-uid is not same-trust. A length prefix is a promise about
// bytes that have not arrived yet: without a ceiling a peer can declare
// 4 GiB, dribble, and `feed` grows the buffer forever.
//
// 16 MiB is far past any real frame (a full repaint of a large terminal
// is tens of kilobytes) and far below anything that matters. Same
// reasoning as lsp.rs's MAX_CONTENT_LENGTH, which this protocol should
// have had from the start.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

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
        // Checked before the "not enough bytes yet" return, so an
        // oversized declaration is refused on the first five bytes
        // rather than after the peer has had a chance to send them.
        if len > MAX_FRAME {
            return Err(format!("frame of {len} bytes exceeds the {MAX_FRAME}-byte limit"));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        let msg = match kind {
            KIND_BYTES => Message::Bytes(payload),
            KIND_PASSTHROUGH => Message::Passthrough(payload),
            KIND_CAPTURE_REQUEST => Message::CaptureRequest,
            KIND_CAPTURE_REPLY => Message::CaptureReply(payload),
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

// The byte relay between a daemon's own pty master and whichever client
// socket is currently attached, if any.
//
// The pty-master-*reading* half runs on its own dedicated background
// thread (drain_pty_master_thread below), not through the on_idle
// mechanism every other part of this bridge uses -- found necessary the
// hard way, via an actual reproducible hang, not assumed upfront. The
// problem: `editor::read_line`/`vk.next_key`'s own on_idle callback
// (`read_key_idle`, editor.rs) only runs while genuinely *waiting* for
// the next byte -- once bytes are already sitting in the pty's read
// queue (a paste, or any input arriving faster than one keystroke at a
// time, which a client relaying a whole burst from its own socket read
// absolutely can produce), every one of them gets consumed and
// redrawn in a tight loop with no on_idle call in between. If the
// *cumulative* redraw output from that burst exceeds the pty's own
// kernel output-queue capacity, the shell's own next `io::stdout()`
// write blocks -- and since nothing else was draining the master side
// (on_idle never got a turn to), it stays blocked forever. Reproduced
// directly: a short command (`echo hi`) never triggered this, but a
// single `e <long path>\n` reliably hung the whole daemon. A dedicated
// thread that does nothing but block-read the master and forward
// whatever arrives keeps that queue draining continuously regardless
// of what the main, single-threaded, on_idle-driven side is doing --
// this closes the deadlock at its actual root rather than trying to
// make every possible burst-producing code path yield more often.
// Never touches Shell/SessionState/any Rc<RefCell> state (just raw
// fds), so this doesn't reopen the single-threaded-by-design question
// for anything else in this codebase -- see this module's own top doc
// comment on why bish stays single-threaded everywhere else.
//
// The client-*input* direction (drain_client below) stays on the main,
// on_idle-driven thread: writing a client's own input into the pty
// master is comparatively small and immediately consumed by the
// already-continuously-reading shell process on the other end, so it
// doesn't have the same unbounded-accumulation shape -- and keeping it
// on the main thread is what lets accept/attach/detach state
// (`client_read`, `just_attached`) stay a single, non-atomic
// `Option`/`bool` with no synchronization needed.
// The size a session shared by these clients runs at: the smallest in
// each direction, taken independently, so a tall narrow terminal beside
// a short wide one leaves both able to see the whole screen.
//
// A client that has not sent its Handshake yet reports (0, 0) and is
// skipped -- it must not shrink the session to nothing for everybody
// else while its first message is still in flight. `None` (nobody
// attached, or nobody who has said anything yet) means leave the pty at
// whatever size it already has.
fn smallest_size(sizes: &[(u16, u16)]) -> Option<(u16, u16)> {
    let rows = sizes.iter().map(|(r, _)| *r).filter(|r| *r > 0).min()?;
    let cols = sizes.iter().map(|(_, c)| *c).filter(|c| *c > 0).min()?;
    Some((rows, cols))
}

// Names one attached client for as long as it is attached. Never
// reused, so the write half the drain thread holds and the read half
// the main thread holds can always be matched back up -- pairing them
// by position in two separate lists would go wrong the first time a
// client in the middle detached.
type ClientId = u64;

// One attached client, from the main thread's side.
struct Client {
    id: ClientId,
    // Read from directly by drain_one_client; the matching write half
    // lives in `writes`, where the drain thread can reach it.
    read: UnixStream,
    // Per client, because two clients' byte streams have nothing to do
    // with each other: one decoder shared between them would splice
    // half of one client's frame onto half of another's.
    decoder: Decoder,
    // The last size this client reported -- its Handshake, then every
    // Resize. The pty gets the *smallest* of these, which is what makes
    // a session shared by two differently-sized terminals legible in
    // both. tmux's own rule.
    size: (u16, u16),
}

// How many clients one session will hold at once. No real workflow
// wants more than a couple; the cap is here so a runaway attach loop
// cannot walk the daemon out of file descriptors.
const MAX_CLIENTS: usize = 16;

pub struct SessionBridge {
    listener: UnixListener,
    // The main thread's own handles, one per attached client.
    clients: Vec<Client>,
    // Shared with the background pty-master-draining thread -- the only
    // thing that thread ever touches. Every attached client's write
    // half, each tagged with its own id so the main thread can take one
    // away without disturbing the others. Kept in step with `clients`
    // by attach/detach, the only two places either is ever changed.
    writes: Arc<Mutex<Vec<(ClientId, UnixStream)>>>,
    next_client_id: ClientId,
    // The daemon's own pidfile, held open for its lock and written
    // through for the status line -- see write_status. `None` outside a
    // real daemon (a test bridge has no pidfile to keep current).
    pidfile: Option<std::fs::File>,
    // What the status line currently says, so an idle session rewrites
    // nothing.
    reported_attached: usize,
    // Clients that have asked what the screen currently shows and are
    // waiting for the answer. Only recorded here: this module has no
    // access to any session's grid, and deliberately keeps none (see
    // the module doc comment). repl.rs, which does, answers them --
    // see `answer_capture_requests`.
    capture_requests: Vec<ClientId>,
    // The main thread's own pty-master handle -- writes only (client
    // input, `Pty::set_size`); the background thread owns the read side
    // via its own separate fd (see SessionBridge::new).
    pty_master: std::fs::File,
    // Set true the moment a connection is accepted (a first attach *or*
    // a reattach after a prior client detached); consumed once by
    // take_just_attached below. repl.rs's on_idle hook uses this to
    // trigger one explicit full repaint (compositor_redraw) right when
    // a client connects -- repl::run's own compositor otherwise only
    // ever sends *incremental* diffs (see compositor_diff_tests), which
    // would leave a freshly-attached client's own blank real terminal
    // stuck showing nothing until unrelated activity happened to redraw
    // it. Not needed for the very first attach performed by `session
    // new` (that one already gets a full paint for free from
    // start_promoted's own one-time compositor_redraw at startup,
    // before any client exists to miss it) but harmless to also fire
    // there -- one extra repaint into a pty nothing was reading from
    // yet.
    just_attached: bool,
    // The most recently received Handshake's own term/colorterm,
    // consumed once by take_pending_capability below. Deliberately
    // *not* applied directly here via `std::env::set_var` -- this
    // bridge has no access to (and shouldn't reach into) any `Shell`'s
    // own state, and a raw env var write would be silently undone the
    // next time any session runs a command anyway (see exec.rs's
    // `sync_real_state_in`/`set_terminal_capability_env`'s own doc
    // comments for why). repl.rs's on_idle hook is what actually
    // applies this, to every session's own remembered environment, at
    // the same moment it notices `just_attached`.
    pending_capability: Option<(String, String)>,
}

// Bounds how many chunks get drained per tick before returning control
// -- same reasoning drive_fg_job's own MAX_READS_PER_TICK already
// documents: a firehose producer (a job printing in a tight loop) could
// otherwise keep this side of the bridge busy indefinitely, starving
// every other on_idle responsibility (job draining, WINCH, the next
// keystroke) that also needs a turn. Only bounds drain_client now --
// the pty-master-reading side runs unbounded on its own thread, where
// starving anything else isn't a concern at all.
const MAX_READS_PER_TICK: u32 = 16;

// The background thread's whole job: block-read `pty_master_read`
// forever, forwarding whatever arrives to `client_write` if anyone's
// there, discarding otherwise (see this struct's own doc comment for
// why discarding, not buffering, and why this must never stop reading
// regardless). A write failure (the client went away) is treated the
// same as "no client" -- this thread never clears `client_write`
// itself, that's the main thread's job (via drain_client's own EOF
// detection on `client_read`); this just silently drops that one
// forward attempt and keeps reading. Returns (ending the thread) only
// when the pty master itself is gone, which in practice means the
// whole daemon process is exiting.
fn drain_pty_master_thread(mut pty_master_read: std::fs::File, writes: Arc<Mutex<Vec<(ClientId, UnixStream)>>>) {
    let mut buf = [0u8; 4096];
    loop {
        match pty_master_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let encoded = Message::Bytes(buf[..n].to_vec()).encode();
                // Every attached client gets the same bytes: they are
                // watching the same screen, which is the whole point of
                // letting more than one attach. A write that fails is
                // dropped and forgotten rather than acted on -- this
                // thread has never been the one to decide a client is
                // gone (the main thread's own EOF detection is), and
                // racing it for that decision is the one way the two
                // halves could come to disagree about who is here.
                if let Ok(guard) = writes.lock() {
                    for (_, stream) in guard.iter() {
                        let _ = (&*stream).write_all(&encoded);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

impl SessionBridge {
    // `listener` is set non-blocking here (the main thread's own
    // accept/drain_client loop is never allowed to block); `pty_master`
    // is *not* -- the main thread only ever writes to it now, and the
    // background thread's own clone (opened here, moved into the
    // spawned thread) stays in ordinary blocking mode on purpose, since
    // that thread has nothing else to do while waiting anyway.
    pub fn new(pty_master: std::fs::File, listener: UnixListener) -> io::Result<SessionBridge> {
        listener.set_nonblocking(true)?;
        let pty_master_read = pty_master.try_clone()?;
        let writes = Arc::new(Mutex::new(Vec::new()));
        let writes_for_thread = writes.clone();
        std::thread::spawn(move || drain_pty_master_thread(pty_master_read, writes_for_thread));
        Ok(SessionBridge {
            listener,
            clients: Vec::new(),
            writes,
            next_client_id: 1,
            pidfile: None,
            reported_attached: 0,
            capture_requests: Vec::new(),
            pty_master,
            just_attached: false,
            pending_capability: None,
        })
    }

    pub fn is_attached(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Hands the daemon's own pidfile to the bridge, so the attached
    /// count in it can be kept current. Called once, by run_daemon.
    pub fn watch_pidfile(&mut self, mut pidfile: std::fs::File) {
        // Written straight out rather than through publish_attached,
        // which only writes on a *change*: the first status line is
        // exactly the one that has nothing to differ from, and without
        // it a session nobody has attached to yet would read as
        // "unknown" for its whole life instead of "detached".
        self.reported_attached = self.clients.len();
        write_status(&mut pidfile, self.reported_attached);
        self.pidfile = Some(pidfile);
    }

    // Writes the attached count into the pidfile, but only when it has
    // actually changed -- this runs on every idle tick.
    fn publish_attached(&mut self) {
        let attached = self.clients.len();
        if attached == self.reported_attached {
            return;
        }
        self.reported_attached = attached;
        if let Some(pidfile) = self.pidfile.as_mut() {
            write_status(pidfile, attached);
        }
    }

    pub fn take_just_attached(&mut self) -> bool {
        std::mem::replace(&mut self.just_attached, false)
    }

    pub fn take_pending_capability(&mut self) -> Option<(String, String)> {
        self.pending_capability.take()
    }

    // Called once per on_idle tick (see service_current_bridge below).
    // Never blocks -- the pty-master-draining half no longer lives
    // here at all (see drain_pty_master_thread).
    pub fn service(&mut self) {
        // Tried every tick, not only while nobody is attached: another
        // terminal joining a session someone is already using is an
        // ordinary thing to do now, not an error.
        self.try_accept();
        self.drain_clients();
        self.publish_attached();
    }

    // The pty is sized to the smallest attached client, so nothing any
    // of them can see sits off the edge of what another one has. Called
    // after every change to the set of clients and after every size any
    // one of them reports. A client that has not said how big it is yet
    // reports (0, 0) and is skipped -- it must not shrink the session
    // to nothing for everybody else while its handshake is in flight.
    fn resize_to_smallest_client(&mut self) {
        let sizes: Vec<(u16, u16)> = self.clients.iter().map(|c| c.size).collect();
        if let Some((rows, cols)) = smallest_size(&sizes) {
            let _ = crate::pty::set_size(self.pty_master.as_raw_fd(), rows, cols);
        }
    }

    fn try_accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                // Verified once, at connect time -- see peer_uid's own
                // doc comment on why this is defense in depth on top of
                // the socket directory's own 0700 permissions, not a
                // replacement for them. A mismatched UID (only reachable
                // at all under a misconfigured/shared runtime directory)
                // just drops the connection rather than accepting an
                // unauthenticated bridge target.
                match peer_uid(&stream) {
                    Ok(uid) if uid == current_uid() => self.attach(stream),
                    _ => { /* wrong UID or the check itself failed -- drop `stream` */ }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
    }

    // Adds an accepted connection to the set of attached clients: the
    // one place `clients` and `writes` both grow, which is what keeps
    // them agreeing about who is here.
    fn attach(&mut self, stream: UnixStream) {
        if self.clients.len() >= MAX_CLIENTS {
            // Dropped, so the far end sees EOF and exits, rather than
            // sitting connected to a session that will never answer it.
            return;
        }
        let Ok(write_half) = stream.try_clone() else { return };
        let _ = stream.set_nonblocking(true);
        let id = self.next_client_id;
        self.next_client_id += 1;
        match self.writes.lock() {
            Ok(mut guard) => guard.push((id, write_half)),
            Err(_) => return,
        }
        self.clients.push(Client { id, read: stream, decoder: Decoder::new(), size: (0, 0) });
        self.just_attached = true;
    }

    // Drops one client from both `clients` (this struct's own handles)
    // and `writes` (the background thread's shared ones) together --
    // the one place either shrinks, so the two never disagree about who
    // is attached. The pty resizes afterwards: with the smallest client
    // gone, everyone left can have the room it was holding them to.
    fn detach(&mut self, id: ClientId) {
        self.clients.retain(|c| c.id != id);
        self.writes.lock().unwrap_or_else(|p| p.into_inner()).retain(|(other, _)| *other != id);
        self.resize_to_smallest_client();
    }

    // Every attached client's input, in turn. Each is drained and
    // decoded on its own: one client's malformed frame or sudden
    // departure has nothing to do with the others.
    fn drain_clients(&mut self) {
        let ids: Vec<ClientId> = self.clients.iter().map(|c| c.id).collect();
        for id in ids {
            if !self.drain_one_client(id) {
                self.detach(id);
            }
        }
    }

    // Reads and acts on whatever `id` has sent. `false` means that
    // client is gone -- EOF, a read error, or a frame this protocol
    // cannot make sense of -- and should be detached.
    fn drain_one_client(&mut self, id: ClientId) -> bool {
        let Some(at) = self.clients.iter().position(|c| c.id == id) else { return true };
        let mut buf = [0u8; 4096];
        // Noted rather than returned on immediately: whatever arrived in
        // the same breath as the EOF is still in the decoder and still
        // meant something. `bish session send` is exactly that shape --
        // connect, write, close -- so returning early here would drop
        // the keys it came to deliver.
        let mut ended = false;
        for _ in 0..MAX_READS_PER_TICK {
            match self.clients[at].read.read(&mut buf) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => {
                    let bytes = buf[..n].to_vec();
                    self.clients[at].decoder.feed(&bytes);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    ended = true;
                    break;
                }
            }
        }
        loop {
            match self.clients[at].decoder.next_message() {
                Ok(Some(Message::Bytes(payload))) => {
                    // A write to the pty master's own kernel buffer --
                    // bounded by that buffer's size, same as any other
                    // pty write in this codebase (e.g. drive_fg_job
                    // forwarding a real paste/mouse sequence into a
                    // job's own pty); not a new risk category (see this
                    // struct's own doc comment for why the *other*
                    // direction was the one that could actually
                    // deadlock, and why this one doesn't the same way).
                    //
                    // Every client types into the same shell. That is
                    // what sharing a session means.
                    let _ = self.pty_master.write_all(&payload);
                }
                Ok(Some(Message::Resize { rows, cols })) => {
                    // Recorded against this client and then resolved
                    // against all of them, rather than applied straight
                    // to the pty: a second, larger terminal must not
                    // stretch the session out from under a smaller one
                    // that can then only see part of it.
                    //
                    // Resizing via the master fd is what delivers a real
                    // SIGWINCH to the slave's controlling-terminal
                    // holder (this same daemon process, via
                    // attach_self_to_pty), so the existing
                    // install_winch_handler/take_winch/
                    // poll_and_apply_resize machinery in exec.rs and
                    // repl.rs picks it up with no new code.
                    self.clients[at].size = (rows, cols);
                    self.resize_to_smallest_client();
                }
                Ok(Some(Message::Handshake { rows, cols, term, colorterm })) => {
                    self.clients[at].size = (rows, cols);
                    self.resize_to_smallest_client();
                    // Stashed for repl.rs to apply to every session's
                    // own remembered environment (Shell::
                    // set_terminal_capability_env) -- not set directly
                    // here. See this struct's own `pending_capability`
                    // field doc comment for why a raw `std::env::
                    // set_var` from inside this bridge doesn't actually
                    // stick (confirmed the hard way: it worked for
                    // exactly one moment, then exec.rs's own
                    // sync_real_state_in silently undid it the next
                    // time any session ran a command).
                    self.pending_capability = Some((term, colorterm));
                }
                Ok(Some(Message::CaptureRequest)) => {
                    // Recorded, not answered: the answer lives in a
                    // session's grid, which this module cannot reach.
                    self.capture_requests.push(id);
                }
                Ok(Some(Message::Passthrough(_) | Message::CaptureReply(_))) => {
                    // Server -> client messages (see Message's own doc
                    // comments). A client never sends these; tolerated
                    // as a no-op rather than treated as malformed, in
                    // case that ever changes.
                }
                Ok(None) => break,
                Err(_) => return false,
            }
        }
        !ended
    }

    // Whether anybody is waiting to be told what the screen shows.
    fn wants_capture(&self) -> bool {
        !self.capture_requests.is_empty()
    }

    // Sends `text` to every client waiting for a capture, and closes
    // each of them out: a capture client asks one question and leaves.
    fn answer_captures(&mut self, text: &str) {
        let encoded = Message::CaptureReply(text.as_bytes().to_vec()).encode();
        for id in std::mem::take(&mut self.capture_requests) {
            if let Ok(guard) = self.writes.lock()
                && let Some((_, stream)) = guard.iter().find(|(other, _)| *other == id)
            {
                let _ = (&*stream).write_all(&encoded);
            }
        }
    }
}

thread_local! {
    static ACTIVE_BRIDGE: RefCell<Option<SessionBridge>> = const { RefCell::new(None) };
}

// Installs `bridge` as this process's current session bridge -- called
// once by run_daemon, before repl::run starts.
pub fn install_bridge(bridge: SessionBridge) {
    ACTIVE_BRIDGE.with(|b| *b.borrow_mut() = Some(bridge));
}

// The one call service_background_jobs (repl.rs) needs -- a plain no-op
// when this process isn't running as a session daemon, which is every
// process except one that reached here via run_daemon below.
pub fn service_current_bridge() {
    ACTIVE_BRIDGE.with(|b| {
        if let Some(bridge) = b.borrow_mut().as_mut() {
            bridge.service();
        }
    });
}

/// Answers any pending `bish session capture` with the text `screen`
/// produces, and says whether there was one.
///
/// `screen` is only called when somebody is actually waiting, so the
/// caller can render as expensively as it likes here. It is a closure
/// rather than anything this module holds because what the screen
/// currently shows lives in repl.rs's own session state, which this
/// module has no access to and keeps none of -- the same shape as the
/// `on_idle` callback every blocking loop in repl.rs already passes.
pub fn answer_capture_requests(screen: impl FnOnce() -> String) -> bool {
    ACTIVE_BRIDGE.with(|b| {
        let mut guard = b.borrow_mut();
        let Some(bridge) = guard.as_mut() else { return false };
        if !bridge.wants_capture() {
            return false;
        }
        let text = screen();
        bridge.answer_captures(&text);
        true
    })
}

// True at most once per attach (a first attach or a reattach) -- lets
// repl.rs's own on_idle hook trigger one explicit full repaint right
// when a client connects. See SessionBridge::just_attached's own doc
// comment for why an incremental-diff-only compositor otherwise leaves
// a freshly (re)attached client's blank real terminal stuck empty.
pub fn take_bridge_just_attached() -> bool {
    ACTIVE_BRIDGE.with(|b| b.borrow_mut().as_mut().is_some_and(|bridge| bridge.take_just_attached()))
}

// The most recently attached client's own (term, colorterm), if any
// arrived and hasn't been consumed yet -- see SessionBridge's own
// `pending_capability` field doc comment for why applying this is
// repl.rs's job, not this bridge's.
pub fn take_pending_capability() -> Option<(String, String)> {
    ACTIVE_BRIDGE.with(|b| b.borrow_mut().as_mut().and_then(|bridge| bridge.take_pending_capability()))
}

// Ignores SIGHUP so a detached daemon survives whatever terminal/ssh
// connection launched `bish session new` going away. setsid() itself
// already happens inside pty::attach_self_to_pty (a precondition for
// TIOCSCTTY, not something worth a second, separate call here) --
// installing this too is cheap, standard defense-in-depth for any
// window where the daemon is still reachable by a HUP before that takes
// effect, the same belt-and-suspenders `nohup` itself relies on.
fn daemonize() {
    term::ignore_sighup();
}

use crate::term;

// The daemon bootstrap: binds the session's socket, gives this process
// its own local pty (see this module's own top doc comment for why),
// installs the bridge, and hands off into the ordinary, completely
// unmodified repl::run. Only returns on a genuine startup failure --
// repl::run itself runs until the interactive shell actually exits.
pub fn run_daemon(name: &str) -> io::Result<i32> {
    check_name(name)?;
    ensure_socket_dir()?;
    // Acquired first, before touching the socket at all: fails fast
    // (AddrInUse-shaped, but really "a live daemon already owns this
    // name") if another live daemon -- or a concurrently-racing `bish
    // session new` for the same name -- already holds it, rather than
    // silently stomping on a still-running session's own socket file.
    let pidfile_lock = acquire_pidfile_lock(name)?;

    let sock_path = socket_path(name);
    // Now safe to remove unconditionally: holding the pidfile's own
    // exclusive lock proves nothing live could still be bound here.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    let pty = crate::pty::open()?;
    let slave_path = pty.slave_path.clone();
    let bridge = SessionBridge::new(pty.master, listener)?;

    daemonize();
    crate::pty::attach_self_to_pty(&slave_path)?;

    // The bridge keeps the attached count in here current, which is the
    // only way `ls` -- a different process entirely -- can know it.
    // The lock lives on in the bridge's own copy for the rest of this
    // process's life, exactly as it did when run_daemon held it alone.
    let mut bridge = bridge;
    bridge.watch_pidfile(pidfile_lock);
    install_bridge(bridge);

    let shell = crate::exec::Shell::new();
    crate::repl::run(shell, true);
    Ok(0)
}

// `bish session ls`: every name in the socket directory whose pidfile
// is currently locked by a live daemon (see is_daemon_alive) -- a
// leftover socket/pidfile pair from a daemon that already exited is
// silently skipped, not listed as if it still existed.
pub fn run_ls() -> io::Result<i32> {
    let entries = match std::fs::read_dir(socket_dir()) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> =
        entries.flatten().filter_map(|entry| entry.file_name().to_str().and_then(|s| s.strip_suffix(".sock")).map(str::to_string)).collect();
    names.sort();
    let live: Vec<String> = names.into_iter().filter(|name| is_daemon_alive(name)).collect();
    if live.is_empty() {
        println!("no sessions");
        return Ok(0);
    }
    // Names are the column anyone scans down, so they line up.
    let width = live.iter().map(String::len).max().unwrap_or(0);
    for name in live {
        let attached = match read_attached(&name) {
            Some(0) => "detached".to_string(),
            Some(1) => "1 client".to_string(),
            Some(n) => format!("{n} clients"),
            // A daemon from before the status line existed, or one that
            // has not written its first one yet.
            None => "?".to_string(),
        };
        let age = session_age(&name).map(short_duration).unwrap_or_else(|| "?".to_string());
        let cwd = read_pid(&name).and_then(session_cwd).map(|p| p.display().to_string()).unwrap_or_default();
        println!("{name:width$}  {attached:<10}  {age:>4} old  {cwd}");
    }
    Ok(0)
}

/// The bytes a `bish session send` argument stands for.
///
/// An argument that names a key is that key; anything else is its own
/// literal text. tmux's `send-keys` does exactly this, and for the same
/// reason: `send-keys make Enter` reads better than any escaping scheme
/// anyone has come up with, and a literal word that happens to be
/// spelled "Enter" is rare enough to be worth the ambiguity.
fn key_bytes(arg: &str) -> Vec<u8> {
    let named: &[u8] = match arg {
        "Enter" | "C-m" => b"\r",
        "Tab" | "C-i" => b"\t",
        "Escape" | "Esc" => b"\x1b",
        "Space" => b" ",
        "BSpace" | "Backspace" => b"\x7f",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        // `C-a` through `C-z`, plus the handful above them that a
        // terminal spells the same way.
        _ => {
            if let Some(rest) = arg.strip_prefix("C-")
                && rest.chars().count() == 1
            {
                let c = rest.chars().next().expect("one character");
                if c.is_ascii_alphabetic() {
                    return vec![(c.to_ascii_lowercase() as u8) - b'a' + 1];
                }
                // C-Space is NUL, which is also bish's own detach key.
                if c == '@' {
                    return vec![0];
                }
            }
            return arg.as_bytes().to_vec();
        }
    };
    named.to_vec()
}

/// `bish session send <name> <arg>...`: types into a running session
/// from outside it, tmux's `send-keys`.
///
/// Connects, writes, and closes -- no handshake, so it never counts
/// towards the session's size and never becomes something the session
/// has to redraw for. The daemon takes what arrived with the EOF (see
/// drain_one_client), so there is nothing to wait for here.
pub fn run_send(name: &str, args: &[String]) -> io::Result<i32> {
    check_name(name)?;
    if !is_daemon_alive(name) {
        eprintln!("bish: session '{name}' is not running");
        return Ok(1);
    }
    let mut payload = Vec::new();
    for arg in args {
        payload.extend_from_slice(&key_bytes(arg));
    }
    if payload.is_empty() {
        return Ok(0);
    }
    let mut stream = UnixStream::connect(socket_path(name))?;
    stream.write_all(&Message::Bytes(payload).encode())?;
    stream.flush()?;
    Ok(0)
}

/// `bish session capture <name>`: prints what that session's focused
/// pane currently shows, tmux's `capture-pane -p`.
///
/// Asks and waits. The answer has to come from the daemon's own idle
/// tick (see answer_capture_requests), so this blocks on the reply with
/// a bound on how long it will wait -- a daemon wedged inside something
/// that never yields should end as a message, not a hang.
pub fn run_capture(name: &str) -> io::Result<i32> {
    check_name(name)?;
    if !is_daemon_alive(name) {
        eprintln!("bish: session '{name}' is not running");
        return Ok(1);
    }
    let mut stream = UnixStream::connect(socket_path(name))?;
    stream.write_all(&Message::CaptureRequest.encode())?;
    stream.flush()?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(250)))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut decoder = Decoder::new();
    let mut buf = [0u8; 4096];
    while std::time::Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => decoder.feed(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(e),
        }
        loop {
            match decoder.next_message() {
                Ok(Some(Message::CaptureReply(text))) => {
                    print!("{}", String::from_utf8_lossy(&text));
                    return Ok(0);
                }
                // Ordinary screen traffic: this connection is an
                // attached client like any other for as long as it is
                // open, so the daemon fans output to it too. Ignored --
                // this one came to ask a question.
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    eprintln!("bish: session '{name}': {e}");
                    return Ok(1);
                }
            }
        }
    }
    eprintln!("bish: session '{name}' did not answer in time");
    Ok(1)
}

/// `bish session rename <old> <new>`.
///
/// Renames the socket and the pidfile, which is all a session's name
/// is. The daemon keeps both open by descriptor -- its listener is
/// bound to the socket's inode, and its pidfile lock and status writes
/// go through the fd it already holds -- so it carries on through the
/// rename without noticing, and clients connecting afterwards find it
/// under the new name.
pub fn run_rename(from: &str, to: &str) -> io::Result<i32> {
    check_name(from)?;
    check_name(to)?;
    if from == to {
        return Ok(0);
    }
    if !is_daemon_alive(from) {
        eprintln!("bish: session '{from}' is not running");
        return Ok(1);
    }
    if is_daemon_alive(to) {
        eprintln!("bish: session '{to}' already exists");
        return Ok(1);
    }
    // A dead name can still have files sitting on it (a daemon killed
    // between the SIGTERM and the sweep, say). They are nobody's, and
    // the rename below would fail on them.
    let _ = std::fs::remove_file(socket_path(to));
    let _ = std::fs::remove_file(pidfile_path(to));
    // The socket first: it is what a client connects to, so the window
    // in which neither name works is as short as it can be.
    std::fs::rename(socket_path(from), socket_path(to))?;
    std::fs::rename(pidfile_path(from), pidfile_path(to))?;
    Ok(0)
}

pub fn run_kill(name: &str) -> io::Result<i32> {
    check_name(name)?;
    if !is_daemon_alive(name) {
        eprintln!("bish: session '{}' is not running", name);
        return Ok(1);
    }
    let pid = match read_pid(name) {
        Some(p) => p,
        None => {
            eprintln!("bish: session '{}' has no readable pidfile", name);
            return Ok(1);
        }
    };
    if unsafe { kill(pid, SIGTERM) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // A daemon killed by a signal runs no cleanup of its own, so its
    // socket and pidfile outlive it. `ls` already ignores them (it
    // tests the pidfile lock, not the file's existence), but leaving a
    // pile of dead sockets in the runtime directory is nobody's idea of
    // tidy, and a name cannot be reasoned about while its corpse is
    // still lying there.
    remove_dead_session_files(name, std::time::Duration::from_secs(2));
    Ok(0)
}

/// Removes a session's socket and pidfile once nothing is holding them.
///
/// Waits up to `grace` for the daemon to actually be gone, then checks
/// once more immediately before unlinking: a new daemon can legitimately
/// have claimed the same name in between, and its files are not ours to
/// delete.
fn remove_dead_session_files(name: &str, grace: std::time::Duration) {
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline && is_daemon_alive(name) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if is_daemon_alive(name) {
        return;
    }
    let _ = std::fs::remove_file(socket_path(name));
    let _ = std::fs::remove_file(pidfile_path(name));
}

// The client loop: connects to `name`'s socket, puts the *local* real
// terminal into raw mode, sends the attach handshake, then relays bytes
// in both directions until the connection ends or the user detaches.
// Unlike the daemon side, this has no on_idle-style other work to
// interleave, so it blocks genuinely indefinitely in PollSet::wait
// rather than polling on an interval -- there is nothing else for a
// client to usefully do while idle.
pub fn run_client(name: &str) -> io::Result<i32> {
    check_name(name)?;
    let sock_path = socket_path(name);
    let mut stream = UnixStream::connect(&sock_path).map_err(|e| io::Error::new(e.kind(), format!("bish: session '{}' not found ({})", name, e)))?;

    let (rows, cols) = crate::pty::get_size(0).map(|ws| (ws.rows, ws.cols)).unwrap_or((24, 80));
    let handshake =
        Message::Handshake { rows, cols, term: std::env::var("TERM").unwrap_or_default(), colorterm: std::env::var("COLORTERM").unwrap_or_default() };
    stream.write_all(&handshake.encode())?;
    stream.set_nonblocking(true)?;

    let _raw_guard = term::RawGuard::enable(0).ok();

    // The client's own *local* terminal can be resized at any time
    // (dragging the window, an ssh client reconnecting at a different
    // size) -- unlike the daemon side, this process has no other
    // on_idle-style periodic check to notice it between poll() calls
    // (it blocks genuinely indefinitely below, on purpose: there is
    // nothing else for a client to usefully do while idle), so SIGWINCH
    // needs to wake this loop directly via the self-pipe trick rather
    // than being polled.
    let winch_pipe = crate::poll::SelfPipe::new()?;
    crate::poll::install_sigwinch_wake(winch_pipe.write_fd());

    let mut poll = crate::poll::PollSet::new();
    poll.add(0);
    poll.add(stream.as_raw_fd());
    poll.add(winch_pipe.read_fd());

    let mut decoder = Decoder::new();
    let mut buf = [0u8; 4096];
    'relay: loop {
        let ready = poll.wait(None)?;
        for fd in ready {
            if fd == winch_pipe.read_fd() {
                winch_pipe.drain();
                if let Ok(ws) = crate::pty::get_size(0) {
                    let msg = Message::Resize { rows: ws.rows, cols: ws.cols };
                    if stream.write_all(&msg.encode()).is_err() {
                        break 'relay;
                    }
                }
            } else if fd == 0 {
                let n = unsafe { read(0, buf.as_mut_ptr(), buf.len()) };
                if n <= 0 {
                    break 'relay;
                }
                // Ctrl+Space (0x00), unaccompanied by anything else in
                // this same read -- consistent with bish's own existing
                // in-process detach binding (see Frame's own doc comment
                // in repl.rs), reused here for the same underlying
                // concept rather than inventing a second one. A real
                // multi-byte paste that happens to start with a NUL is
                // vanishingly unlikely and not specially guarded against
                // here, same tradeoff editor.rs's own single-byte
                // control-key decoding already accepts elsewhere.
                if n == 1 && buf[0] == 0x00 {
                    break 'relay;
                }
                let msg = Message::Bytes(buf[..n as usize].to_vec());
                if stream.write_all(&msg.encode()).is_err() {
                    break 'relay;
                }
            } else {
                let mut got_eof = false;
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => {
                            got_eof = true;
                            break;
                        }
                        Ok(n) => decoder.feed(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => {
                            got_eof = true;
                            break;
                        }
                    }
                }
                loop {
                    match decoder.next_message() {
                        Ok(Some(Message::Bytes(payload))) => {
                            let mut out = io::stdout();
                            let _ = out.write_all(&payload);
                            let _ = out.flush();
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => {
                            got_eof = true;
                            break;
                        }
                    }
                }
                if got_eof {
                    break 'relay;
                }
            }
        }
    }
    drop(_raw_guard);
    println!("\r\n[detached from session '{}']", name);
    Ok(0)
}

// `bish session new <name>`: spawns the daemon (a detached child
// re-exec'ing this same binary in `--daemon` mode) and, once its socket
// is up, attaches to it as the first client -- both in one command,
// matching plain `tmux`'s own UX (see the implementation plan's
// decision 1) rather than tmux's `-d`.
/// `bish session new <name>`, and `new -A <name>`.
///
/// `attach_if_running` is `-A`: attach to the session if it is already
/// there, create it if it is not. Without it a name already taken is an
/// error, which it has to be -- spawning a second daemon for a live
/// name loses the pidfile lock and dies, and the connect below then
/// lands on the *existing* session. That looked like "new" quietly
/// working, while actually attaching -- and since an attach replaces
/// whoever was there, it also threw whoever was using that session off
/// it, with no message anywhere.
pub fn run_new(name: &str, attach_if_running: bool) -> io::Result<i32> {
    check_name(name)?;
    if is_daemon_alive(name) {
        if attach_if_running {
            return run_client(name);
        }
        eprintln!("bish: session '{0}' already exists -- `bish session attach {0}`, or `new -A` for either", name);
        return Ok(1);
    }
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .args(["session", "--daemon", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let sock_path = socket_path(name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if UnixStream::connect(&sock_path).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("bish: session '{}' did not start in time", name)));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    run_client(name)
}

#[cfg(test)]
mod tests {
    // A declared length is a promise about bytes that have not arrived.
    // Believing a 4 GiB one means `feed` grows the buffer forever while
    // the peer dribbles.
    #[test]
    fn an_oversized_frame_is_refused_on_its_header() {
        let mut d = super::Decoder::new();
        let mut header = vec![super::KIND_BYTES];
        header.extend_from_slice(&(u32::MAX).to_be_bytes());
        d.feed(&header);
        let err = d.next_message().unwrap_err();
        assert!(err.contains("exceeds"), "{err}");

        // Refused on the five header bytes alone -- the payload never
        // has to arrive, which is the whole point.
        assert_eq!(header.len(), 5);

        // And the largest legal frame still decodes.
        let big = super::Message::Bytes(vec![b'x'; 4096]).encode();
        let mut d = super::Decoder::new();
        d.feed(&big);
        assert!(matches!(d.next_message().unwrap(), Some(super::Message::Bytes(b)) if b.len() == 4096));
    }

    // A session name becomes a filename in the 0700 socket directory.
    #[test]
    fn a_session_name_has_to_be_one_path_component() {
        for good in ["work", "a-b_c.2", "1"] {
            assert!(super::check_name(good).is_ok(), "{good}");
        }
        for bad in ["", ".", "..", "../../tmp/x", "a/b", "a\0b", "-r --flag"] {
            assert!(super::check_name(bad).is_err(), "{bad:?} must not become a path");
        }
    }

    use super::*;
    use std::os::unix::net::UnixStream;

    // `XDG_RUNTIME_DIR` is process-wide mutable state, and `cargo test`
    // runs tests in parallel by default -- every test below that reads
    // `socket_dir()` (directly, or via any function built on it) needs
    // to run with *this* env var pinned to a value only it controls for
    // the duration, or two such tests racing each other will
    // occasionally observe (and act on) each other's value. Confirmed
    // this was a real, live flake, not just a hypothetical one, while
    // adding the pidfile-lock tests below (a fresh `cargo test session::`
    // run failed exactly this way on the first try). A single shared
    // mutex, held for the whole body, is the standard fix -- serializes
    // only these tests relative to each other, the rest of the suite
    // stays fully parallel.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Runs `body` with `XDG_RUNTIME_DIR` set to `dir` (a fresh, isolated
    // temp path, unique per call via `tag`) for its duration, holding
    // `ENV_LOCK` throughout and restoring the env var afterward
    // regardless of how `body` returns. `.unwrap_or_else(|p| p.into_inner())`
    // recovers from a poisoned lock (an earlier panicking test having
    // held it) rather than cascading that failure into every later test
    // that also needs this same lock.
    fn with_isolated_runtime_dir(tag: &str, body: impl FnOnce(&std::path::Path)) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("bish-session-test-{}-{}", std::process::id(), tag));
        let saved = std::env::var_os("XDG_RUNTIME_DIR");
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        body(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        match saved {
            Some(v) => unsafe { std::env::set_var("XDG_RUNTIME_DIR", v) },
            None => unsafe { std::env::remove_var("XDG_RUNTIME_DIR") },
        }
    }

    #[test]
    fn socket_dir_prefers_xdg_runtime_dir_when_set() {
        with_isolated_runtime_dir("prefers-xdg", |dir| {
            assert_eq!(socket_dir(), dir.join("bish"));
        });
    }

    #[test]
    fn socket_dir_falls_back_to_tmp_when_xdg_runtime_dir_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
        with_isolated_runtime_dir("named-consistently", |dir| {
            assert_eq!(socket_path("work"), dir.join("bish/work.sock"));
            assert_eq!(pidfile_path("work"), dir.join("bish/work.pid"));
        });
    }

    #[test]
    fn ensure_socket_dir_creates_it_mode_0700() {
        use std::os::unix::fs::PermissionsExt;
        with_isolated_runtime_dir("mode-0700", |_dir| {
            let dir = ensure_socket_dir().expect("ensure_socket_dir");
            let mode = std::fs::metadata(&dir).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "expected the session socket dir to be mode 0700, got {:o}", mode);
        });
    }

    #[test]
    fn ensure_socket_dir_is_idempotent() {
        with_isolated_runtime_dir("idempotent", |_dir| {
            ensure_socket_dir().expect("first call");
            let result = ensure_socket_dir();
            assert!(result.is_ok(), "calling ensure_socket_dir a second time should succeed, got {:?}", result);
        });
    }

    #[test]
    fn peer_uid_reports_this_processs_own_real_uid_over_a_socketpair() {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        let uid = peer_uid(&a).expect("peer_uid");
        assert_eq!(uid, current_uid(), "a socketpair's peer is this same process");
    }

    #[test]
    fn acquire_pidfile_lock_writes_this_processs_own_pid() {
        with_isolated_runtime_dir("acquire", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            let _lock = acquire_pidfile_lock("work").expect("acquire_pidfile_lock");
            assert_eq!(read_pid("work"), Some(std::process::id() as i32));
        });
    }

    #[test]
    fn a_key_name_is_that_key_and_anything_else_is_its_own_text() {
        assert_eq!(key_bytes("Enter"), b"\r");
        assert_eq!(key_bytes("Tab"), b"\t");
        assert_eq!(key_bytes("Escape"), b"\x1b");
        assert_eq!(key_bytes("Up"), b"\x1b[A");
        // A word that is not a key name is the word.
        assert_eq!(key_bytes("make"), b"make");
        assert_eq!(key_bytes("echo hello"), b"echo hello");
        // Including one that only looks like a key name.
        assert_eq!(key_bytes("C-"), b"C-");
        assert_eq!(key_bytes("C-hello"), b"C-hello");
    }

    #[test]
    fn control_keys_are_the_control_codes_they_stand_for() {
        assert_eq!(key_bytes("C-c"), vec![3]);
        assert_eq!(key_bytes("C-d"), vec![4]);
        assert_eq!(key_bytes("C-a"), vec![1]);
        assert_eq!(key_bytes("C-z"), vec![26]);
        // Case makes no difference to which code it is.
        assert_eq!(key_bytes("C-C"), vec![3]);
        // C-Space, which is also bish's own detach key.
        assert_eq!(key_bytes("C-@"), vec![0]);
        // The two spellings a terminal genuinely shares.
        assert_eq!(key_bytes("C-m"), key_bytes("Enter"));
        assert_eq!(key_bytes("C-i"), key_bytes("Tab"));
    }

    #[test]
    fn the_capture_messages_round_trip() {
        let request = Message::CaptureRequest;
        let mut dec = Decoder::new();
        dec.feed(&request.encode());
        assert_eq!(dec.next_message().unwrap(), Some(request));

        let reply = Message::CaptureReply(b"line one\nline two\n".to_vec());
        let mut dec = Decoder::new();
        dec.feed(&reply.encode());
        assert_eq!(dec.next_message().unwrap(), Some(reply));
    }

    #[test]
    fn a_duration_reads_as_the_largest_unit_that_is_not_zero() {
        use std::time::Duration;
        assert_eq!(short_duration(Duration::from_secs(0)), "0s");
        assert_eq!(short_duration(Duration::from_secs(59)), "59s");
        assert_eq!(short_duration(Duration::from_secs(60)), "1m");
        assert_eq!(short_duration(Duration::from_secs(3599)), "59m");
        assert_eq!(short_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(short_duration(Duration::from_secs(86399)), "23h");
        assert_eq!(short_duration(Duration::from_secs(86400)), "1d");
        // Nothing after the first unit: "3h" belongs in a list,
        // "3h07m12s" does not.
        assert_eq!(short_duration(Duration::from_secs(3 * 3600 + 432)), "3h");
    }

    #[test]
    fn the_status_line_sits_under_the_pid_and_does_not_disturb_it() {
        with_isolated_runtime_dir("status-line", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(pidfile_path("s")).expect("pidfile");

            write_status(&mut file, 2);
            assert_eq!(read_pid("s"), Some(std::process::id() as i32), "the pid is still the first line");
            assert_eq!(read_attached("s"), Some(2));

            // Rewritten in place, shorter -- the old line must not show
            // through underneath the new one.
            write_status(&mut file, 0);
            assert_eq!(read_attached("s"), Some(0));
            assert_eq!(read_pid("s"), Some(std::process::id() as i32));
        });
    }

    #[test]
    fn a_pidfile_with_no_status_line_reads_as_unknown_not_as_nobody() {
        // What a daemon from before the status line existed leaves.
        // "?" in the listing is honest; "detached" would not be.
        with_isolated_runtime_dir("old-pidfile", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            std::fs::write(pidfile_path("old"), b"4242\n").expect("pidfile");
            assert_eq!(read_pid("old"), Some(4242));
            assert_eq!(read_attached("old"), None);
        });
    }

    #[test]
    fn a_shared_session_runs_at_the_smallest_attached_size() {
        // Each direction independently, so a tall narrow terminal beside
        // a short wide one leaves both able to see the whole screen.
        assert_eq!(smallest_size(&[(40, 100), (24, 80)]), Some((24, 80)));
        assert_eq!(smallest_size(&[(24, 200), (60, 80)]), Some((24, 80)));
        assert_eq!(smallest_size(&[(30, 90)]), Some((30, 90)));
    }

    #[test]
    fn a_client_that_has_not_said_its_size_yet_does_not_shrink_the_session() {
        // (0, 0) is what a just-accepted client reports until its
        // Handshake arrives. Taking it literally would collapse the pty
        // to nothing for everyone already attached.
        assert_eq!(smallest_size(&[(40, 100), (0, 0)]), Some((40, 100)));
        assert_eq!(smallest_size(&[(0, 0)]), None, "and with nobody sized yet, leave the pty alone");
        assert_eq!(smallest_size(&[]), None, "same when nobody is attached at all");
    }

    #[test]
    fn dead_session_files_are_removed_once_nothing_holds_them() {
        with_isolated_runtime_dir("dead-files", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            // What a daemon killed by a signal leaves behind: it runs no
            // cleanup of its own, so both files outlive it.
            std::fs::write(socket_path("gone"), b"").expect("socket stand-in");
            std::fs::write(pidfile_path("gone"), b"12345\n").expect("pidfile stand-in");

            remove_dead_session_files("gone", std::time::Duration::from_millis(0));

            assert!(!socket_path("gone").exists(), "the socket should be gone");
            assert!(!pidfile_path("gone").exists(), "and so should the pidfile");
        });
    }

    #[test]
    fn a_live_sessions_files_are_left_alone() {
        with_isolated_runtime_dir("live-files", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            std::fs::write(socket_path("live"), b"").expect("socket stand-in");
            // Holding the lock is exactly what "a daemon is alive here"
            // means (see acquire_pidfile_lock), so this stands in for
            // one -- including the case that matters most: a *new*
            // daemon claiming the name between the kill and the sweep.
            let _held = acquire_pidfile_lock("live").expect("acquire");

            remove_dead_session_files("live", std::time::Duration::from_millis(0));

            assert!(socket_path("live").exists(), "a live session's socket is not ours to delete");
            assert!(pidfile_path("live").exists());
        });
    }

    #[test]
    fn a_second_acquire_fails_while_the_first_lock_is_still_held() {
        with_isolated_runtime_dir("double-acquire", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            let _first = acquire_pidfile_lock("work").expect("first acquire");
            // flock is associated with the open file *description*, not
            // the process -- a second, independent open+lock attempt
            // against the same path genuinely conflicts even though
            // it's the same process making both calls, which is exactly
            // what lets this simulate "a second `bish session new work`
            // while the first is still running" faithfully.
            let second = acquire_pidfile_lock("work");
            assert!(second.is_err(), "a second lock attempt should fail while the first is still held");
        });
    }

    #[test]
    fn is_daemon_alive_tracks_whether_the_lock_is_currently_held() {
        with_isolated_runtime_dir("liveness", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            assert!(!is_daemon_alive("work"), "no pidfile yet -- not alive");
            let lock = acquire_pidfile_lock("work").expect("acquire");
            assert!(is_daemon_alive("work"), "lock held -- alive");
            drop(lock);
            assert!(!is_daemon_alive("work"), "lock released -- no longer alive, even though the file itself still exists");
        });
    }

    #[test]
    fn is_daemon_alive_is_false_for_a_name_that_was_never_created() {
        with_isolated_runtime_dir("never-created", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            assert!(!is_daemon_alive("nonexistent"));
        });
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
