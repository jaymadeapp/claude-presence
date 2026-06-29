//! The daemon ingest socket: a local unix-socket server that receives the
//! sanitized push path (statusline + hooks) and feeds it into the aggregator
//! (design §4.1, §1).
//!
//! Wire format: newline-delimited JSON, one [`IngestEvent`] per line. The chained
//! shell scripts pipe their stdin into `claude-presence forward --kind …`, which
//! connects here and writes the bytes (see [`forward_stdin`]).
//!
//! Security (FR-8/AC-4):
//! * the socket file is `0600`, created inside the `0700` state dir;
//! * every accepted connection is verified to come from the **same uid** via
//!   `getpeereid(2)` and dropped otherwise;
//! * raw line bytes are **never** logged — only sanitized [`Overlay`] summaries
//!   and counts.
//!
//! The server emits an [`Overlay`] per understood event onto an `mpsc` channel the
//! run loop consumes ([`serve`]); the run loop layers those deltas onto the
//! transcript-derived session set before aggregation.

use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::ingest::events::{IngestEvent, Overlay};

/// Bound on a single forwarded line so a runaway/garbage writer cannot exhaust
/// memory; a statusLine JSON is well under 8 KiB in practice.
const MAX_LINE_BYTES: u64 = 64 * 1024;

/// Bound on overlays buffered toward the run loop before backpressure; bursts of
/// hook events are absorbed without blocking accept.
const OVERLAY_CHANNEL_CAP: usize = 256;

/// Cap on in-flight connection-handler tasks (FR-7/AC-1). A flood of simultaneous
/// connections cannot spawn unbounded tasks or hold unbounded fds: a permit is
/// taken per accepted connection and a connection over the cap is dropped at once.
const MAX_INFLIGHT_CONNECTIONS: usize = 16;

/// Idle ceiling on a single read (FR-7/AC-1). A slowloris peer that connects but
/// never writes (or stalls mid-frame) is closed once no bytes arrive within this
/// window, rather than pinning a task forever.
const READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Hard ceiling on a single connection's total lifetime (FR-7/AC-1). Even a peer
/// that dribbles a byte just inside every idle window cannot hold a task open
/// indefinitely; past this deadline the connection is closed.
const CONNECTION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Resolve the daemon ingest socket path: `~/.local/state/claude-presence/daemon.sock`.
///
/// Mirrors the `0700` state-dir convention used by `lib.rs`/`logging.rs`
/// (`directories::state_dir()` is `None` on macOS, so it is built explicitly).
/// The directory is ensured `0700` and the socket is bound `0600` by [`serve`].
pub fn socket_path() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or(Error::PathResolution("home"))?;
    Ok(base
        .home_dir()
        .join(".local")
        .join("state")
        .join("claude-presence")
        .join("daemon.sock"))
}

/// A running ingest server: the accept-loop task handle plus the overlay stream.
///
/// Hold [`Self::overlays`] (drain it in the run loop) to receive sanitized
/// deltas; drop or abort [`Self::handle`] to stop accepting. The socket file is
/// removed on a clean shutdown of the loop.
pub struct IngestServer {
    /// The accept loop. Aborting it stops the server.
    pub handle: tokio::task::JoinHandle<()>,
    /// Sanitized overlays emitted per understood event.
    pub overlays: mpsc::Receiver<Overlay>,
}

