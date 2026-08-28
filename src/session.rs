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

unsafe extern "C" {
    fn getuid() -> u32;
    fn mkdir(path: *const i8, mode: u32) -> i32;
    fn getsockopt(sockfd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut u32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
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

// The stateless byte relay between a daemon's own pty master and
// whichever client socket is currently attached, if any -- see this
// module's own top doc comment for why draining the pty master must
// never be skipped, attached or not.
pub struct SessionBridge {
    pty_master: std::fs::File,
    listener: UnixListener,
    client: Option<UnixStream>,
    decoder: Decoder,
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
}

// Bounds how many chunks get drained per tick before returning control
// -- same reasoning drive_fg_job's own MAX_READS_PER_TICK already
// documents: a firehose producer (a job printing in a tight loop) could
// otherwise keep this side of the bridge busy indefinitely, starving
// every other on_idle responsibility (job draining, WINCH, the next
// keystroke) that also needs a turn.
const MAX_READS_PER_TICK: u32 = 16;

impl SessionBridge {
    // `pty_master`/`listener` are both set non-blocking here, so
    // callers don't need to remember to -- there's no correct blocking
    // use of either from inside `service`.
    pub fn new(pty_master: std::fs::File, listener: UnixListener) -> io::Result<SessionBridge> {
        crate::pty::set_nonblocking(pty_master.as_raw_fd());
        listener.set_nonblocking(true)?;
        Ok(SessionBridge { pty_master, listener, client: None, decoder: Decoder::new(), just_attached: false })
    }

    pub fn is_attached(&self) -> bool {
        self.client.is_some()
    }

    pub fn take_just_attached(&mut self) -> bool {
        std::mem::replace(&mut self.just_attached, false)
    }

    // Called once per on_idle tick (see service_current_bridge below).
    // Never blocks.
    pub fn service(&mut self) {
        if self.client.is_none() {
            self.try_accept();
        }
        self.drain_pty_master();
        self.drain_client();
    }

    fn try_accept(&mut self) {
        match self.listener.accept() {
            Ok((stream, _addr)) => {
                // Verified once, at connect time -- see peer_uid's own
                // doc comment on why this is defense in depth on top of
                // the socket directory's own 0700 permissions, not a
                // replacement for them. A mismatched UID (only reachable
                // at all under a misconfigured/shared runtime directory)
                // just drops the connection rather than accepting a
                // second, unauthenticated bridge target.
                match peer_uid(&stream) {
                    Ok(uid) if uid == current_uid() => {
                        let _ = stream.set_nonblocking(true);
                        self.client = Some(stream);
                        self.decoder = Decoder::new();
                        self.just_attached = true;
                    }
                    _ => { /* wrong UID or the check itself failed -- drop `stream` */ }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
    }

    fn drain_pty_master(&mut self) {
        let mut buf = [0u8; 4096];
        for _ in 0..MAX_READS_PER_TICK {
            match self.pty_master.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Some(client) = &mut self.client {
                        let msg = Message::Bytes(buf[..n].to_vec());
                        if client.write_all(&msg.encode()).is_err() {
                            self.client = None;
                        }
                    }
                    // No client attached: discard. Draining still has to
                    // happen either way -- see this module's own top doc
                    // comment on why an unattended daemon must never
                    // stop reading its own pty master.
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn drain_client(&mut self) {
        let Some(client) = &mut self.client else { return };
        let mut buf = [0u8; 4096];
        let mut disconnected = false;
        for _ in 0..MAX_READS_PER_TICK {
            match client.read(&mut buf) {
                Ok(0) => {
                    disconnected = true;
                    break;
                }
                Ok(n) => self.decoder.feed(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected {
            self.client = None;
            return;
        }
        loop {
            match self.decoder.next_message() {
                Ok(Some(Message::Bytes(payload))) => {
                    // A write to the pty master's own kernel buffer --
                    // bounded by that buffer's size, same as any other
                    // pty write in this codebase (e.g. drive_fg_job
                    // forwarding a real paste/mouse sequence into a
                    // job's own pty); not a new risk category.
                    let _ = self.pty_master.write_all(&payload);
                }
                Ok(Some(Message::Resize { rows, cols })) => {
                    // Resizing via the master fd is what actually
                    // delivers a real SIGWINCH to the slave's
                    // controlling-terminal holder (this same daemon
                    // process, via attach_self_to_pty) -- the existing
                    // install_winch_handler/take_winch/
                    // poll_and_apply_resize machinery in exec.rs/repl.rs
                    // picks this up with zero new code.
                    let _ = crate::pty::set_size(self.pty_master.as_raw_fd(), rows, cols);
                }
                Ok(Some(Message::Handshake { rows, cols, term, colorterm })) => {
                    let _ = crate::pty::set_size(self.pty_master.as_raw_fd(), rows, cols);
                    // detect_color_support() (exec.rs) reads these two
                    // env vars -- setting them here is today's whole
                    // "capability renegotiation" story (see the
                    // implementation plan's build-order step 6 for
                    // per-attach re-application on a *later* reattach;
                    // this alone already covers the one-client case).
                    if !term.is_empty() {
                        unsafe { std::env::set_var("TERM", &term) };
                    }
                    if !colorterm.is_empty() {
                        unsafe { std::env::set_var("COLORTERM", &colorterm) };
                    }
                }
                Ok(Some(Message::Passthrough(_))) => {
                    // A client never sends this direction in this
                    // protocol (server -> client only, see Message's own
                    // doc comment) -- tolerated as a no-op rather than
                    // treated as malformed, in case that ever changes.
                }
                Ok(None) => break,
                Err(_) => {
                    self.client = None;
                    break;
                }
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

// True at most once per attach (a first attach or a reattach) -- lets
// repl.rs's own on_idle hook trigger one explicit full repaint right
// when a client connects. See SessionBridge::just_attached's own doc
// comment for why an incremental-diff-only compositor otherwise leaves
// a freshly (re)attached client's blank real terminal stuck empty.
pub fn take_bridge_just_attached() -> bool {
    ACTIVE_BRIDGE.with(|b| b.borrow_mut().as_mut().is_some_and(|bridge| bridge.take_just_attached()))
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
    ensure_socket_dir()?;
    let sock_path = socket_path(name);
    // A stale socket file left behind by a no-longer-running daemon of
    // the same name would otherwise make `bind` fail with AddrInUse --
    // best-effort removal here; real staleness verification (is
    // anything actually still listening?) is build-order step 7's
    // pidfile+flock job, not this one's.
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    std::fs::write(pidfile_path(name), std::process::id().to_string())?;

    let pty = crate::pty::open()?;
    let slave_path = pty.slave_path.clone();
    let bridge = SessionBridge::new(pty.master, listener)?;

    daemonize();
    crate::pty::attach_self_to_pty(&slave_path)?;

    install_bridge(bridge);

    let shell = crate::exec::Shell::new();
    crate::repl::run(shell, true);
    Ok(0)
}

// The client loop: connects to `name`'s socket, puts the *local* real
// terminal into raw mode, sends the attach handshake, then relays bytes
// in both directions until the connection ends or the user detaches.
// Unlike the daemon side, this has no on_idle-style other work to
// interleave, so it blocks genuinely indefinitely in PollSet::wait
// rather than polling on an interval -- there is nothing else for a
// client to usefully do while idle.
pub fn run_client(name: &str) -> io::Result<i32> {
    let sock_path = socket_path(name);
    let mut stream = UnixStream::connect(&sock_path).map_err(|e| io::Error::new(e.kind(), format!("bish: session '{}' not found ({})", name, e)))?;

    let (rows, cols) = crate::pty::get_size(0).map(|ws| (ws.rows, ws.cols)).unwrap_or((24, 80));
    let handshake = Message::Handshake { rows, cols, term: std::env::var("TERM").unwrap_or_default(), colorterm: std::env::var("COLORTERM").unwrap_or_default() };
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
pub fn run_new(name: &str) -> io::Result<i32> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe).args(["session", "--daemon", name]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;

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
