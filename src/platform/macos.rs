//! macOS platform specifics: well-known Claude/Discord paths and the PID→cwd
//! fallback used when `sysinfo` reports an empty working directory (design §3,
//! "platform/macos.rs: $TMPDIR discord sockets, lsof cwd fallback").
//!
//! # Why an `lsof` fallback instead of `libproc`
//!
//! The design table lists `libproc` (`proc_pidinfo(PROC_PIDVNODEPATHINFO)`) as
//! the cwd fallback, but the pinned `libproc 0.14` crate's `pidcwd` is a stub on
//! macOS (it returns `Err("pidcwd is not implemented for macos")`) and the crate
//! exposes no `VNodePathInfo` accessor for `vip_cdir`. The dossier (lane B1)
//! independently verified that `lsof -a -p <PID> -d cwd -Fn` resolves the cwd of
//! every live engine without root, so that is the robust fallback used here. The
//! primary path remains `sysinfo`'s `cwd()`; this is only consulted when that is
//! `None`/empty (FR-1/AC-3).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::error::{Error, Result};

/// Absolute, trusted path to `lsof` on macOS. Invoking by absolute path (rather
/// than a bare `lsof` resolved through `$PATH`) prevents a hijacked `$PATH` from
/// substituting a malicious binary (FR-8/AC-1, F28).
const LSOF_PATH: &str = "/usr/sbin/lsof";

/// Upper bound on how long the cwd `lsof` probe may run before we give up. A hung
/// `lsof` must not stall the 3s discovery tick (FR-8/AC-1, F41).
const LSOF_TIMEOUT: Duration = Duration::from_secs(2);

/// Resolve the home directory, erroring categorically if it cannot be found.
pub fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or(Error::PathResolution("home"))
}

/// The Claude Code state root, `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

/// The live-session registry directory, `~/.claude/sessions`, holding one
/// `<PID>.json` per running engine (FR-1/AC-2).
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(claude_dir()?.join("sessions"))
}

/// The transcript root, `~/.claude/projects`, with one `<project-slug>/`
/// directory per cwd (FR-1/AC-3).
pub fn projects_dir() -> Result<PathBuf> {
    Ok(claude_dir()?.join("projects"))
}

/// Resolve the per-user temporary directory Discord opens its IPC sockets in.
///
/// `gui/<uid>` launchd agents inherit the dynamic `/var/folders/...` `$TMPDIR`;
/// fall back to `/tmp` (where the Flatpak/Snap and some Discord builds place the
/// socket) when the variable is unset. The Discord sink probes
/// `discord-ipc-0..9` inside this directory (FR-6/AC-1).
pub fn tmp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Full path of a candidate Discord IPC socket, `$TMPDIR/discord-ipc-<n>`.
pub fn discord_ipc_socket(n: u8) -> PathBuf {
    tmp_dir().join(format!("discord-ipc-{n}"))
}

/// Derive Claude Code's project-slug directory name from an absolute cwd.
///
/// Verified against the live `~/.claude/projects` layout (dossier lane B1): every
/// character that is not ASCII-alphanumeric is replaced by `-`. This reproduces
/// the non-obvious cases exactly — e.g. a `/foo/.claude-worktrees/` segment maps
/// to `-foo--claude-worktrees` (the `/` and the `.` each become a dash) and
/// `qxpspace.com` maps to `qxpspace-com`. A path-separator-only `/` → `-`
/// substitution would be wrong.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// PID→cwd fallback via `/usr/sbin/lsof -a -p <PID> -d cwd -Fn` (FR-1/AC-3).
///
/// Used only when `sysinfo` reports no cwd. `-Fn` emits machine-parseable output
/// where the cwd line starts with `n` followed by the absolute path; we return
/// the first such line. `lsof` is invoked by its absolute trusted path and run
/// under a bounded wait so a hijacked `$PATH` or a hung `lsof` cannot affect or
/// stall discovery (FR-8/AC-1). Returns `None` (never an error) on any failure —
/// including a timeout — so the caller can simply skip a session whose cwd cannot
/// be resolved.
pub fn cwd_via_lsof(pid: i32) -> Option<PathBuf> {
    let stdout = run_bounded(
        LSOF_PATH,
        &["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"],
        LSOF_TIMEOUT,
    )?;
    parse_lsof_fn(&stdout)
}

