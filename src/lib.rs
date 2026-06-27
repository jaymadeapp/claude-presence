//! `claude-presence` — a macOS daemon that aggregates live Claude Code activity
//! into a single Discord Rich Presence card.
//!
//! The crate exposes a **library target** alongside the `claude-presence` binary
//! so integration tests (`tests/`) can exercise the real public API as
//! `claude_presence::…` rather than recompiling modules via `#[path]`. The binary
//! (`src/main.rs`) is a thin clap dispatcher over these modules; the daemon
//! orchestration entry point is [`run`].
//!
//! Module map (design §3): collectors live under [`claude`] (process/transcript)
//! and [`ingest`] (statusline/hook socket); [`state`] aggregates them into one
//! [`state::model::PresenceModel`]; [`discord`] drives the single IPC presence;
//! [`install`] owns the reversible launchd/hooks/statusline wiring; [`privacy`]
//! and [`logging`] enforce the sanitize-everything contract (C-7, FR-8/AC-4).

pub mod claude;
pub mod config;
pub mod discord;
pub mod error;
pub mod ingest;
pub mod install;
pub mod logging;
pub mod platform;
pub mod privacy;
pub mod state;
#[cfg(feature = "tray")]
pub mod tray;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::claude::sessions::{self, LiveSession};
use crate::claude::transcript::{self, DerivedState, SessionWatcher};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::ingest::events::Overlay;
use crate::state::aggregator::aggregate_channel;
use crate::state::model::SessionState;

/// How often the discovery loop re-enumerates the live session set.
///
/// Per-session *activity* is event-driven through each transcript's `notify`
/// watcher (FR-2/AC-1); this interval only governs how quickly a newly-started
/// or just-exited top-level session appears/disappears from the card. A few
/// seconds keeps idle CPU negligible (NFR-1) while staying responsive.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(3);

