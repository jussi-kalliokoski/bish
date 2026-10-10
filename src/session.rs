// Detachable `bish session` support: where a session's socket and
// pidfile live, the framed protocol over that socket, checking a peer's
// UID before trusting it, the daemon (`SessionBridge`, `run_daemon`) and
// the client (`run_client`).
//
// The daemon needs nothing from `repl::run`: `run_daemon` gives itself
// a pty pair (`pty::attach_self_to_pty`) with fd 0/1/2 on the slave, so
// every termios, ioctl, read and write works unchanged; as far as the
// repl knows, it has a real terminal. `SessionBridge` relays bytes
// between that pty's master and the attached client, if any. It touches
// no shell state, so it can be serviced from the idle hook every
// blocking loop already calls (`service_background_jobs`, via
// `service_current_bridge`) through a global, without new parameters;
// the process is single-threaded, as with exec.rs's `WINCH_FLAG`.
//
// `is_attached`, `has_rpc_peers` and `Peer::fd` are used only by tests.
#![allow(dead_code)]

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
#[cfg(not(unix))]
use wasm_socket::{UnixListener, UnixStream};

// Without UNIX-domain sockets sessions can't work: these stand in for
// `std::os::unix::net`'s two types so the module compiles, with every
// operation returning "unsupported". The browser build is the one such
// target, and it has no second process to detach from.
#[cfg(not(unix))]
mod wasm_socket {
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::path::Path;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "detachable sessions are not available on this platform"))
    }

    pub struct UnixStream;

    impl UnixStream {
        pub fn connect<P: AsRef<Path>>(_path: P) -> io::Result<UnixStream> {
            unsupported()
        }

        pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
            unsupported()
        }

        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            unsupported()
        }

        pub fn try_clone(&self) -> io::Result<UnixStream> {
            unsupported()
        }

        pub fn set_read_timeout(&self, _timeout: Option<std::time::Duration>) -> io::Result<()> {
            unsupported()
        }
    }

    impl AsRawFd for UnixStream {
        fn as_raw_fd(&self) -> RawFd {
            -1
        }
    }

    impl io::Read for UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            unsupported()
        }
    }

    impl io::Write for UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            unsupported()
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // The protocol also writes through a shared reference (`(&*stream)`),
    // so the borrowed form needs both traits.
    impl io::Read for &UnixStream {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            unsupported()
        }
    }

    impl io::Write for &UnixStream {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            unsupported()
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct UnixListener;

    impl UnixListener {
        pub fn bind<P: AsRef<Path>>(_path: P) -> io::Result<UnixListener> {
            unsupported()
        }

        /// As `UnixListener::accept`; the second element, the peer address,
        /// is ignored.
        pub fn accept(&self) -> io::Result<(UnixStream, ())> {
            unsupported()
        }

        pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
            unsupported()
        }
    }

    impl AsRawFd for UnixListener {
        fn as_raw_fd(&self) -> RawFd {
            -1
        }
    }
}

// SIGTERM is 15 on every Unix, unlike the job-control signals, so it's
// here rather than in the platform tables.
const SIGTERM: i32 = 15;

// The connecting peer's real UID, checked once per connection before
// trusting a byte. On top of the socket directory's `0700` (see
// `socket_dir`): if the directory were shared or misconfigured, this
// still refuses another user.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    crate::platform::peer_user(stream.as_raw_fd())
}

fn current_uid() -> u32 {
    crate::platform::real_user()
}

// Where sessions' sockets and pidfiles live: `$XDG_RUNTIME_DIR/bish`
// (per-user, cleared on logout), else `/tmp/bish-<uid>`.
// `ensure_socket_dir` creates it `0700`; this only computes the path.
pub fn socket_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("bish");
    }
    PathBuf::from(format!("/tmp/bish-{}", current_uid()))
}

// A session name becomes a filename in the socket directory, so it must
// be one path component: `bish session new ../../tmp/x` would write
// outside it. Not a privilege boundary while names come from the user's
// own argv, but it would be once one could come from a config, a hook
// or `$BISH_SESSION`.
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

// Opens (creating) `name`'s pidfile, writes this PID and takes an
// exclusive non-blocking `flock`. Keep the file alive for the process's
// life: dropping it releases the lock, which is how `is_daemon_alive`
// tells a live daemon from a stale file. The PID alone can't, as it
// may be reused after a crash. Also stops two `bish session new` for the
// same name: the second's `flock` fails before it touches the socket.
fn acquire_pidfile_lock(name: &str) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(pidfile_path(name))?;
    crate::platform::lock_exclusive(file.as_raw_fd())?;
    use std::io::Write as _;
    let mut f = &file;
    write!(f, "{}", crate::platform::process_id())?;
    Ok(file)
}

// Whether `name`'s pidfile is locked by a live daemon, probed with a
// non-blocking lock released straight away (see
// `acquire_pidfile_lock`). A missing pidfile is "not alive".
fn is_daemon_alive(name: &str) -> bool {
    let file = match std::fs::File::open(pidfile_path(name)) {
        Ok(f) => f,
        Err(_) => return false,
    };
    if crate::platform::lock_exclusive(file.as_raw_fd()).is_ok() {
        // Nobody held it: stale. Release the probe lock.
        crate::platform::unlock(file.as_raw_fd());
        false
    } else {
        true
    }
}

fn read_pid(name: &str) -> Option<i32> {
    // First line only: the daemon writes a status line under it (see
    // `write_status`).
    std::fs::read_to_string(pidfile_path(name)).ok()?.lines().next()?.trim().parse().ok()
}

/// How many clients are attached to `name`, as its daemon last wrote.
/// Only the daemon knows, and `ls` runs in another process. Rewritten
/// only on change. `None` (not yet written) reads as unknown, not
/// nobody.
fn read_attached(name: &str) -> Option<usize> {
    let text = std::fs::read_to_string(pidfile_path(name)).ok()?;
    text.lines().nth(1)?.strip_prefix("attached ")?.trim().parse().ok()
}

/// Rewrites `pidfile` as the pid line plus the attached count, through
/// the open descriptor, so `session rename` can rename the file under a
/// running daemon.
fn write_status(pidfile: &mut std::fs::File, attached: usize) {
    use std::io::{Seek, SeekFrom};
    let text = format!("{}\nattached {}\n", crate::platform::process_id(), attached);
    let _ = pidfile.seek(SeekFrom::Start(0));
    let _ = pidfile.set_len(0);
    let _ = pidfile.write_all(text.as_bytes());
    let _ = pidfile.flush();
}

/// When the session was created, from the socket's mtime: it's written
/// once at bind, unlike the pidfile.
fn session_age(name: &str) -> Option<std::time::Duration> {
    let created = std::fs::metadata(socket_path(name)).ok()?.modified().ok()?;
    std::time::SystemTime::now().duration_since(created).ok()
}