/// Spawn `program args…` and wait at most `timeout` for it to exit, returning its
/// captured stdout on a successful exit. On a timeout the child is killed and
/// reaped; on any error (spawn failure, non-zero exit, or timeout) this returns
/// `None` and logs at debug. Uses only std: a watcher thread drains stdout (so a
/// full pipe can't deadlock the child) and signals completion over a channel,
/// while the parent retains the `Child` handle and `recv_timeout`s on that signal
/// — killing and reaping the child itself if it overruns the deadline.
fn run_bounded(program: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::debug!(program, %err, "failed to spawn process for cwd probe");
            return None;
        }
    };

    // We always request a piped stdout, so this is `Some` on a successful spawn;
    // if it is somehow missing, kill the child and degrade rather than risk a
    // blocking wait with no completion signal.
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };

    // Drain stdout on a watcher thread so a child writing more than a pipe buffer
    // cannot block forever, and so the bytes are ready when it exits. EOF on
    // stdout means the child closed its end (it has exited or is about to); the
    // thread then signals the parent and hands back the drained bytes. `tx` is
    // moved into the closure, so the receiver disconnects if the thread dies.
    let (tx, rx) = mpsc::channel();
    let watcher = thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let outcome = match rx.recv_timeout(timeout) {
        Ok(buf) => {
            // Child closed stdout within the deadline; reap it for its status.
            match child.wait() {
                Ok(status) if status.success() => Some(buf),
                Ok(status) => {
                    tracing::debug!(program, ?status, "cwd probe exited unsuccessfully");
                    None
                }
                Err(err) => {
                    tracing::debug!(program, %err, "failed to wait on cwd probe");
                    None
                }
            }
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Hung child: kill and reap so it can't stall discovery or leak a zombie.
            tracing::debug!(program, ?timeout, "cwd probe timed out; killing");
            let _ = child.kill();
            let _ = child.wait();
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Watcher gone without sending (e.g. read error) — reap and degrade.
            let _ = child.wait();
            None
        }
    };

    // The watcher returns once stdout hits EOF, which the kill above guarantees.
    let _ = watcher.join();
    outcome
}

/// Parse the `-Fn` body of `lsof`, returning the first `n`-prefixed path.
///
/// Split out for unit testing without spawning a process.
fn parse_lsof_fn(stdout: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        if let Some(path) = line.strip_prefix('n') {
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_live_layout() {
        // Verified directory names from ~/.claude/projects (dossier lane B1).
        assert_eq!(
            project_slug(Path::new("/Users/jakubsladek/Projects/private")),
            "-Users-jakubsladek-Projects-private"
        );
        assert_eq!(
            project_slug(Path::new("/Users/jakubsladek/Projects/private/fnetflyx")),
            "-Users-jakubsladek-Projects-private-fnetflyx"
        );
    }

    #[test]
    fn slug_handles_dot_and_worktree_double_dash() {
        // `/.claude-worktrees/` → `--claude-worktrees` (slash + dot both → dash).
        assert_eq!(
            project_slug(Path::new(
                "/Users/jakubsladek/Projects/private/fnetflyx/.claude-worktrees/great-lamport-44ebec"
            )),
            "-Users-jakubsladek-Projects-private-fnetflyx--claude-worktrees-great-lamport-44ebec"
        );
        // A `.` in a directory name is a dash, not a separator.
        assert_eq!(
            project_slug(Path::new(
                "/Users/jakubsladek/Projects/private/qxpspace.com"
            )),
            "-Users-jakubsladek-Projects-private-qxpspace-com"
        );
    }

    #[test]
    fn lsof_fn_parser_extracts_first_path() {
        // Real `-Fn` shape: a `p<pid>` header line then an `n<path>` line.
        let body = b"p98608\nn/Users/jakubsladek/Projects/private/fnetflyx\n";
        assert_eq!(
            parse_lsof_fn(body),
            Some(PathBuf::from(
                "/Users/jakubsladek/Projects/private/fnetflyx"
            ))
        );
    }

    #[test]
    fn lsof_fn_parser_ignores_non_path_lines() {
        assert_eq!(parse_lsof_fn(b"p123\n"), None);
        assert_eq!(parse_lsof_fn(b""), None);
        // An empty `n` field is not a usable path.
        assert_eq!(parse_lsof_fn(b"n\n"), None);
    }

    #[test]
    fn discord_socket_path_uses_tmpdir() {
        let sock = discord_ipc_socket(0);
        assert!(sock.ends_with("discord-ipc-0"));
    }

    #[test]
    fn lsof_is_invoked_by_absolute_trusted_path() {
        // The cwd probe must use the absolute `/usr/sbin/lsof`, never a bare
        // `lsof` resolved through `$PATH` (FR-8/AC-1, F28).
        assert_eq!(LSOF_PATH, "/usr/sbin/lsof");
        assert!(Path::new(LSOF_PATH).is_absolute());
    }

    #[test]
    fn run_bounded_returns_stdout_on_fast_success() {
        // A trusted, quick command resolves within the deadline and its stdout is
        // returned verbatim.
        let out = run_bounded("/bin/sh", &["-c", "printf hello"], Duration::from_secs(5));
        assert_eq!(out.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn run_bounded_times_out_and_returns_none_for_slow_command() {
        // A fabricated slow command must be killed at the deadline and yield
        // `None` rather than stalling discovery (FR-8/AC-1, F41).
        let start = std::time::Instant::now();
        let out = run_bounded("/bin/sh", &["-c", "sleep 30"], Duration::from_millis(200));
        let elapsed = start.elapsed();
        assert_eq!(out, None);
        // It must return promptly near the deadline, not after the full sleep.
        assert!(
            elapsed < Duration::from_secs(5),
            "run_bounded did not honor the timeout: {elapsed:?}"
        );
    }

    #[test]
    fn run_bounded_returns_none_on_nonzero_exit() {
        let out = run_bounded("/bin/sh", &["-c", "exit 1"], Duration::from_secs(5));
        assert_eq!(out, None);
    }

    #[test]
    fn run_bounded_returns_none_when_program_missing() {
        let out = run_bounded(
            "/nonexistent/definitely-not-a-binary",
            &[],
            Duration::from_secs(5),
        );
        assert_eq!(out, None);
    }
}