/// Boot the daemon and run it in the foreground until SIGINT/SIGTERM (FR-8).
///
/// Pipeline (design §1, MVP = sessions + transcript only, FR-5/AC-1):
/// 1. init logging (the [`tracing_appender::non_blocking::WorkerGuard`] is held
///    for the whole run so buffered lines flush);
/// 2. acquire the single-instance lock (`flock` on a lock file in the `0700`
///    state dir); a second live instance exits clearly with
///    [`Error::AlreadyRunning`] (FR-8/AC-1 — two writers would breach the
///    Discord 5/20s rate limit);
/// 3. load [`Config`];
/// 4. spawn the discovery + transcript collector loop publishing a
///    `watch::Receiver<Vec<SessionState>>`;
/// 5. [`aggregate_channel`] → [`crate::discord::sink::run_sink`];
/// 6. wait for a termination signal, flip the sink's `shutdown` watch to `true`
///    so it clears the Discord presence, and `join` the sink thread before
///    returning (FR-8/AC-3: graceful shutdown clears presence).
///
/// Runs on the multi-threaded tokio runtime established by `#[tokio::main]`.
pub async fn run() -> Result<()> {
    // (1) Logging first so every later step is captured; keep the guard alive
    // for the whole function (dropping it stops the background log writer).
    let _log_guard = logging::init()?;
    info!("claude-presence: starting");

    // (2) Single-instance lock. Held for the process lifetime; the lock releases
    // when `_instance_lock` (and thus its fd) is dropped at the end of `run`.
    let _instance_lock = acquire_single_instance_lock()?;

    // (3) Config (safe defaults on any failure — FR-7/AC-3).
    let cfg = Config::load();

    // (3b) Ingest socket (the statusline + hooks push path, FR-3/FR-4). A bind
    // failure must not sink the daemon — degrade to the JSONL-only MVP and log,
    // never crash (FR-5/AC-1). Its sanitized overlays are folded into the session
    // set by the collector loop below.
    let overlays = match ingest::socket::serve(cfg.clone()) {
        Ok(server) => {
            info!("claude-presence: ingest socket up");
            Some(server)
        }
        Err(err) => {
            warn!(%err, "ingest socket unavailable; running JSONL-only");
            None
        }
    };
    let (ingest_handle, overlay_rx) = match overlays {
        Some(server) => (Some(server.handle), Some(server.overlays)),
        None => (None, None),
    };

    // (4) Collector loop → live session set (with ingest overlays folded in).
    let (collector_handle, sessions_rx) = spawn_collectors(cfg.clone(), overlay_rx);

    // (5) Aggregator → debounced presence stream → Discord sink on its own thread.
    let presence_rx = aggregate_channel(sessions_rx, cfg.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let sink_handle = discord::sink::run_sink(presence_rx, cfg, shutdown_rx);
    info!("claude-presence: pipeline up; awaiting termination signal");

    // (6) Block until SIGINT/SIGTERM, then tear down cleanly.
    wait_for_terminate().await;
    info!("claude-presence: shutdown requested; clearing presence");

    // Flip the sink's shutdown watch so it clears the Discord presence, then join
    // the dedicated sink thread so the clear actually completes before we exit
    // (FR-8/AC-3). Joining a sync thread blocks; do it off the async runtime.
    let _ = shutdown_tx.send(true);
    if let Err(err) = tokio::task::spawn_blocking(move || sink_handle.join())
        .await
        .map(|_| ())
    {
        warn!(%err, "discord sink join task failed");
    }

    // Stop the collector loop (it owns the per-session transcript watchers) and
    // the ingest accept loop (it owns the daemon socket).
    collector_handle.abort();
    if let Some(handle) = ingest_handle {
        handle.abort();
    }

    info!("claude-presence: stopped");
    Ok(())
}

/// A held single-instance lock. Owns the locked file descriptor; dropping it
/// closes the fd, which the kernel uses to release the advisory `flock` (and it
/// is released on process exit regardless).
#[derive(Debug)]
pub struct InstanceLock {
    _fd: OwnedFd,
}

// `flock(2)` is the POSIX/BSD advisory whole-file lock. `nix`'s safe wrapper is
// gated behind its `fs` feature (not enabled here, and `Cargo.toml` is owned by
// other tasks), so we bind the libc symbol directly with a tiny self-contained
// FFI block rather than pull in an extra dependency. `LOCK_EX | LOCK_NB` takes a
// non-blocking exclusive lock; on contention it fails with `EWOULDBLOCK`.
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// Acquire the daemon's single-instance lock on the state dir's lock file
/// (FR-8/AC-1).
///
/// Resolves `~/.local/state/claude-presence`, ensures it exists `0700`, opens
/// `daemon.lock` (created `0600`), and takes a **non-blocking exclusive** `flock`.
/// A second live instance fails the non-blocking lock with `EWOULDBLOCK` and is
/// reported as [`Error::AlreadyRunning`] so it exits clearly instead of becoming
/// a second Discord writer.
pub fn acquire_single_instance_lock() -> Result<InstanceLock> {
    let state_dir = state_dir()?;
    ensure_dir_0700(&state_dir)?;
    let lock_path = state_dir.join("daemon.lock");
    lock_file(&lock_path)
}

/// Open `lock_path` `0600` and take a non-blocking exclusive `flock`, mapping
/// contention to [`Error::AlreadyRunning`]. Split from
/// [`acquire_single_instance_lock`] so it is unit-testable against a temp path.
fn lock_file(lock_path: &Path) -> Result<InstanceLock> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path)?;
    let fd: OwnedFd = file.into();

    // Non-blocking exclusive lock: a live second instance fails immediately
    // rather than blocking. The fd is held in `InstanceLock` for the process
    // lifetime; closing it (on drop/exit) releases the advisory lock.
    let rc = unsafe { flock(fd.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        return Ok(InstanceLock { _fd: fd });
    }
    let errno = std::io::Error::last_os_error();
    match errno.raw_os_error() {
        // EWOULDBLOCK == EAGAIN (11 on Linux, 35 on macOS): the lock is held by
        // another live instance (FR-8/AC-1).
        Some(code)
            if errno.kind() == std::io::ErrorKind::WouldBlock || code == 11 || code == 35 =>
        {
            Err(Error::AlreadyRunning)
        }
        _ => Err(Error::Other(format!("could not lock daemon.lock: {errno}"))),
    }
}

/// Resolve `~/.local/state/claude-presence` (the `0700` state dir, design §4.1).
///
/// `directories::ProjectDirs::state_dir()` is `None` on macOS, so the XDG-style
/// state path is built explicitly from the home dir, matching `logging.rs`.
fn state_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or(Error::PathResolution("home"))?;
    Ok(base
        .home_dir()
        .join(".local")
        .join("state")
        .join("claude-presence"))
}