/// A duration as a person says it: the largest nonzero unit only. "3h"
/// reads better in a list than "3h07m12s".
fn short_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// The session's directory, from `/proc`: the daemon is the shell, so a
/// `cd` moves its cwd. `None` without `/proc` (macOS) or if unreadable.
fn session_cwd(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

// Creates `socket_dir()` and any missing parents. The final directory's
// `0700` is set explicitly rather than left to the umask: it holds a
// socket only this user should reach.
pub fn ensure_socket_dir() -> io::Result<PathBuf> {
    let dir = socket_dir();
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(err) = crate::platform::create_dir_with_mode(&dir, 0o700) {
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
        // EEXIST doesn't mean the directory is ours: on the `/tmp` fallback
        // anyone can pre-create `/tmp/bish-<uid>`, even as a symlink to
        // their own directory, and `set_permissions` below follows
        // symlinks. `symlink_metadata` doesn't, so this checks the entry
        // itself is a real directory owned by us.
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} exists and is not a directory", dir.display())));
        }
        let owner = crate::platform::file_owner(&meta);
        if owner != current_uid() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is owned by uid {}, not by you", dir.display(), owner)));
        }
    }
    // Force the mode even if it existed looser (an old `/tmp` fallback, say).
    crate::platform::set_file_mode(&dir, 0o700)?;
    Ok(dir)
}

// The framed protocol over a session socket: a type byte, a big-endian
// u32 length, the payload. Raw terminal bytes both ways (keystrokes in,
// rendered frames out), the handshake a client sends on connecting, a
// resize when the client's terminal resizes, and a passthrough for an
// escape sequence the client's real terminal must see (OSC 52 clipboard
// writes; see `registers::write_osc52`), carried uninterpreted.
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
    /// `bish session capture <name>`: what the focused pane shows. Client to
    /// server.
    CaptureRequest,
    /// The answer, plain text with a newline between rows. Server to client;
    /// the one message a client waits for and then exits on.
    CaptureReply(Vec<u8>),
    /// One JSON-RPC message from something asking about this session from
    /// outside (`bish tool mcp-server`). Client to server. Opaque here, like
    /// `Passthrough`: this layer only carries bytes.
    Rpc(Vec<u8>),
    /// The answer, and unsolicited notifications too. Server to client. One
    /// kind for both: a notification has no `id`, which already says so.
    RpcReply(Vec<u8>),
}

const KIND_BYTES: u8 = 0;
const KIND_HANDSHAKE: u8 = 1;
const KIND_RESIZE: u8 = 2;
const KIND_PASSTHROUGH: u8 = 3;
const KIND_CAPTURE_REQUEST: u8 = 4;
const KIND_CAPTURE_REPLY: u8 = 5;
const KIND_RPC: u8 = 6;
const KIND_RPC_REPLY: u8 = 7;

impl Message {
    // A 5-byte header (kind, big-endian length), then that many payload
    // bytes.
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
            Message::Rpc(b) => (KIND_RPC, b.clone()),
            Message::RpcReply(b) => (KIND_RPC_REPLY, b.clone()),
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

// A `String` in a Handshake: one length byte, then that many UTF-8 bytes.
// `$TERM`/`$COLORTERM` are a handful of bytes.
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

// The largest frame accepted. `try_accept` refuses other users, but the
// same user isn't the same trust, and a socket outlives its creator:
// without a ceiling a peer could declare 4 GiB, dribble, and grow the
// buffer forever. A full repaint is tens of kilobytes. As framing.rs's
// `MAX_CONTENT_LENGTH`.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Default)]
// Collects bytes from a stream socket, which may split a message across
// reads (a paste, a big frame), and yields whole messages. `feed` only
// appends; `next_message` parses.
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