/// Bind the ingest socket and spawn the accept loop (design §4.1).
///
/// Creates/tightens the `0700` state dir, removes any stale socket file, binds a
/// [`UnixListener`], and chmods the socket `0600`. Returns an [`IngestServer`]
/// whose `overlays` receiver carries one sanitized [`Overlay`] per understood
/// event. A bind failure is returned as [`Error::Ingest`] so the caller (the run
/// loop) can degrade to the JSONL-only MVP rather than crash.
pub fn serve(cfg: Config) -> Result<IngestServer> {
    let path = socket_path()?;
    let listener = bind(&path)?;
    info!("ingest: listening on daemon socket");

    let (tx, rx) = mpsc::channel::<Overlay>(OVERLAY_CHANNEL_CAP);
    let own_uid = nix::unistd::Uid::current().as_raw();
    let limit = Arc::new(Semaphore::new(MAX_INFLIGHT_CONNECTIONS));

    let handle = tokio::spawn(async move {
        accept_loop(listener, cfg, own_uid, tx, limit).await;
        // Best-effort cleanup so a later run can rebind without a stale-file race.
        let _ = std::fs::remove_file(&path);
        debug!("ingest: accept loop exited");
    });

    Ok(IngestServer {
        handle,
        overlays: rx,
    })
}

/// Create the `0700` parent dir, remove a stale socket, bind, and chmod `0600`.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        ensure_dir_0700(dir)?;
    }
    // A leftover socket from a previous run would make `bind` fail with
    // `EADDRINUSE`; remove it first (it is recreated below).
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)
        .map_err(|err| Error::Ingest(format!("could not bind daemon socket: {}", err.kind())))?;
    // Tighten to 0600 immediately after bind (the bind honours umask otherwise).
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Create `dir` (and parents) `0700`, tightening an existing looser dir.
fn ensure_dir_0700(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Accept connections until the listener errors or the overlay receiver is
/// dropped, handling each on its own task.
///
/// `limit` caps in-flight handler tasks (FR-7/AC-1): a permit is taken per
/// accepted connection and held for the connection's lifetime; once
/// [`MAX_INFLIGHT_CONNECTIONS`] permits are out, a new connection is dropped at
/// once rather than spawning an unbounded task.
async fn accept_loop(
    listener: UnixListener,
    cfg: Config,
    own_uid: u32,
    tx: mpsc::Sender<Overlay>,
    limit: Arc<Semaphore>,
) {
    /// Abandon the socket after this many consecutive accept failures so a
    /// pathological flood (e.g. EMFILE that never clears) is bounded.
    const MAX_CONSECUTIVE_FAILURES: u32 = 50;
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            // The run loop dropped the overlay receiver → nothing to feed. Racing
            // this against `accept()` makes the documented "stop by dropping the
            // receiver" wake an idle accept; `select!` is cancel-safe.
            _ = tx.closed() => break,
            res = listener.accept() => match res {
                Ok((stream, _addr)) => {
                    consecutive_failures = 0;
                    // Cap in-flight handlers (FR-7/AC-1): a flood over the cap is
                    // dropped here so it can never exhaust fds/tasks. The permit is
                    // moved into the task and released when the handler returns.
                    let Ok(permit) = Arc::clone(&limit).try_acquire_owned() else {
                        warn!("ingest: connection cap reached; dropping connection");
                        continue;
                    };
                    let cfg = cfg.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        handle_conn(stream, cfg, own_uid, tx).await;
                    });
                }
                Err(err) => {
                    // A transient accept error must not kill the server; log the
                    // category (never any payload). Back off with a real delay —
                    // a persistent error (e.g. EMFILE) does not clear by yielding,
                    // so a bare reschedule would busy-spin a core.
                    warn!(kind = %err.kind(), "ingest: accept failed");
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        warn!("ingest: too many consecutive accept failures; abandoning socket");
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }
}

/// Verify the peer uid, then read newline-delimited events from one connection,
/// emitting a sanitized [`Overlay`] per understood event.
async fn handle_conn(stream: UnixStream, cfg: Config, own_uid: u32, tx: mpsc::Sender<Overlay>) {
    // Peer-uid check (FR-8/AC-4): only this user's own processes may push.
    match peer_uid(&stream) {
        Ok(peer) if peer == own_uid => {}
        Ok(_) => {
            warn!("ingest: rejected connection from foreign uid");
            return;
        }
        Err(err) => {
            warn!(kind = %err.kind(), "ingest: could not verify peer uid; dropping");
            return;
        }
    }

    read_lines(stream, cfg, tx).await;
}