/// Create `dir` (and parents) `0700`, tightening an existing looser dir.
fn ensure_dir_0700(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The collector loop's task handle plus the live-session watch channel it feeds.
type CollectorHandle = tokio::task::JoinHandle<()>;

/// Spawn the discovery + transcript collector loop (FR-1, FR-2 → FR-5/AC-1).
///
/// Returns the loop's [`JoinHandle`](tokio::task::JoinHandle) and a
/// `watch::Receiver<Vec<SessionState>>` carrying the merged live-session set the
/// aggregator consumes. The loop:
/// - re-enumerates live sessions every [`DISCOVERY_INTERVAL`] (FR-1);
/// - keeps one [`SessionWatcher`] per session whose transcript drives activity /
///   model / tokens / busy / subagents via `notify` events (FR-2/AC-1, event-
///   driven where the FS allows; the discovery interval only adds/removes the
///   top-level session itself);
/// - folds each session + its latest [`DerivedState`] into a [`SessionState`] and
///   publishes the set on change.
fn spawn_collectors(
    cfg: Config,
    mut overlay_rx: Option<tokio::sync::mpsc::Receiver<Overlay>>,
) -> (CollectorHandle, watch::Receiver<Vec<SessionState>>) {
    let (tx, rx) = watch::channel::<Vec<SessionState>>(Vec::new());

    let handle = tokio::spawn(async move {
        let mut watchers: std::collections::HashMap<String, SessionTracker> =
            std::collections::HashMap::new();
        // Latest ingest overlay per session, persisted across discovery ticks so
        // a statusLine's exact cost/ctx%/model (and a hook's activity) stays on
        // the card until the next push for that session (FR-3, FR-4/AC-2).
        let mut overlays: std::collections::HashMap<String, Overlay> =
            std::collections::HashMap::new();
        let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let live = match sessions::discover() {
                Ok(live) => live,
                Err(err) => {
                    // A discovery failure (e.g. unresolvable ~/.claude) must not
                    // kill the loop — degrade to "no sessions" and retry (NFR-2).
                    warn!(%err, "session discovery failed; treating as no sessions");
                    Vec::new()
                }
            };

            reconcile_watchers(&mut watchers, &live, &cfg);
            // Drop overlays for sessions that are no longer live so a stale push
            // can never resurrect a dead session.
            prune_overlays(&mut overlays, &live);
            let snapshot = build_snapshot(&watchers, &live, &overlays);

            // Publish only on a meaningful change so the aggregator debounces
            // naturally; equality is structural over the card-relevant fields
            // (including the overlay-driven cost/ctx%/activity, so a push
            // republishes — FR-3, FR-4/AC-2).
            if !sessions_eq(&tx.borrow(), &snapshot) && tx.send(snapshot).is_err() {
                // Aggregator dropped the receiver → nothing to feed; stop.
                break;
            }

            // Re-enumerate on the discovery tick OR the instant an ingest overlay
            // arrives, so a `PreToolUse` "Running X" / statusLine update lands
            // immediately rather than at the next 3s tick (FR-4/AC-2).
            match overlay_rx.as_mut() {
                Some(rx) => {
                    tokio::select! {
                        _ = ticker.tick() => {}
                        recv = rx.recv() => match recv {
                            Some(overlay) => {
                                overlays.insert(overlay.session_id.clone(), overlay);
                                // Coalesce any other immediately-pending overlays
                                // before re-aggregating, to batch a hook burst.
                                while let Ok(more) = rx.try_recv() {
                                    overlays.insert(more.session_id.clone(), more);
                                }
                            }
                            // Ingest server gone → stop draining overlays but keep
                            // the JSONL-only loop running on the timer alone.
                            None => overlay_rx = None,
                        }
                    }
                }
                None => {
                    ticker.tick().await;
                }
            }
        }
        debug!("collector loop exited");
    });

    (handle, rx)
}

/// Drop overlays whose session is no longer in the live set so a stale ingest
/// push cannot keep a dead session on the card.
fn prune_overlays(overlays: &mut std::collections::HashMap<String, Overlay>, live: &[LiveSession]) {
    let live_ids: std::collections::HashSet<&str> =
        live.iter().map(|s| s.session_id.as_str()).collect();
    overlays.retain(|id, _| live_ids.contains(id.as_str()));
}