    // `Ok(None)`: not enough bytes for a whole message yet; feed more. `Err`:
    // a malformed stream (unknown kind, a handshake shorter than its own
    // lengths), meaning the peer isn't speaking this protocol; close it.
    pub fn next_message(&mut self) -> Result<Option<Message>, String> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let kind = self.buf[0];
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        // Before the not-enough-bytes return, so an oversized declaration is
        // refused on its first five bytes.
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
            KIND_RPC => Message::Rpc(payload),
            KIND_RPC_REPLY => Message::RpcReply(payload),
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

// The size a session shared by these clients runs at: the smallest in
// each direction independently, so a tall narrow terminal and a short
// wide one both see the whole screen. A client yet to send its
// handshake reports (0, 0) and is skipped. `None`, with nobody sized,
// leaves the pty as it is.
fn smallest_size(sizes: &[(u16, u16)]) -> Option<(u16, u16)> {
    let rows = sizes.iter().map(|(r, _)| *r).filter(|r| *r > 0).min()?;
    let cols = sizes.iter().map(|(_, c)| *c).filter(|c| *c > 0).min()?;
    Some((rows, cols))
}

// Names an attached client while attached, never reused, so the drain
// thread's write half and the main thread's read half match up; pairing
// by position would break when a middle client left.
type ClientId = u64;

// One attached client, from the main thread's side.
struct Client {
    id: ClientId,
    // Read by `drain_one_client`; the write half is in `writes`, for the
    // drain thread.
    read: UnixStream,
    // Per client: one shared decoder would splice half of one client's frame
    // onto another's.
    decoder: Decoder,
    // The last size this client reported (handshake, then each resize).
    // The pty gets the smallest, so a session shared by two different
    // terminals fits both.
    size: (u16, u16),
}

// How many clients a session holds at once; a cap so a runaway attach
// loop can't exhaust the daemon's descriptors.
const MAX_CLIENTS: usize = 16;

// The relay between a daemon's pty master and its attached clients.
//
// Reading the master runs on its own thread (`drain_pty_master_thread`).
// The idle hook runs only while a loop waits for a key; a burst of input
// (a paste, a client relaying a whole read) is consumed and redrawn with
// no idle call between. If that output overfills the pty's queue, the
// shell's next write blocks, and with nothing draining the master it
// blocks forever (`e <long path>` hung the daemon). A thread that only
// reads the master and forwards keeps the queue moving. It touches raw
// fds only, no shell state.
//
// Client input (`drain_client`) stays on the main thread: it's small and
// consumed at once by the shell, and that keeps attach state a plain
// `Option`/`bool` with no locking.
pub struct SessionBridge {
    listener: UnixListener,
    // The main thread's handle for each attached client.
    clients: Vec<Client>,
    // Shared with the pty-draining thread, the only thing it touches: each
    // client's write half, tagged with its id. Changed only by attach and
    // detach, in step with `clients`.
    //
    // The flag is whether the client is watching the screen: true until it
    // asks a question. An RPC peer reads replies and notifications, and has
    // no use for every byte a busy session draws.
    writes: Arc<Mutex<Vec<(ClientId, UnixStream, bool)>>>,
    next_client_id: ClientId,
    // The daemon's pidfile, held for its lock and written for the status
    // line (see `write_status`). `None` outside a daemon.
    pidfile: Option<std::fs::File>,
    // What the status line says, so an idle session rewrites nothing.
    reported_attached: usize,
    // Clients waiting for what the screen shows. Only recorded here: this
    // module has no session grid; repl.rs answers
    // (`answer_capture_requests`).
    capture_requests: Vec<ClientId>,
    // JSON-RPC messages not answered yet, with who asked, recorded for the
    // same reason (see `answer_rpc_requests`).
    pending_rpc: Vec<(ClientId, Vec<u8>)>,
    // Every client that has sent one. A peer asking questions also wants to
    // hear of changes, so this is the subscriber list for
    // `push_rpc_notification`.
    rpc_clients: Vec<ClientId>,
    // The main thread's pty master, for writes (client input,
    // `Pty::set_size`); the drain thread reads its own fd. `None` for a plain
    // bish's bridge (`listen_here`): no terminal, and its peers only ask.
    pty_master: Option<std::fs::File>,
    // Set when a connection is accepted; taken by `take_just_attached`.
    // repl.rs then repaints fully once, as the compositor otherwise only
    // sends changes and a new client's blank terminal would show nothing.
    just_attached: bool,
    // The last handshake's TERM and COLORTERM, taken by
    // `take_pending_capability`. Not `set_var`'d here: the bridge doesn't
    // touch shell state, and the next command would undo it (see
    // `set_terminal_capability_env`). repl.rs applies it to every session
    // when it sees `just_attached`.
    pending_capability: Option<(String, String)>,
}

// Chunks drained per tick before returning, as in `drive_fg_job`: a
// client firehosing input would otherwise starve job draining, resizes
// and keys. Bounds `drain_client` only; the pty side has its own thread.
const MAX_READS_PER_TICK: u32 = 16;

// The drain thread: block-reads the pty master forever, forwarding what
// arrives to attached clients and discarding it when there are none,
// so the pty never fills. A failed write is ignored; the main thread
// decides when a client is gone. Ends when the pty master does, as the
// daemon exits.
fn drain_pty_master_thread(mut pty_master_read: std::fs::File, writes: Arc<Mutex<Vec<(ClientId, UnixStream, bool)>>>) {
    let mut buf = [0u8; 4096];
    loop {
        match pty_master_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let encoded = Message::Bytes(buf[..n].to_vec()).encode();
                // Every client gets the same bytes: they watch the same
                // screen. A failed write is dropped; deciding a client left
                // is the main thread's job, and racing it would let the two
                // disagree.
                if let Ok(guard) = writes.lock() {
                    for (_, stream, _) in guard.iter().filter(|(_, _, watching)| *watching) {
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
    // `listener` is made non-blocking, as the main thread must never block;
    // the drain thread's clone of `pty_master` stays blocking, as that
    // thread has nothing else to do.
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
            pending_rpc: Vec::new(),
            rpc_clients: Vec::new(),
            pty_master: Some(pty_master),
            just_attached: false,
            pending_capability: None,
        })
    }

    /// A bridge with no terminal: what a plain interactive bish listens
    /// with, so programs in its panes can ask what they'd ask a named
    /// session. Questions and captures are answered; keys, sizes and
    /// terminal capabilities are ignored.
    pub fn without_terminal(listener: UnixListener) -> io::Result<SessionBridge> {
        listener.set_nonblocking(true)?;
        Ok(SessionBridge {
            listener,
            clients: Vec::new(),
            writes: Arc::new(Mutex::new(Vec::new())),
            next_client_id: 1,
            pidfile: None,
            reported_attached: 0,
            capture_requests: Vec::new(),
            pending_rpc: Vec::new(),
            rpc_clients: Vec::new(),
            pty_master: None,
            just_attached: false,
            pending_capability: None,
        })
    }

    pub fn is_attached(&self) -> bool {
        !self.clients.is_empty()
    }

    /// Hands the daemon's pidfile to the bridge to keep the attached count
    /// current. Called once, by `run_daemon`.
    pub fn watch_pidfile(&mut self, mut pidfile: std::fs::File) {
        // Written directly, not through `publish_attached`, which writes only
        // on change; without it an unattached session would read "unknown"
        // rather than "detached".
        self.reported_attached = self.clients.len();
        write_status(&mut pidfile, self.reported_attached);
        self.pidfile = Some(pidfile);
    }

    // Writes the attached count into the pidfile only when it changed; this
    // runs every tick.
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

    // Called each idle tick (`service_current_bridge`). Never blocks.
    pub fn service(&mut self) {
        // Every tick, not only with nobody attached: joining a session
        // someone is using is ordinary.
        self.try_accept();
        self.drain_clients();
        self.publish_attached();
    }

    // Sizes the pty to the smallest attached client, so nothing one sees is
    // past another's edge. Called after the set of clients or any size
    // changes. A client yet to handshake reports (0, 0) and is skipped.
    fn resize_to_smallest_client(&mut self) {
        let sizes: Vec<(u16, u16)> = self.clients.iter().map(|c| c.size).collect();
        if let (Some((rows, cols)), Some(master)) = (smallest_size(&sizes), self.pty_master.as_ref()) {
            let _ = crate::pty::set_size(master.as_raw_fd(), rows, cols);
        }
    }

    fn try_accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                // Checked once on connect (see `peer_uid`). A mismatched
                // UID, possible only with a shared or misconfigured runtime
                // directory, drops the connection.
                match peer_uid(&stream) {
                    Ok(uid) if uid == current_uid() => self.attach(stream),
                    _ => { /* wrong UID or the check itself failed -- drop `stream` */ }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
    }

    // Adds an accepted connection: the one place `clients` and `writes`
    // grow, keeping them in agreement.
    fn attach(&mut self, stream: UnixStream) {
        if self.clients.len() >= MAX_CLIENTS {
            // Dropped, so the far end sees EOF and exits rather than
            // waiting on a session that won't answer.
            return;
        }
        let Ok(write_half) = stream.try_clone() else { return };
        let _ = stream.set_nonblocking(true);
        let id = self.next_client_id;
        self.next_client_id += 1;
        match self.writes.lock() {
            Ok(mut guard) => guard.push((id, write_half, true)),
            Err(_) => return,
        }
        self.clients.push(Client { id, read: stream, decoder: Decoder::new(), size: (0, 0) });
        // A repaint is for a terminal that just started watching; a plain
        // bish's peers watch nothing, and repainting for each agent would
        // flicker the screen.
        self.just_attached = self.pty_master.is_some();
    }

    // Drops one client from `clients` and `writes` together, the one place
    // either shrinks. The pty then resizes: with the smallest client gone,
    // the rest may have more room.
    fn detach(&mut self, id: ClientId) {
        self.rpc_clients.retain(|other| *other != id);
        self.pending_rpc.retain(|(other, _)| *other != id);
        self.clients.retain(|c| c.id != id);
        self.writes.lock().unwrap_or_else(|p| p.into_inner()).retain(|(other, _, _)| *other != id);
        self.resize_to_smallest_client();
    }

    // Every client's input in turn, each drained and decoded alone: one's
    // malformed frame or departure doesn't affect the others.
    fn drain_clients(&mut self) {
        let ids: Vec<ClientId> = self.clients.iter().map(|c| c.id).collect();
        for id in ids {
            if !self.drain_one_client(id) {
                self.detach(id);
            }
        }
    }

    // Reads and acts on what `id` sent. `false`: the client is gone (EOF, a
    // read error, an unparseable frame) and should be detached.
    fn drain_one_client(&mut self, id: ClientId) -> bool {
        let Some(at) = self.clients.iter().position(|c| c.id == id) else { return true };
        let mut buf = [0u8; 4096];
        // Noted, not returned on at once: what arrived with the EOF is
        // still in the decoder. `bish session send` connects, writes and
        // closes, and returning early would drop its keys.
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
                    // Into the pty master's buffer, bounded like any pty
                    // write here (see `SessionBridge` for why the other
                    // direction was the risky one). Every client types into
                    // the same shell: that's what sharing means.
                    if let Some(master) = self.pty_master.as_mut() {
                        let _ = master.write_all(&payload);
                    }
                }
                Ok(Some(Message::Resize { rows, cols })) => {
                    // Recorded per client and resolved over all, so a
                    // larger terminal doesn't stretch the session past a
                    // smaller one. Resizing the master sends SIGWINCH to
                    // this daemon (the slave's controlling process), and
                    // the existing resize handling picks it up.
                    self.clients[at].size = (rows, cols);
                    self.resize_to_smallest_client();
                }
                Ok(Some(Message::Handshake { rows, cols, term, colorterm })) => {
                    self.clients[at].size = (rows, cols);
                    self.resize_to_smallest_client();
                    // Stashed for repl.rs to apply to every session's
                    // remembered environment
                    // (`Shell::set_terminal_capability_env`); see
                    // `pending_capability`.
                    if self.pty_master.is_some() {
                        self.pending_capability = Some((term, colorterm));
                    }
                }
                Ok(Some(Message::CaptureRequest)) => {
                    // Recorded, not answered: the answer is in a session's
                    // grid, out of reach here.
                    self.capture_requests.push(id);
                }
                Ok(Some(Message::Rpc(payload))) => {
                    // Recorded, like a capture, and answered by
                    // `answer_rpc_requests`.
                    if !self.rpc_clients.contains(&id) {
                        self.rpc_clients.push(id);
                        // Asking, not watching: from now on it gets
                        // answers, not the screen.
                        if let Ok(mut guard) = self.writes.lock() {
                            for entry in guard.iter_mut().filter(|(other, _, _)| *other == id) {
                                entry.2 = false;
                            }
                        }
                    }
                    self.pending_rpc.push((id, payload));
                }
                Ok(Some(Message::Passthrough(_) | Message::CaptureReply(_) | Message::RpcReply(_))) => {
                    // Server-to-client messages a client never sends;
                    // ignored rather than treated as malformed.
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

    // Whether any JSON-RPC message is waiting to be answered.
    fn wants_rpc(&self) -> bool {
        !self.pending_rpc.is_empty()
    }

    // An unsolicited message to every RPC peer. Nothing is queued for
    // absent peers: a notification is about a moment, and a later client
    // wants the state it finds.
    fn notify_rpc(&mut self, payload: &[u8]) {
        let encoded = Message::RpcReply(payload.to_vec()).encode();
        for id in self.rpc_clients.clone() {
            self.send_to(id, &encoded);
        }
    }

    // One encoded message to one client, if it's still there.
    fn send_to(&mut self, id: ClientId, encoded: &[u8]) {
        if let Ok(guard) = self.writes.lock()
            && let Some((_, stream, _)) = guard.iter().find(|(other, _, _)| *other == id)
        {
            let _ = (&*stream).write_all(encoded);
        }
    }

    // Sends `text` to every client waiting for a capture and closes each:
    // a capture client asks once and leaves.
    fn answer_captures(&mut self, text: &str) {
        let encoded = Message::CaptureReply(text.as_bytes().to_vec()).encode();
        for id in std::mem::take(&mut self.capture_requests) {
            if let Ok(guard) = self.writes.lock()
                && let Some((_, stream, _)) = guard.iter().find(|(other, _, _)| *other == id)
            {
                let _ = (&*stream).write_all(&encoded);
            }
        }
    }
}

thread_local! {
    static ACTIVE_BRIDGE: RefCell<Option<SessionBridge>> = const { RefCell::new(None) };
}

// Installs this process's session bridge; `run_daemon` calls it before
// `repl::run`.
pub fn install_bridge(bridge: SessionBridge) {
    ACTIVE_BRIDGE.with(|b| *b.borrow_mut() = Some(bridge));
}

// What `service_background_jobs` calls; a no-op unless this process is a
// session daemon.
pub fn service_current_bridge() {
    ACTIVE_BRIDGE.with(|b| {
        if let Some(bridge) = b.borrow_mut().as_mut() {
            bridge.service();
        }
    });
}

/// Answers pending `bish session capture` requests with what `screen`
/// returns, and says whether there were any. `screen` runs only when
/// someone is waiting, and is a closure because the screen lives in
/// repl.rs's state, which this module can't see.
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

/// Who asked a question, kept by whoever can't answer it yet. Most are
/// answered in the tick they arrive; one waiting on a person's decision
/// is answered much later, and must reach only the client that asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller(ClientId);

/// `answer_rpc_requests`, passing each question's `Caller` for a handler
/// that answers some later with `reply_to`. `None` sends nothing now.
pub fn answer_rpc_requests_with(mut handle: impl FnMut(&[u8], Caller) -> Option<Vec<u8>>) -> bool {
    ACTIVE_BRIDGE.with(|b| {
        let mut guard = b.borrow_mut();
        let Some(bridge) = guard.as_mut() else { return false };
        if !bridge.wants_rpc() {
            return false;
        }
        for (id, payload) in std::mem::take(&mut bridge.pending_rpc) {
            let Some(reply) = handle(&payload, Caller(id)) else { continue };
            bridge.send_to(id, &Message::RpcReply(reply).encode());
        }
        true
    })
}

/// The answer to an earlier question, to the client that asked; one that
/// left gets nothing.
pub fn reply_to(caller: Caller, payload: &[u8]) {
    ACTIVE_BRIDGE.with(|b| {
        if let Some(bridge) = b.borrow_mut().as_mut() {
            bridge.send_to(caller.0, &Message::RpcReply(payload.to_vec()).encode());
        }
    });
}

/// Sends one unsolicited JSON-RPC message to every peer that has spoken;
/// usually there are none.
pub fn push_rpc_notification(payload: &[u8]) {
    ACTIVE_BRIDGE.with(|b| {
        if let Some(bridge) = b.borrow_mut().as_mut() {
            bridge.notify_rpc(payload);
        }
    });
}

/// Whether anything listens for notifications, so a caller can skip
/// building one.
pub fn has_rpc_peers() -> bool {
    ACTIVE_BRIDGE.with(|b| b.borrow().as_ref().is_some_and(|bridge| !bridge.rpc_clients.is_empty()))
}

/// Whether this process has a socket to answer on: a session daemon's, or
/// a plain bish's (see `listen_here`).
pub fn is_listening() -> bool {
    ACTIVE_BRIDGE.with(|b| b.try_borrow().map_or(true, |bridge| bridge.is_some()))
}

// A question answered outside the idle loop; see `answer_while_busy`.
type BusyAnswer = Box<dyn Fn(&[u8]) -> Option<Vec<u8>>>;

thread_local! {
    static BUSY_ANSWER: RefCell<Option<BusyAnswer>> = const { RefCell::new(None) };
}

/// What `service_while_busy` may answer while a command holds the idle
/// loop, `None` once it doesn't. repl.rs sets it before a line runs,
/// from the editor's state then, which holds until the line is done.
pub fn answer_while_busy(answer: Option<BusyAnswer>) {
    BUSY_ANSWER.with(|a| *a.borrow_mut() = answer);
}

/// Services the socket from a wait outside the idle loop: a command
/// partway through a list, or at a prompt with no panes, where a peer
/// would otherwise give up.
///
/// Answers only what `answer_while_busy` covers; anything else (a review,
/// a capture, an unknown question) stays queued in order for the idle
/// loop. Does nothing inside a coroutine, or while the bridge is in use
/// further up this thread.
pub fn service_while_busy() {
    if crate::coroutine::in_coroutine() {
        return;
    }
    ACTIVE_BRIDGE.with(|b| {
        let Ok(mut guard) = b.try_borrow_mut() else { return };
        let Some(bridge) = guard.as_mut() else { return };
        bridge.service();
        BUSY_ANSWER.with(|a| {
            let Ok(answer) = a.try_borrow() else { return };
            let Some(answer) = answer.as_ref() else { return };
            let mut unanswered = Vec::new();
            for (id, question) in std::mem::take(&mut bridge.pending_rpc) {
                match answer(&question) {
                    Some(reply) => bridge.send_to(id, &Message::RpcReply(reply).encode()),
                    None => unanswered.push((id, question)),
                }
            }
            bridge.pending_rpc = unanswered;
        });
    });
}

// True once per attach, for repl.rs's one full repaint (see
// `SessionBridge::just_attached`).
pub fn take_bridge_just_attached() -> bool {
    ACTIVE_BRIDGE.with(|b| b.borrow_mut().as_mut().is_some_and(|bridge| bridge.take_just_attached()))
}

// The last attached client's (term, colorterm), if not yet taken; see
// `SessionBridge::pending_capability`.
pub fn take_pending_capability() -> Option<(String, String)> {
    ACTIVE_BRIDGE.with(|b| b.borrow_mut().as_mut().and_then(|bridge| bridge.take_pending_capability()))
}

// Ignores SIGHUP so a detached daemon survives the terminal or ssh
// session that launched it. `setsid` happens in
// `pty::attach_self_to_pty`; this covers the window before.
fn daemonize() {
    term::ignore_sighup();
}

use crate::term;

// The daemon: binds the session's socket, gives this process its own
// pty (see the module doc), installs the bridge, and runs `repl::run`.
// Returns only on a startup failure; otherwise the shell runs until it
// exits.
pub fn run_daemon(name: &str) -> io::Result<i32> {
    check_name(name)?;
    ensure_socket_dir()?;
    // First, before the socket: fails fast if a live daemon (or a racing
    // `bish session new`) holds this name, instead of clobbering its socket.
    let pidfile_lock = acquire_pidfile_lock(name)?;

    let sock_path = socket_path(name);
    // Safe to remove: holding the pidfile's lock proves nothing live is
    // bound here.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    let pty = crate::pty::open()?;
    let slave_path = pty.slave_path.clone();
    let bridge = SessionBridge::new(pty.master, listener)?;

    daemonize();
    crate::pty::attach_self_to_pty(&slave_path)?;

    // The bridge keeps the attached count in the pidfile, which is how `ls`
    // knows it, and holds the lock for the process's life.
    let mut bridge = bridge;
    bridge.watch_pidfile(pidfile_lock);
    install_bridge(bridge);

    let mut shell = crate::exec::Shell::new();
    // How a program in a pane finds this session without its name (`bish
    // tool mcp-server` does); see `listen_here`.
    shell.export_var("BISH_SOCKET", sock_path.to_string_lossy().into_owned());
    // `true`: a session daemon reads the user's config like any interactive
    // shell.
    crate::repl::run(shell, true, true);
    Ok(0)
}

/// Makes this interactive bish reachable as a named session is, and
/// returns the path its panes get as `BISH_SOCKET`, so a program inside
/// it (an agent's MCP server) can ask what's open and what's wrong.
///
/// In the same 0700 directory, with peer uids checked. Named `<pid>.ide`,
/// not `.sock`, so `session ls` and `attach` skip it: there's no terminal
/// to attach to.
///
/// Answered from the idle loop, so an agent running as a command in a
/// pane gets answers. Not while a command runs at a prompt with no panes
/// or partway through a list (`cd proj && claude`, in
/// `drive_pending_fg_inline`).
///
/// A bish that dies without `stop_listening` leaves its socket; the next
/// to start finds nothing answering on it and removes it, testing by
/// connecting, since pids are reused.
pub fn listen_here() -> io::Result<PathBuf> {
    if !cfg!(feature = "sessions") {
        return Err(io::Error::new(io::ErrorKind::Unsupported, "detachable sessions are not part of this build"));
    }
    let dir = ensure_socket_dir()?;
    remove_unanswered_ide_sockets(&dir);
    let path = ide_socket_path(crate::platform::process_id());
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    install_bridge(SessionBridge::without_terminal(listener)?);
    Ok(path)
}

/// Where a plain bish with this pid listens; see `listen_here`.
pub fn ide_socket_path(pid: u32) -> PathBuf {
    socket_dir().join(format!("{pid}.ide"))
}

/// Removes this bish's socket as it exits. A no-op for a session daemon,
/// whose socket and pidfile are handled together.
pub fn stop_listening() {
    let plain = ACTIVE_BRIDGE.with(|b| b.borrow().as_ref().is_some_and(|bridge| bridge.pty_master.is_none()));
    if plain {
        let _ = std::fs::remove_file(ide_socket_path(crate::platform::process_id()));
    }
}

fn remove_unanswered_ide_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "ide")
            && matches!(UnixStream::connect(&path), Err(e) if e.kind() == io::ErrorKind::ConnectionRefused)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// `bish session ls`: every name in the socket directory whose pidfile a
// live daemon holds (see `is_daemon_alive`); stale pairs are skipped.
pub fn run_ls() -> io::Result<i32> {
    let entries = match std::fs::read_dir(socket_dir()) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> =
        entries.flatten().filter_map(|entry| entry.file_name().to_str().and_then(|s| s.strip_suffix(".sock")).map(str::to_string)).collect();
    crate::sort::sort(&mut names);
    let live: Vec<String> = names.into_iter().filter(|name| is_daemon_alive(name)).collect();
    if live.is_empty() {
        println!("no sessions");
        return Ok(0);
    }
    // Names line up, as the column people scan.
    let width = live.iter().map(String::len).max().unwrap_or(0);
    for name in live {
        let attached = match read_attached(&name) {
            Some(0) => "detached".to_string(),
            Some(1) => "1 client".to_string(),
            Some(n) => format!("{n} clients"),
            // A daemon that hasn't written its status line yet.
            None => "?".to_string(),
        };
        let age = session_age(&name).map(short_duration).unwrap_or_else(|| "?".to_string());
        let cwd = read_pid(&name).and_then(session_cwd).map(|p| p.display().to_string()).unwrap_or_default();
        println!("{name:width$}  {attached:<10}  {age:>4} old  {cwd}");
    }
    Ok(0)
}

/// A connection to a running session kept open for many answers, for a
/// peer that stays (`bish tool mcp-server`); the daemon can push
/// notifications down it. `run_capture` asks once and exits.
pub struct Peer {
    stream: UnixStream,
    decoder: Decoder,
}

impl Peer {
    /// Connects to `name`, or says why not, naming the likely cause: a
    /// session never started looks just like a typo.
    pub fn connect(name: &str) -> io::Result<Peer> {
        check_name(name)?;
        let path = socket_path(name);
        let stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                io::Error::new(e.kind(), format!("no live session named '{name}' (try `bish session ls`)"))
            }
            _ => e,
        })?;
        Ok(Peer { stream, decoder: Decoder::default() })
    }

    /// Connects to the bish listening at `path`, the one a program runs
    /// inside, from its `BISH_SOCKET` (see `listen_here`).
    pub fn connect_path(path: &std::path::Path) -> io::Result<Peer> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                io::Error::new(e.kind(), format!("no bish is listening at {} any more", path.display()))
            }
            _ => e,
        })?;
        Ok(Peer { stream, decoder: Decoder::default() })
    }

    /// A peer over a connected socket, for tests: a daemon needs a
    /// controlling terminal, and a socketpair exercises the protocol
    /// without one.
    #[cfg(test)]
    pub fn over(stream: UnixStream) -> Peer {
        Peer { stream, decoder: Decoder::default() }
    }

    /// Says something with no answer expected: how a peer announces itself,
    /// as the daemon only knows of a peer once it has spoken.
    pub fn tell(&mut self, payload: &[u8]) -> io::Result<()> {
        self.stream.write_all(&Message::Rpc(payload.to_vec()).encode())?;
        self.stream.flush()
    }

    /// Asks one question and waits up to `timeout`. The daemon answers from
    /// its idle tick, which a foreground command holds; the timeout turns
    /// that wait into an error message.
    pub fn ask(&mut self, payload: &[u8], timeout: std::time::Duration) -> io::Result<Vec<u8>> {
        self.stream.write_all(&Message::Rpc(payload.to_vec()).encode())?;
        self.stream.flush()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(reply) = self.next_reply()? {
                return Ok(reply);
            }
            let left = deadline.checked_duration_since(std::time::Instant::now());
            let Some(left) = left else {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the session did not answer in time"));
            };
            self.stream.set_read_timeout(Some(left.min(std::time::Duration::from_millis(250))))?;
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the session went away")),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// This connection's descriptor, for waiting on it with something else,
    /// as anything reading notifications must.
    pub fn fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.stream.as_raw_fd()
    }

    /// Reads what has arrived without waiting and returns each complete
    /// message: how notifications, which nobody waits on, are collected.
    pub fn poll(&mut self) -> io::Result<Vec<Vec<u8>>> {
        self.stream.set_read_timeout(Some(std::time::Duration::from_millis(1)))?;
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the session went away")),
            Ok(n) => self.decoder.feed(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
        let mut out = Vec::new();
        while let Some(reply) = self.next_reply()? {
            out.push(reply);
        }
        Ok(out)
    }

    /// Waits for the reply `wanted` picks out, however long, returning what
    /// else arrived meanwhile so notifications aren't lost. No timeout: it
    /// waits on a person deciding, and the session ending stops it
    /// otherwise.
    pub fn wait_for(&mut self, wanted: impl Fn(&[u8]) -> bool) -> io::Result<(Vec<u8>, Vec<Vec<u8>>)> {
        let mut others = Vec::new();
        self.stream.set_read_timeout(None)?;
        loop {
            while let Some(reply) = self.next_reply()? {
                if wanted(&reply) {
                    return Ok((reply, others));
                }
                others.push(reply);
            }
            let mut buf = [0u8; 8192];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the session went away")),
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The next reply already arrived, if any. The session's screen bytes,
    /// which every client gets, are dropped: a peer asking isn't a terminal.
    fn next_reply(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            match self.decoder.next_message() {
                Ok(Some(Message::RpcReply(payload))) => return Ok(Some(payload)),
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(None),
                Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
            }
        }
    }
}

/// The bytes a `bish session send` argument stands for: a key name is
/// that key, anything else its literal text. `send make Enter` reads
/// better than any escaping; a literal word spelled "Enter" is rare.
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
        // `C-a` to `C-z`, and the few above them a terminal spells the same
        // way.
        _ => {
            if let Some(rest) = arg.strip_prefix("C-")
                && rest.chars().count() == 1
            {
                let c = rest.chars().next().expect("one character");
                if c.is_ascii_alphabetic() {
                    return vec![(c.to_ascii_lowercase() as u8) - b'a' + 1];
                }
                // C-Space is NUL, still accepted as the detach key (Ctrl+G
                // is the advertised one).
                if c == '@' {
                    return vec![0];
                }
            }
            return arg.as_bytes().to_vec();
        }
    };
    named.to_vec()
}

