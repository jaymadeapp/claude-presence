//! Logging setup: `tracing-subscriber` + a rotating file appender.
//!
//! # Privacy contract (FR-8/AC-4, NFR-3, C-7)
//!
//! Logs are as sensitive as the Discord card: they MUST NOT contain raw
//! `tool_input`, prompt/transcript text, full filesystem paths, or unredacted
//! statusline JSON — only the same sanitized summaries that are emitted to
//! Discord. This module owns the *sink*, not the *content*: it cannot inspect
//! every value a call site passes. The guarantee is therefore split:
//!
//! * **Here** — the sink writes nowhere but a `0600` file inside a `0700`
//!   directory owned by the current user, and this module itself logs no
//!   sensitive data.
//! * **At every call site** — code MUST pass only sanitized fields. Paths are
//!   reduced to basenames, bash arguments are dropped/scrubbed, and raw
//!   payloads are summarized *before* they reach a `tracing` macro. Run any
//!   user-derived string through `crate::privacy` first; never log a
//!   `tool_input`, a transcript line, or a statusline body verbatim.
//!
//! The file appender rotates daily (`tracing-appender`), so old logs age out
//! and the daemon satisfies the "rotating logs" requirement (FR-8/AC-1).
//!
//! `init` is the public entry point, wired in by the `run` command.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::error::{Error, Result};

/// Permissions for the state/log directory: owner-only `rwx` (0700).
const DIR_MODE: u32 = 0o700;
/// Permissions for log files: owner-only `rw` (0600).
const FILE_MODE: u32 = 0o600;
/// Base name of the rotating log file (a date suffix is appended per rotation).
const LOG_FILE_PREFIX: &str = "claude-presence.log";
/// Fallback filter when `RUST_LOG` is unset.
const DEFAULT_FILTER: &str = "info";

/// Initialize the global tracing subscriber.
///
/// Sets up an [`EnvFilter`] (honoring `RUST_LOG`, defaulting to `info`) and a
/// non-blocking, daily-rotating file appender under the `0700` state directory,
/// with each log file created `0600`. Returns the appender's [`WorkerGuard`],
/// which the caller MUST keep alive for the lifetime of the process — dropping
/// it flushes and stops the background writer, so buffered lines would be lost.
///
/// Callers are responsible for the privacy contract documented at the module
/// level: only sanitized summaries may be logged.
pub fn init() -> Result<WorkerGuard> {
    let log_dir = state_log_dir()?;
    ensure_dir_0700(&log_dir)?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .build(&log_dir)
        .map_err(|e| Error::Other(format!("could not build log appender: {e}")))?;

    // `build` opens the current dated file eagerly (with the process umask
    // deciding the mode), so it already exists here. Pin it — and any rotated
    // siblings — to 0600 before any sensitive line could be written. On the next
    // daily rotation the appender re-opens with O_APPEND; the enclosing 0700 dir
    // keeps a freshly rotated file unreadable to other users regardless.
    harden_logs_0600(&log_dir);

    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .try_init()
        .map_err(|e| Error::Other(format!("could not init tracing subscriber: {e}")))?;

    Ok(guard)
}

/// Resolve `~/.local/state/claude-presence/logs`.
///
/// `directories::ProjectDirs::state_dir()` is `None` on macOS, so the home
/// directory is resolved via `BaseDirs` and the XDG-style state path is built
/// explicitly — matching the daemon socket location in design §4.1.
fn state_log_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or(Error::PathResolution("home"))?;
    Ok(base
        .home_dir()
        .join(".local")
        .join("state")
        .join("claude-presence")
        .join("logs"))
}

/// Create `dir` (and parents) with mode `0700`, and normalize the mode if it
/// already exists with looser permissions.
fn ensure_dir_0700(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(DIR_MODE)
            .create(dir)?;
    }
    // `recursive(true)` applies the mode only to freshly created leaf
    // components; an already-present dir (or parents) may be looser, so pin it.
    let perms = fs::Permissions::from_mode(DIR_MODE);
    fs::set_permissions(dir, perms)?;
    Ok(())
}

/// Force every log file (those whose name starts with [`LOG_FILE_PREFIX`]) in
/// `dir` to `0600`.
///
/// The rolling appender opens files with the process umask, so we re-pin the
/// mode after it has created the current dated file. Errors are swallowed:
/// hardening is best-effort and must never take down the daemon.
fn harden_logs_0600(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_log = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(LOG_FILE_PREFIX));
        if is_log {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A unique scratch directory under the system temp dir. No external
    /// dev-dependency (tempfile) is declared, so this is hand-rolled.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cp-logging-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mode_of(path: &std::path::Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_dir_creates_with_0700() {
        let base = scratch_dir("mkdir");
        let nested = base.join("a").join("b").join("logs");
        ensure_dir_0700(&nested).unwrap();
        assert!(nested.is_dir());
        assert_eq!(mode_of(&nested), DIR_MODE, "leaf dir must be 0700");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn ensure_dir_tightens_existing_loose_dir() {
        let base = scratch_dir("tighten");
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir_0700(&base).unwrap();
        assert_eq!(
            mode_of(&base),
            DIR_MODE,
            "existing dir must be re-pinned to 0700"
        );
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn harden_pins_log_files_to_0600_and_skips_others() {
        let dir = scratch_dir("harden");
        let log = dir.join(format!("{LOG_FILE_PREFIX}.2026-06-21"));
        let other = dir.join("not-a-log.txt");
        fs::write(&log, b"line").unwrap();
        fs::write(&other, b"x").unwrap();
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o644)).unwrap();

        harden_logs_0600(&dir);

        assert_eq!(mode_of(&log), FILE_MODE, "log file must be 0600");
        assert_eq!(mode_of(&other), 0o644, "non-log files are left untouched");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn harden_is_silent_on_missing_dir() {
        // Must not panic when the directory does not exist (best-effort).
        harden_logs_0600(std::path::Path::new("/nonexistent/cp-logging-xyz"));
    }

    #[test]
    fn state_log_dir_is_under_local_state() {
        let dir = state_log_dir().unwrap();
        assert!(
            dir.ends_with(".local/state/claude-presence/logs"),
            "got {dir:?}"
        );
    }
}