/// One live session and its transcript watcher, kept across discovery ticks so
/// the `notify` watcher is not torn down and recreated each interval.
struct SessionTracker {
    session: LiveSession,
    watcher: Option<SessionWatcher>,
}

/// Add watchers for newly-seen sessions, refresh the cached [`LiveSession`] for
/// existing ones (cwd/branch/start can change), and drop watchers for sessions
/// that are no longer live (FR-1).
fn reconcile_watchers(
    watchers: &mut std::collections::HashMap<String, SessionTracker>,
    live: &[LiveSession],
    cfg: &Config,
) {
    let live_ids: std::collections::HashSet<&str> =
        live.iter().map(|s| s.session_id.as_str()).collect();
    watchers.retain(|id, _| live_ids.contains(id.as_str()));

    for session in live {
        match watchers.get_mut(&session.session_id) {
            Some(tracker) => {
                tracker.session = session.clone();
                // Retry the attach for a session that was tracked registry-only
                // because its transcript did not exist yet on an earlier tick
                // (the registry file is written ~1–2s before the first transcript
                // line). Without this, such a session never gains a watcher, so
                // its tokens_total stays None and silently drops out of the
                // combined multi-session total even though it still counts toward
                // live_count.
                if tracker.watcher.is_none() {
                    tracker.watcher = attach_watcher(session, cfg);
                }
            }
            None => {
                // Attach a transcript watcher when the transcript exists; a
                // brand-new session without a transcript yet is still tracked
                // (registry-only view) and gains a watcher on a later tick.
                let watcher = attach_watcher(session, cfg);
                watchers.insert(
                    session.session_id.clone(),
                    SessionTracker {
                        session: session.clone(),
                        watcher,
                    },
                );
            }
        }
    }
}

/// Attach a transcript watcher for `session` when discovery has resolved its
/// `<sessionId>.jsonl` path, logging and degrading to `None` on a watcher-start
/// error (NFR-2). Returns `None` (no log) when the transcript is not present yet
/// (`session.transcript` is `None`) — the caller retries on a later discovery
/// tick once discovery resolves it (FR-1/AC-3).
fn attach_watcher(session: &LiveSession, cfg: &Config) -> Option<SessionWatcher> {
    let path = session.transcript.clone()?;
    match transcript::watch_session(path, cfg.clone()) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            warn!(%err, "could not start transcript watcher");
            None
        }
    }
}

/// Fold the tracked sessions into the [`SessionState`] vector the aggregator
/// consumes, layering each transcript's latest [`DerivedState`] over the
/// registry-derived [`LiveSession`] (FR-5/AC-1).
fn build_snapshot(
    watchers: &std::collections::HashMap<String, SessionTracker>,
    live: &[LiveSession],
    overlays: &std::collections::HashMap<String, Overlay>,
) -> Vec<SessionState> {
    live.iter()
        .map(|session| {
            let derived = watchers
                .get(&session.session_id)
                .and_then(|t| t.watcher.as_ref())
                .map(|w| w.current())
                .unwrap_or_default();
            let mut state = to_session_state(session, &derived);
            // Layer the latest ingest overlay over the transcript-derived state:
            // statusLine overrides cost/ctx%/model; a hook sets/clears activity
            // and busy (FR-3, FR-4/AC-2).
            if let Some(overlay) = overlays.get(&session.session_id) {
                overlay.apply_to(&mut state);
            }
            state
        })
        .collect()
}