/// `bish session send <name> <arg>...`: types into a running session.
/// Connects, writes and closes, with no handshake, so it never affects
/// the session's size or causes a redraw. The daemon takes what arrived
/// with the EOF (see `drain_one_client`).
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

/// `bish session capture <name>`: prints what the session's focused pane
/// shows. The daemon answers from its idle tick (see
/// `answer_capture_requests`), so the wait is bounded: a wedged daemon
/// ends in a message, not a hang.
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
                // Screen traffic: while connected this is a client like any
                // other. Ignored; it came to ask.
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

/// `bish session rename <old> <new>`: renames the socket and the
/// pidfile, which is all a name is. The daemon holds both by descriptor
/// (the listener on the socket's inode, the pidfile's lock and status
/// writes through its fd), so it carries on, and new clients find it
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
    // A dead name may still have files (a daemon killed between the
    // SIGTERM and the sweep); they're nobody's, and would fail the rename.
    let _ = std::fs::remove_file(socket_path(to));
    let _ = std::fs::remove_file(pidfile_path(to));
    // The socket first: it's what clients connect to, so the window where
    // neither name works is shortest.
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
    if !crate::platform::send_signal(pid, SIGTERM) {
        return Err(io::Error::last_os_error());
    }
    // A daemon killed by a signal leaves its socket and pidfile. `ls`
    // ignores them (it tests the lock), but they'd pile up in the runtime
    // directory.
    remove_dead_session_files(name, std::time::Duration::from_secs(2));
    Ok(0)
}