/// Read newline-delimited frames from one connection and dispatch each.
///
/// Tokio's `io-util` feature (which provides `BufReader`/`lines()`) is not
/// enabled in `Cargo.toml` (owned by another task), so framing is done by hand
/// over [`UnixStream`]'s inherent `readable`/`try_read` — both available with
/// just the `net` feature. A partial trailing line is held in `buf` across reads;
/// an over-long unterminated frame closes the connection without ever being logged
/// (FR-7/AC-2, FR-8/AC-4).
///
/// Two `tokio::time::timeout` ceilings bound a misbehaving peer (FR-7/AC-1): each
/// read waits at most [`READ_IDLE_TIMEOUT`] for bytes (slowloris defence), and the
/// whole connection is closed once [`CONNECTION_DEADLINE`] elapses (a peer that
/// dribbles just inside every idle window still cannot pin the task forever).
async fn read_lines(stream: UnixStream, cfg: Config, tx: mpsc::Sender<Overlay>) {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 8192];
    let deadline = tokio::time::Instant::now() + CONNECTION_DEADLINE;

    loop {
        // The next read may wait at most until the idle timeout or the overall
        // connection deadline, whichever is sooner; on elapse the connection is
        // closed (no bytes are ever logged).
        let idle_deadline = tokio::time::Instant::now() + READ_IDLE_TIMEOUT;
        match tokio::time::timeout_at(idle_deadline.min(deadline), stream.readable()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                warn!(kind = %err.kind(), "ingest: readable() failed; closing connection");
                break;
            }
            Err(_) => {
                warn!("ingest: connection idle/deadline exceeded; closing");
                break;
            }
        }
        match stream.try_read(&mut chunk) {
            Ok(0) => break, // peer closed
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if drain_lines(&mut buf, &cfg, &tx).await.is_break() {
                    break;
                }
                // A frame that never terminates must not grow unbounded; close the
                // connection rather than silently resyncing a hostile stream
                // (FR-7/AC-2). The bytes are never logged (FR-8/AC-4).
                if buf.len() as u64 > MAX_LINE_BYTES {
                    warn!("ingest: oversized unterminated frame; closing connection");
                    break;
                }
            }
            // `try_read` after `readable` can still report WouldBlock spuriously.
            Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                // Never log the bytes — only the io category (FR-8/AC-4).
                warn!(kind = %err.kind(), "ingest: read error; closing connection");
                break;
            }
        }
    }
}

/// Split complete `\n`-terminated lines out of `buf`, dispatching each; leaves an
/// unterminated trailing partial in `buf`. Returns
/// [`std::ops::ControlFlow::Break`] only when the run loop has gone away.
async fn drain_lines(
    buf: &mut Vec<u8>,
    cfg: &Config,
    tx: &mpsc::Sender<Overlay>,
) -> std::ops::ControlFlow<()> {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        // The bytes are decoded here only to parse; on invalid UTF-8 the line is
        // dropped without logging its contents.
        if let Ok(text) = std::str::from_utf8(&line) {
            if process_line(text, cfg, tx).await.is_break() {
                return std::ops::ControlFlow::Break(());
            }
        } else {
            debug!("ingest: dropping non-utf8 line");
        }
    }
    std::ops::ControlFlow::Continue(())
}

/// Parse + sanitize one line and forward its overlay. Returns
/// [`std::ops::ControlFlow::Break`] only when the run loop has gone away.
async fn process_line(
    line: &str,
    cfg: &Config,
    tx: &mpsc::Sender<Overlay>,
) -> std::ops::ControlFlow<()> {
    match IngestEvent::parse_line(line) {
        Ok(Some(event)) => {
            if let Some(overlay) = event.overlay(cfg) {
                // Log only the sanitized summary, never the raw line (FR-8/AC-4).
                debug!(summary = %overlay.log_summary(), "ingest: overlay");
                if tx.send(overlay).await.is_err() {
                    return std::ops::ControlFlow::Break(());
                }
            }
        }
        Ok(None) => {}
        Err(_) => {
            // A malformed frame is dropped with a generic note — the bytes that
            // failed to parse are never emitted (they may contain a payload).
            debug!("ingest: dropping unparseable line");
        }
    }
    std::ops::ControlFlow::Continue(())
}

