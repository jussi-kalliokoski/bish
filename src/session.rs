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
    std::fs::read_to_string(pidfile_path(name)).ok()?.trim().parse().ok()
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
pub struct SessionBridge {
    listener: UnixListener,
    // The main thread's own handle: read from directly (drain_client),
    // written to only via a fresh try_clone() each time a new client is
    // accepted (see try_accept) -- kept distinct from
    // `client_write_shared` below so accept/detach only ever need to
    // touch this field, no lock required on the hot path most on_idle
    // ticks actually take (no new client, no new input).
    client_read: Option<UnixStream>,
    // Shared with the background pty-master-draining thread -- the only
    // thing that thread ever touches. Updated (attach: Some, detach/
    // EOF: None) by the main thread alongside `client_read` so both
    // always agree on whether a client is currently attached.
    client_write: Arc<Mutex<Option<UnixStream>>>,
    // The main thread's own pty-master handle -- writes only (client
    // input, `Pty::set_size`); the background thread owns the read side
    // via its own separate fd (see SessionBridge::new).
    pty_master: std::fs::File,
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
fn drain_pty_master_thread(mut pty_master_read: std::fs::File, client_write: Arc<Mutex<Option<UnixStream>>>) {
    let mut buf = [0u8; 4096];
    loop {
        match pty_master_read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let msg = Message::Bytes(buf[..n].to_vec());
                let encoded = msg.encode();
                if let Ok(guard) = client_write.lock()
                    && let Some(stream) = guard.as_ref()
                {
                    let _ = (&*stream).write_all(&encoded);
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
        let client_write = Arc::new(Mutex::new(None));
        let client_write_for_thread = client_write.clone();
        std::thread::spawn(move || drain_pty_master_thread(pty_master_read, client_write_for_thread));
        Ok(SessionBridge { listener, client_read: None, client_write, pty_master, decoder: Decoder::new(), just_attached: false, pending_capability: None })
    }

    pub fn is_attached(&self) -> bool {
        self.client_read.is_some()
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
        if self.client_read.is_none() {
            self.try_accept();
        }
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
                        match stream.try_clone() {
                            Ok(write_half) => {
                                *self.client_write.lock().unwrap_or_else(|p| p.into_inner()) = Some(write_half);
                                self.client_read = Some(stream);
                                self.decoder = Decoder::new();
                                self.just_attached = true;
                            }
                            Err(_) => { /* couldn't clone -- drop `stream`, try again next tick */ }
                        }
                    }
                    _ => { /* wrong UID or the check itself failed -- drop `stream` */ }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
    }

    // Clears both `client_read` (this struct's own handle) and
    // `client_write` (the background thread's shared handle) together
    // -- the one place either is ever set back to None, so the two
    // never disagree about whether a client is attached.
    fn detach(&mut self) {
        self.client_read = None;
        *self.client_write.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn drain_client(&mut self) {
        let Some(client) = &mut self.client_read else { return };
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
            self.detach();
            return;
        }
        loop {
            match self.decoder.next_message() {
                Ok(Some(Message::Bytes(payload))) => {
                    // A write to the pty master's own kernel buffer --
                    // bounded by that buffer's size, same as any other
                    // pty write in this codebase (e.g. drive_fg_job
                    // forwarding a real paste/mouse sequence into a
                    // job's own pty); not a new risk category (see this
                    // struct's own doc comment for why the *other*
                    // direction was the one that could actually
                    // deadlock, and why this one doesn't the same way).
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
                Ok(Some(Message::Passthrough(_))) => {
                    // A client never sends this direction in this
                    // protocol (server -> client only, see Message's own
                    // doc comment) -- tolerated as a no-op rather than
                    // treated as malformed, in case that ever changes.
                }
                Ok(None) => break,
                Err(_) => {
                    self.detach();
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

    install_bridge(bridge);

    let shell = crate::exec::Shell::new();
    crate::repl::run(shell, true);
    // Not reached in practice (repl::run only returns by way of the
    // whole process exiting) -- kept so a future change to repl::run's
    // own exit story doesn't silently drop the pidfile lock early
    // without a compiler-visible reason to reconsider this.
    drop(pidfile_lock);
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
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str().and_then(|s| s.strip_suffix(".sock")).map(str::to_string))
        .collect();
    names.sort();
    let mut any = false;
    for name in names {
        if is_daemon_alive(&name) {
            any = true;
            match read_pid(&name) {
                Some(pid) => println!("{}\t(pid {})", name, pid),
                None => println!("{}", name),
            }
        }
    }
    if !any {
        println!("no sessions");
    }
    Ok(0)
}

// `bish session kill <name>`: verifies a live daemon actually holds
// this name (see is_daemon_alive) immediately before signaling it --
// narrows, though can't fully eliminate, the PID-reuse race any
// pidfile-based scheme has (the daemon would have to exit *and* have
// its exact PID reused by something else, both within this function's
// own brief window) -- see acquire_pidfile_lock's own doc comment for
// why this is still a meaningfully smaller risk than trusting a raw PID
// with no liveness check at all. Sends SIGTERM (bish installs no
// handler for it, so this terminates the daemon immediately) rather
// than attempting a graceful in-shell shutdown -- matches what
// tmux/screen's own kill-session already does, not a new tradeoff.
pub fn run_kill(name: &str) -> io::Result<i32> {
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