/// Removes a session's socket and pidfile once nothing holds them. Waits
/// up to `grace` for the daemon to go, then checks again just before
/// unlinking: a new daemon may have claimed the name meanwhile.
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

// The client: connects to `name`'s socket, puts the local terminal in
// raw mode, sends the handshake, and relays both ways until the
// connection ends or the user detaches. With nothing else to do, it
// blocks in `PollSet::wait` rather than polling.
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

    // The local terminal can resize any time (a window drag, an ssh
    // reconnect), and this blocks in poll with nothing periodic, so
    // SIGWINCH wakes the loop through a self-pipe.
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
                let Ok(n) = crate::platform::read_bytes(0, &mut buf) else { break 'relay };
                if n == 0 {
                    break 'relay;
                }
                // Ctrl+G (0x07), or Ctrl+Space's NUL, alone in a read: the
                // detach key, as in-process (see `Frame` in repl.rs). A
                // paste starting with either isn't guarded against.
                if n == 1 && matches!(buf[0], 0x07 | 0x00) {
                    break 'relay;
                }
                let msg = Message::Bytes(buf[..n].to_vec());
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

/// `bish session new <name>`, and `new -A <name>`: spawns the daemon (a
/// detached child re-exec'ing this binary with `--daemon`) and attaches
/// to it once its socket is up.
///
/// `attach_if_running` is `-A`: attach if the session exists, else create
/// it. Without it a taken name is an error: a second daemon would lose
/// the pidfile lock and die, and the connect below would silently attach
/// to the existing session instead.
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
    // A declared length promises bytes not yet arrived; believing 4 GiB,
    // `feed` grows forever while the peer dribbles.
    #[test]
    fn an_oversized_frame_is_refused_on_its_header() {
        let mut d = super::Decoder::new();
        let mut header = vec![super::KIND_BYTES];
        header.extend_from_slice(&(u32::MAX).to_be_bytes());
        d.feed(&header);
        let err = d.next_message().unwrap_err();
        assert!(err.contains("exceeds"), "{err}");

        // Refused on the five header bytes; the payload never has to arrive.
        assert_eq!(header.len(), 5);

        // The largest legal frame still decodes.
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

    // `XDG_RUNTIME_DIR` is process-wide and tests run in parallel, so every
    // test reading `socket_dir()` holds this lock while it pins the
    // variable, or they see each other's values.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // Runs `body` with `XDG_RUNTIME_DIR` set to a fresh temp dir unique per
    // `tag`, holding `ENV_LOCK` and restoring the variable after. A poisoned
    // lock is recovered rather than failing every later test.
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

    // A busy session's screen isn't sent to a peer asking questions. A
    // socket pair stands in for the pty master so what each client gets can
    // be read back.
    #[test]
    fn the_screen_goes_to_whoever_watches_it_and_not_to_a_peer_asking_questions() {
        let (master, master_far_end) = UnixStream::pair().unwrap();
        let master_read = std::fs::File::from(std::os::fd::OwnedFd::from(master_far_end));
        let (watcher_end, mut watcher) = UnixStream::pair().unwrap();
        let (asker_end, mut asker) = UnixStream::pair().unwrap();
        let writes = Arc::new(Mutex::new(vec![(1, watcher_end, true), (2, asker_end, false)]));
        std::thread::spawn({
            let writes = writes.clone();
            move || drain_pty_master_thread(master_read, writes)
        });
        (&master).write_all(b"hello").unwrap();

        watcher.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut decoder = Decoder::new();
        let mut buf = [0u8; 64];
        let got = loop {
            let n = watcher.read(&mut buf).unwrap();
            decoder.feed(&buf[..n]);
            if let Ok(Some(message)) = decoder.next_message() {
                break message;
            }
        };
        assert!(matches!(&got, Message::Bytes(bytes) if bytes == b"hello"));
        // Written in the same pass as the watcher's, so it would be there by
        // now.
        asker.set_nonblocking(true).unwrap();
        assert_eq!(asker.read(&mut buf).unwrap_err().kind(), io::ErrorKind::WouldBlock, "nothing for the peer asking questions");
    }

    #[test]
    fn a_client_stops_being_sent_the_screen_once_it_asks_a_question() {
        with_isolated_runtime_dir("asker", |_| {
            let dir = ensure_socket_dir().unwrap();
            let mut bridge = SessionBridge::without_terminal(UnixListener::bind(dir.join("t.sock")).unwrap()).unwrap();
            let (ours, mut theirs) = UnixStream::pair().unwrap();
            bridge.attach(ours);
            let watching = |bridge: &SessionBridge| bridge.writes.lock().unwrap().iter().map(|(_, _, watching)| *watching).collect::<Vec<_>>();
            assert_eq!(watching(&bridge), [true], "a client is watching until it says otherwise");
            theirs.write_all(&Message::Rpc(br#"{"q":"hello"}"#.to_vec()).encode()).unwrap();
            bridge.drain_clients();
            assert_eq!(watching(&bridge), [false]);
            assert_eq!(bridge.pending_rpc.len(), 1, "and its question is still there to answer");
        });
    }

    // While a command holds the idle loop, a question the stored answer
    // covers is answered from the wait, and one it doesn't stays queued in
    // order for the loop.
    #[test]
    fn a_busy_wait_answers_what_it_can_and_leaves_the_rest_queued() {
        with_isolated_runtime_dir("busy", |_| {
            let path = listen_here().unwrap();
            answer_while_busy(Some(Box::new(|question: &[u8]| (question == b"state").then(|| b"the state".to_vec()))));
            let asker = std::thread::spawn({
                let path = path.clone();
                move || {
                    let mut peer = Peer::connect_path(&path).unwrap();
                    peer.tell(b"later").unwrap();
                    peer.ask(b"state", std::time::Duration::from_secs(10)).unwrap()
                }
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !asker.is_finished() && std::time::Instant::now() < deadline {
                service_while_busy();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(asker.join().unwrap(), b"the state");
            let left = ACTIVE_BRIDGE.with(|b| b.borrow().as_ref().unwrap().pending_rpc.iter().map(|(_, q)| q.clone()).collect::<Vec<_>>());
            assert_eq!(left, [b"later".to_vec()], "left for the idle loop");
            answer_while_busy(None);
            stop_listening();
            ACTIVE_BRIDGE.with(|b| *b.borrow_mut() = None);
        });
    }

    // A plain bish's socket end to end: a question through
    // `Peer::connect_path` is answered, a peer arriving repaints nothing,
    // and a dead bish's socket goes when the next one listens.
    #[test]
    fn a_plain_bish_answers_on_its_own_socket_and_sweeps_up_a_dead_ones() {
        with_isolated_runtime_dir("listen-here", |_| {
            let dir = ensure_socket_dir().unwrap();
            let dead = dir.join("999999999.ide");
            drop(UnixListener::bind(&dead).unwrap());
            let session_like = dir.join("work.sock");
            drop(UnixListener::bind(&session_like).unwrap());

            let path = listen_here().unwrap();
            assert_eq!(path, ide_socket_path(std::process::id()));
            assert!(!dead.exists(), "nothing answered on it, so it was swept");
            assert!(session_like.exists(), "a session's own files are not this sweep's to touch");

            let asker = std::thread::spawn({
                let path = path.clone();
                move || Peer::connect_path(&path).unwrap().ask(br#"{"q":"ping"}"#, std::time::Duration::from_secs(10)).unwrap()
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut repainted = false;
            while !asker.is_finished() && std::time::Instant::now() < deadline {
                service_current_bridge();
                repainted |= take_bridge_just_attached();
                answer_rpc_requests_with(|payload, _| Some([b"pong:".as_slice(), payload].concat()));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(asker.join().unwrap(), br#"pong:{"q":"ping"}"#);
            assert!(!repainted, "a peer with no terminal is no reason to repaint one");

            stop_listening();
            assert!(!path.exists(), "gone as this bish leaves");
            ACTIVE_BRIDGE.with(|b| *b.borrow_mut() = None);
            let Err(gone) = Peer::connect_path(&path) else { panic!("connected to a bish that has left") };
            assert!(gone.to_string().contains("no bish is listening"), "{gone}");
        });
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
        // `Path::starts_with` compares whole components, and `bish-1000`
        // isn't `bish-`, so this checks the string.
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
        // A word that isn't a key name is the word.
        assert_eq!(key_bytes("make"), b"make");
        assert_eq!(key_bytes("echo hello"), b"echo hello");
        // Including one that only looks like one.
        assert_eq!(key_bytes("C-"), b"C-");
        assert_eq!(key_bytes("C-hello"), b"C-hello");
    }

    #[test]
    fn control_keys_are_the_control_codes_they_stand_for() {
        assert_eq!(key_bytes("C-c"), vec![3]);
        assert_eq!(key_bytes("C-d"), vec![4]);
        assert_eq!(key_bytes("C-a"), vec![1]);
        assert_eq!(key_bytes("C-z"), vec![26]);
        // Case doesn't matter.
        assert_eq!(key_bytes("C-C"), vec![3]);
        // C-Space, also the detach key.
        assert_eq!(key_bytes("C-@"), vec![0]);
        // The two spellings a terminal shares.
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
        // Only the first unit: "3h", not "3h07m12s".
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

            // Rewritten in place, shorter; the old line mustn't show through.
            write_status(&mut file, 0);
            assert_eq!(read_attached("s"), Some(0));
            assert_eq!(read_pid("s"), Some(std::process::id() as i32));
        });
    }

    #[test]
    fn a_pidfile_with_no_status_line_reads_as_unknown_not_as_nobody() {
        // A pidfile with no status line: "?" in the listing, not "detached".
        with_isolated_runtime_dir("old-pidfile", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            std::fs::write(pidfile_path("old"), b"4242\n").expect("pidfile");
            assert_eq!(read_pid("old"), Some(4242));
            assert_eq!(read_attached("old"), None);
        });
    }

    #[test]
    fn a_shared_session_runs_at_the_smallest_attached_size() {
        // Each direction independently.
        assert_eq!(smallest_size(&[(40, 100), (24, 80)]), Some((24, 80)));
        assert_eq!(smallest_size(&[(24, 200), (60, 80)]), Some((24, 80)));
        assert_eq!(smallest_size(&[(30, 90)]), Some((30, 90)));
    }

    #[test]
    fn a_client_that_has_not_said_its_size_yet_does_not_shrink_the_session() {
        // A client reports (0, 0) until its handshake arrives; taken
        // literally it would collapse the pty for everyone.
        assert_eq!(smallest_size(&[(40, 100), (0, 0)]), Some((40, 100)));
        assert_eq!(smallest_size(&[(0, 0)]), None, "and with nobody sized yet, leave the pty alone");
        assert_eq!(smallest_size(&[]), None, "same when nobody is attached at all");
    }

    #[test]
    fn dead_session_files_are_removed_once_nothing_holds_them() {
        with_isolated_runtime_dir("dead-files", |_dir| {
            ensure_socket_dir().expect("ensure_socket_dir");
            // What a killed daemon leaves: both files outlive it.
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
            // Holding the lock is what a live daemon means (see
            // `acquire_pidfile_lock`), so this stands in for a new daemon
            // claiming the name between the kill and the sweep.
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
            // `flock` belongs to the open file description, not the
            // process, so a second open and lock conflicts even in the same
            // process, standing in for a second `bish session new work`.
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
    fn the_rpc_messages_round_trip() {
        // A JSON-RPC body is just bytes here; the framing doesn't care.
        let request = Message::Rpc(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_vec());
        let mut dec = Decoder::new();
        dec.feed(&request.encode());
        assert_eq!(dec.next_message().unwrap(), Some(request));

        let reply = Message::RpcReply(br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#.to_vec());
        let mut dec = Decoder::new();
        dec.feed(&reply.encode());
        assert_eq!(dec.next_message().unwrap(), Some(reply));

        // Even one that isn't text: this layer counts bytes.
        let raw = Message::Rpc(vec![0x00, 0xff, 0xfe, b'\n']);
        let mut dec = Decoder::new();
        dec.feed(&raw.encode());
        assert_eq!(dec.next_message().unwrap(), Some(raw));
    }

    // Two messages glued together and fed byte by byte, as a stream socket
    // delivers them.
    #[test]
    fn rpc_messages_survive_arriving_in_pieces() {
        let one = Message::Rpc(b"{\"id\":1}".to_vec());
        let two = Message::RpcReply(b"{\"id\":2}".to_vec());
        let mut wire = one.encode();
        wire.extend_from_slice(&two.encode());

        let first = one.encode().len();
        let mut dec = Decoder::new();
        let mut seen = Vec::new();
        for (at, byte) in wire.iter().enumerate() {
            dec.feed(&[*byte]);
            while let Some(msg) = dec.next_message().unwrap() {
                // Each appears on the byte that completes it, not before.
                assert!(at + 1 == first || at + 1 == wire.len(), "a message surfaced at byte {}", at + 1);
                seen.push(msg);
            }
        }
        assert_eq!(seen, vec![one, two]);
    }

    // Refused on the five header bytes, before any body.
    #[test]
    fn an_oversized_rpc_frame_is_refused_on_its_header() {
        let mut dec = Decoder::new();
        let mut header = vec![6u8];
        header.extend_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
        dec.feed(&header);
        assert!(dec.next_message().is_err());
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