/// Build a [`SessionState`] from a registry [`LiveSession`] and its transcript
/// [`DerivedState`].
///
/// Cost/ctx% are computed as a **transcript fallback** here (FR-3/AC-3) from the
/// latest request's usage × pricing and live-context tokens. A statusLine push
/// still OVERRIDES them later via [`Overlay::apply_to`], which only writes those
/// fields when it carries `Some` — so the exact Anthropic figures win when
/// available, and the card is never blank when they are not (FR-5/AC-1).
fn to_session_state(session: &LiveSession, derived: &DerivedState) -> SessionState {
    let started_at = epoch_ms_to_system_time(session.started_at);

    // ctx% fallback: live-context tokens over the model's effective window.
    let ctx_pct = derived
        .context_tokens
        .zip(derived.model.as_deref())
        .map(|(ctx, model)| {
            crate::claude::pricing::ctx_pct(
                ctx,
                crate::claude::pricing::effective_context_window(model, None),
            )
        });

    // cost fallback: latest request usage × per-model pricing.
    let cost_usd = derived
        .usage
        .as_ref()
        .zip(derived.model.as_deref())
        .map(|(usage, model)| crate::claude::pricing::cost(model, usage));

    SessionState {
        session_id: session.session_id.clone(),
        pid: session.pid,
        project: session.project_name.clone(),
        cwd: session.cwd.clone(),
        // Branch: prefer the transcript-derived value (FR-2/AC-3), fall back to
        // whatever the registry/discovery carried.
        branch: derived.branch.clone().or_else(|| session.branch.clone()),
        model: derived.model.clone(),
        started_at,
        // Focus recency: the newest transcript line's timestamp (or the file
        // mtime fallback baked into `derived.last_event` by the watcher), so
        // focus orders by activity, not start time (FR-5/AC-1, AC-4).
        last_active: derived.last_event.unwrap_or(started_at),
        busy: derived.busy,
        working: derived.working,
        activity: derived.activity.clone(),
        title: derived.title.clone(),
        cost_usd,
        ctx_pct,
        tokens_total: derived.tokens_total,
        subagents: derived.subagents,
        subagent_tokens: derived.subagent_tokens,
    }
}

/// Convert a registry epoch-**milliseconds** start time to a [`SystemTime`],
/// falling back to "now" when absent so the elapsed timer is at worst zeroed,
/// never wrong by 1000× (FR-5/AC-4).
fn epoch_ms_to_system_time(started_at_ms: Option<i64>) -> SystemTime {
    match started_at_ms {
        Some(ms) if ms >= 0 => SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64),
        _ => SystemTime::now(),
    }
}

/// Structural equality of two session sets over the fields that drive the card,
/// so the collector publishes only on a meaningful change.
fn sessions_eq(a: &[SessionState], b: &[SessionState]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(x, y)| {
        x.session_id == y.session_id
            && x.project == y.project
            && x.branch == y.branch
            && x.model == y.model
            && x.busy == y.busy
            && x.working == y.working
            && x.tokens_total == y.tokens_total
            && x.subagents == y.subagents
            && x.subagent_tokens == y.subagent_tokens
            // Include the ingest-overlay-driven fields so a statusLine cost/ctx%
            // update or a hook activity change actually republishes (FR-3, FR-4).
            && f64_eq(x.cost_usd, y.cost_usd)
            && f64_eq(x.ctx_pct, y.ctx_pct)
            && activity_eq(x.activity.as_ref(), y.activity.as_ref())
    })
}

/// Bit-exact equality of two optional `f64`s for change detection.
///
/// Overlay cost/ctx% values are copied verbatim from the statusLine push (never
/// recomputed), so a bit-exact compare is the right "did this change" test here;
/// `NaN` (never produced on this path) compares unequal, which only forces a
/// harmless extra republish.
fn f64_eq(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Structural equality of two optional [`Activity`](state::model::Activity)
/// values (the type does not derive `PartialEq`).
fn activity_eq(a: Option<&state::model::Activity>, b: Option<&state::model::Activity>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.verb == b.verb && a.target == b.target && a.small_image_key == b.small_image_key
        }
        _ => false,
    }
}

