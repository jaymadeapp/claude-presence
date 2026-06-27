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
use std::process::Command;

use crate::error::{Error, Result};

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

/// PID→cwd fallback via `lsof -a -p <PID> -d cwd -Fn` (FR-1/AC-3).
///
/// Used only when `sysinfo` reports no cwd. `-Fn` emits machine-parseable output
/// where the cwd line starts with `n` followed by the absolute path; we return
/// the first such line. Returns `None` (never an error) on any failure so the
/// caller can simply skip a session whose cwd cannot be resolved.
pub fn cwd_via_lsof(pid: i32) -> Option<PathBuf> {
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_lsof_fn(&output.stdout)
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
}