/// Read the peer's uid from a connected stream via `getpeereid(2)`.
///
/// `nix::unistd::getpeereid` needs the (unenabled) `socket` feature, so the libc
/// symbol is bound directly — mirroring the self-contained `flock` FFI in
/// `lib.rs`. macOS' `getpeereid` returns the credentials of the peer at
/// `connect` time, which is exactly what the same-uid check needs.
fn peer_uid<F: AsRawFd>(stream: &F) -> std::io::Result<u32> {
    extern "C" {
        fn getpeereid(fd: i32, uid: *mut u32, gid: *mut u32) -> i32;
    }
    let mut uid: u32 = u32::MAX;
    let mut gid: u32 = u32::MAX;
    let rc = unsafe { getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc == 0 {
        Ok(uid)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Forward all of stdin to the daemon socket and return — the `forward` CLI body.
///
/// Synchronous and dependency-free (this is a short-lived CLI, not the daemon):
/// reads stdin to EOF, connects to [`socket_path`], writes the bytes
/// newline-terminated, and flushes. **Never fails the caller** (FR-4/AC-3): a
/// missing/refused socket, an unresolvable path, or a write error is swallowed and
/// reported as `Ok(())` so a down daemon can never fail the hook/tool call.
pub fn forward_stdin() -> Result<()> {
    use std::io::Read as _;

    let mut payload = Vec::new();
    if std::io::stdin().read_to_end(&mut payload).is_err() {
        // Could not even read stdin — still never fail the tool call.
        return Ok(());
    }
    // Best-effort: any failure to deliver is intentionally non-fatal.
    let _ = deliver(&payload);
    Ok(())
}

/// Connect to the daemon socket and write `payload` newline-terminated.
///
/// Split out from [`forward_stdin`] so a test can exercise the round-trip against
/// a temp socket. Returns the io error so the test can assert success; the CLI
/// caller discards it (FR-4/AC-3).
fn deliver(payload: &[u8]) -> std::io::Result<()> {
    let path = socket_path()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "socket path unresolved"))?;
    deliver_to(&path, payload)
}

/// Write `payload` (newline-terminated) to the socket at `path`.
fn deliver_to(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let mut stream = StdUnixStream::connect(path)?;
    // Bound the write so a wedged daemon reader (overlay channel full because the
    // run loop stalled) can never block the Claude Code hot path; a timeout is a
    // non-fatal abandon, swallowed by `forward_stdin` (C-6, FR-4/AC-3).
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(200)));
    stream.write_all(payload)?;
    if !payload.ends_with(b"\n") {
        stream.write_all(b"\n")?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn temp_sock() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cp-ingest-{}-{nanos}.sock", std::process::id()))
    }

    /// Bind, or `None` when the sandbox forbids AF_UNIX `bind()` (`EPERM`).
    ///
    /// Some CI/test sandboxes deny binding a unix socket from inside the test
    /// runner even though the real daemon process can bind fine. The bind-
    /// dependent tests below short-circuit (skip) in that case rather than fail a
    /// behaviour the environment — not the code — makes untestable. Any other bind
    /// error is a real defect and still panics.
    fn try_bind(path: &Path) -> Option<UnixListener> {
        match bind(path) {
            Ok(listener) => Some(listener),
            Err(Error::Io(err))
                if err.raw_os_error() == Some(libc_eperm())
                    || err.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                eprintln!("skipping: sandbox forbids AF_UNIX bind ({err})");
                None
            }
            Err(other) => panic!("bind failed unexpectedly: {other:?}"),
        }
    }

    fn libc_eperm() -> i32 {
        1 // EPERM on macOS and Linux.
    }

    /// A full-size connection-cap semaphore for `accept_loop` in tests that do not
    /// exercise the cap itself.
    fn test_limit() -> Arc<Semaphore> {
        Arc::new(Semaphore::new(MAX_INFLIGHT_CONNECTIONS))
    }

    #[test]
    fn forward_is_non_failing_when_socket_absent() {
        // FR-4/AC-3: with no daemon listening, delivery fails internally but
        // `deliver` surfaces the io error (so the test sees it) while the public
        // `forward_stdin` would swallow it.
        let path = temp_sock();
        assert!(!path.exists());
        let err = deliver_to(&path, b"{\"kind\":\"hook\"}");
        assert!(err.is_err(), "no listener → connect refused/not-found");
    }

    #[test]
    fn deliver_to_returns_promptly_without_panic_when_absent() {
        // FR-4/AC-3 + C-6: a missing socket must return at once (connect refused)
        // and never hang or panic the caller. No live daemon is involved.
        let path = temp_sock();
        assert!(!path.exists());
        let start = std::time::Instant::now();
        let result = deliver_to(&path, b"{\"kind\":\"hook\"}");
        // Connect to a non-existent socket fails fast; the bounded write timeout
        // (200ms) is the absolute ceiling even on a slow box.
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "delivery to an absent socket must not block the hot path"
        );
        assert!(result.is_err(), "no listener → connect refused/not-found");
    }

    #[tokio::test]
    async fn forward_round_trip_emits_overlay() {
        // A statusLine line written via the forwarder must arrive as an overlay.
        let path = temp_sock();
        let Some(listener) = try_bind(&path) else {
            return;
        };
        let own_uid = nix::unistd::Uid::current().as_raw();
        let (tx, mut rx) = mpsc::channel::<Overlay>(8);

        let server = tokio::spawn(async move {
            accept_loop(listener, Config::default(), own_uid, tx, test_limit()).await;
        });

        // Write from a blocking thread, mirroring the real CLI path.
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || {
            deliver_to(
                &write_path,
                br#"{"kind":"statusline","session_id":"s1","model":"Opus 4.8","cost_usd":0.5,"ctx_pct":3.0}"#,
            )
            .expect("deliver");
        })
        .await
        .unwrap();

        let overlay = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("overlay within timeout")
            .expect("overlay present");
        assert_eq!(overlay.session_id, "s1");
        assert_eq!(overlay.cost_usd, Some(0.5));
        assert_eq!(overlay.ctx_pct, Some(3.0));
        assert_eq!(overlay.model.as_deref(), Some("Opus 4.8"));

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn round_trip_never_logs_raw_payload() {
        // A hook line carrying a fake secret must arrive only as a sanitized
        // overlay whose summary omits the payload (FR-8/AC-4).
        let path = temp_sock();
        let Some(listener) = try_bind(&path) else {
            return;
        };
        let own_uid = nix::unistd::Uid::current().as_raw();
        let (tx, mut rx) = mpsc::channel::<Overlay>(8);

        let server = tokio::spawn(async move {
            accept_loop(listener, Config::default(), own_uid, tx, test_limit()).await;
        });

        let write_path = path.clone();
        tokio::task::spawn_blocking(move || {
            deliver_to(
                &write_path,
                br#"{"kind":"hook","event":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"echo sk-FAKE-SECRET-123"}}"#,
            )
            .expect("deliver");
        })
        .await
        .unwrap();

        let overlay = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("overlay")
            .expect("present");
        let summary = overlay.log_summary();
        assert!(!summary.contains("sk-FAKE-SECRET"), "{summary}");
        let activity = overlay.activity.expect("activity");
        // Bash args dropped by default → only the program token survives.
        assert_eq!(activity.target.as_deref(), Some("echo"));

        server.abort();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bind_sets_socket_0600() {
        let path = temp_sock();
        let Some(listener) = try_bind(&path) else {
            return;
        };
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "daemon.sock must be 0600");
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }
}