/// Await a termination signal (SIGINT or SIGTERM), returning when either fires.
///
/// Uses tokio's `signal` feature, which installs a **process-wide** signal
/// handler, so the graceful-shutdown path (clear the Discord presence, then exit
/// 0) runs no matter which runtime thread the signal is delivered to. A previous
/// implementation used `pthread_sigmask` from inside the running runtime, which
/// only blocks the *calling* thread — the other tokio worker threads (spawned
/// before the block) still took the signal's default action and terminated the
/// process with code 143 before the presence was ever cleared (FR-8/AC-3).
async fn wait_for_terminate() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(err) => {
            // SIGTERM handler unavailable: fall back to SIGINT (Ctrl-C) alone
            // rather than failing to shut down at all.
            warn!(%err, "could not install SIGTERM handler; waiting on SIGINT only");
            let _ = tokio::signal::ctrl_c().await;
            debug!("SIGINT received");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(err) => {
            warn!(%err, "could not install SIGINT handler; waiting on SIGTERM only");
            sigterm.recv().await;
            debug!("SIGTERM received");
            return;
        }
    };

    tokio::select! {
        _ = sigterm.recv() => debug!("SIGTERM received"),
        _ = sigint.recv() => debug!("SIGINT received"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cp-run-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn single_instance_lock_acquires_then_contends() {
        let dir = unique_dir("lock");
        let lock_path = dir.join("daemon.lock");

        // First acquire succeeds and creates the 0600 lock file.
        let held = lock_file(&lock_path).expect("first lock acquires");
        assert!(lock_path.exists());

        // A second non-blocking acquire while the first is held must report
        // AlreadyRunning (FR-8/AC-1) rather than blocking or succeeding.
        match lock_file(&lock_path) {
            Err(Error::AlreadyRunning) => {}
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }

        // Releasing the first lock lets a fresh acquire succeed again.
        drop(held);
        let reacquired = lock_file(&lock_path).expect("re-acquire after release");
        drop(reacquired);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_file_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_dir("lock-mode");
        let lock_path = dir.join("daemon.lock");
        let held = lock_file(&lock_path).expect("lock");
        let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "lock file must be 0600");
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn epoch_ms_round_trips_to_system_time() {
        let st = epoch_ms_to_system_time(Some(1_781_989_000_123));
        let ms = st
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert_eq!(ms, 1_781_989_000_123);
        // Absent/negative → "now"-ish, never a 1970 timestamp.
        let now = epoch_ms_to_system_time(None);
        assert!(now > SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    }

    #[test]
    fn to_session_state_layers_transcript_over_registry() {
        let live = LiveSession {
            pid: 4242,
            session_id: "abc-123".to_string(),
            cwd: PathBuf::from("/Users/me/Projects/demo"),
            project_name: "demo".to_string(),
            transcript: None,
            branch: Some("registry-branch".to_string()),
            started_at: Some(1_781_989_000_000),
            version: Some("2.1.181".to_string()),
        };
        let derived = DerivedState {
            model: Some("claude-opus-4-8".to_string()),
            branch: Some("feature/x".to_string()),
            busy: true,
            tokens_total: Some(1234),
            subagents: 2,
            ..DerivedState::default()
        };

        let s = to_session_state(&live, &derived);
        assert_eq!(s.session_id, "abc-123");
        assert_eq!(s.pid, 4242);
        assert_eq!(s.project, "demo");
        assert_eq!(s.cwd, PathBuf::from("/Users/me/Projects/demo"));
        // Transcript branch wins over the registry's.
        assert_eq!(s.branch.as_deref(), Some("feature/x"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert!(s.busy);
        assert_eq!(s.tokens_total, Some(1234));
        assert_eq!(s.subagents, 2);
        // No usage/context_tokens in this derived state → cost/ctx% stay None
        // (the statusLine push still overrides when present).
        assert!(s.cost_usd.is_none());
        assert!(s.ctx_pct.is_none());
        // No per-line timestamp → last_active falls back to the start time.
        assert_eq!(s.last_active, s.started_at);
    }

    #[test]
    fn to_session_state_computes_cost_and_ctx_fallback() {
        let live = LiveSession {
            pid: 1,
            session_id: "s".to_string(),
            cwd: PathBuf::from("/p"),
            project_name: "p".to_string(),
            transcript: None,
            branch: None,
            started_at: Some(1_000),
            version: None,
        };
        // Worked example from pricing.rs: Opus 4.8 @ this usage ≈ $0.4497, and
        // 70_333 live-context tokens over the 1M window ≈ 7.03%.
        let usage = crate::claude::pricing::Usage {
            input: 131,
            output: 12570,
            cache_read: 59709,
            cache_create_5m: 0,
            cache_create_1h: 10493,
        };
        let derived = DerivedState {
            model: Some("claude-opus-4-8".to_string()),
            context_tokens: Some(131 + 59709 + 10493),
            usage: Some(usage),
            ..DerivedState::default()
        };
        let s = to_session_state(&live, &derived);
        let cost = s.cost_usd.expect("cost computed from usage × pricing");
        assert!((cost - 0.449_690).abs() < 1e-5, "got ${cost}");
        let ctx = s
            .ctx_pct
            .expect("ctx% computed from context_tokens / window");
        assert!((ctx - 7.033_3).abs() < 1e-3, "got {ctx}%");
    }

    #[test]
    fn to_session_state_last_active_from_last_event() {
        let live = LiveSession {
            pid: 1,
            session_id: "s".to_string(),
            cwd: PathBuf::from("/p"),
            project_name: "p".to_string(),
            transcript: None,
            branch: None,
            started_at: Some(1_000),
            version: None,
        };
        let event = SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_989_000);
        let derived = DerivedState {
            last_event: Some(event),
            ..DerivedState::default()
        };
        let s = to_session_state(&live, &derived);
        assert_eq!(
            s.last_active, event,
            "last_active must come from last_event"
        );
        assert_ne!(s.last_active, s.started_at);
    }

    #[test]
    fn sessions_eq_detects_relevant_changes() {
        let live = LiveSession {
            pid: 1,
            session_id: "s".to_string(),
            cwd: PathBuf::from("/p"),
            project_name: "p".to_string(),
            transcript: None,
            branch: None,
            started_at: Some(1_000),
            version: None,
        };
        let base = to_session_state(&live, &DerivedState::default());

        let busy = DerivedState {
            busy: true,
            ..DerivedState::default()
        };
        let changed = to_session_state(&live, &busy);

        let base_slice = std::slice::from_ref(&base);
        assert!(sessions_eq(base_slice, base_slice));
        assert!(!sessions_eq(base_slice, std::slice::from_ref(&changed)));
        assert!(!sessions_eq(base_slice, &[]));
    }

    /// Regression for the "combined token total is too low" bug: a session may be
    /// discovered *before* its `<sessionId>.jsonl` transcript can be resolved (the
    /// registry `sessions/<PID>.json` is written ~1–2s before the first transcript
    /// line, and discovery ticks every 3s), so discovery hands `reconcile` a
    /// session with `transcript = None` and it is tracked registry-only with no
    /// watcher. Once a later tick resolves the transcript, `reconcile` MUST attach
    /// a watcher — otherwise the session counts toward `live_count` ("2 sessions")
    /// yet contributes `tokens_total = None`, so its tokens silently drop out of
    /// the multi-session combined total (observed: 116K shown for a pair whose
    /// real combined was far higher).
    #[test]
    fn watcher_attaches_on_a_later_tick_once_transcript_resolves() {
        let dir = unique_dir("late-transcript");
        let transcript = dir.join("sess-late.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"assistant\",\"message\":{\"id\":\"m1\",\"model\":\"claude-opus-4-8\",\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":120000,\"output_tokens\":50}}}\n",
        )
        .unwrap();

        let base = LiveSession {
            pid: std::process::id() as i32,
            session_id: "sess-late".to_string(),
            cwd: PathBuf::from("/Users/me/Projects/demo"),
            project_name: "demo".to_string(),
            transcript: None,
            branch: None,
            started_at: Some(1_781_989_000_000),
            version: Some("2.1.181".to_string()),
        };
        let cfg = Config::default();
        let mut watchers = std::collections::HashMap::new();

        // Tick 1 — discovery has not resolved the transcript yet: registry-only,
        // no watcher.
        reconcile_watchers(&mut watchers, std::slice::from_ref(&base), &cfg);
        assert!(
            watchers[&base.session_id].watcher.is_none(),
            "with an unresolved transcript, the session is registry-only (no watcher)"
        );

        // Tick 2 — a later discovery resolved the transcript (the file now exists).
        let resolved = LiveSession {
            transcript: Some(transcript.clone()),
            ..base.clone()
        };
        reconcile_watchers(&mut watchers, std::slice::from_ref(&resolved), &cfg);

        let attached = watchers[&base.session_id].watcher.is_some();
        let derived = watchers[&base.session_id]
            .watcher
            .as_ref()
            .map(|w| w.current());

        drop(watchers);
        std::fs::remove_dir_all(&dir).ok();

        assert!(
            attached,
            "BUG: a session whose transcript resolves on a later tick never gains a \
             watcher, so its tokens_total stays None and vanishes from the combined \
             multi-session total"
        );
        // The freshly-attached watcher derives the real per-session figure
        // (input + cache_read + output = 100 + 120000 + 50).
        assert_eq!(
            derived.and_then(|d| d.tokens_total),
            Some(120_150),
            "the attached watcher must derive this session's tokens"
        );
    }
}
