//! launchd user-agent lifecycle: render/write the `LaunchAgent` plist, then
//! `launchctl bootstrap`/`bootout` it into the per-user GUI domain (FR-8/AC-1,
//! AC-2, AC-3; design §3.1).
//!
//! # Why a user agent (no root)
//!
//! The daemon runs entirely in the calling user's `gui/<uid>` domain. A
//! `gui/<uid>` agent inherits that user's per-session environment — including the
//! dynamic `/var/folders` `TMPDIR` and `HOME` — so the plist deliberately omits
//! `EnvironmentVariables`; baking the current `TMPDIR` would pin a stale path and
//! break the Discord socket probe (design §3.1).
//!
//! # KeepAlive and the bootout-before-exit rule
//!
//! `KeepAlive = { SuccessfulExit = false }` means launchd relaunches the daemon
//! only when it *crashes* — a clean `exit(0)` that has already cleared the Discord
//! presence is NOT relaunched. `uninstall` must therefore call [`bootout`] (which
//! tears the job out of launchd) **before** the process exits, so launchd cannot
//! race in and relaunch a freshly-cleared daemon (FR-8/AC-3). The
//! `install`/`uninstall` flows in task 4.2 compose these functions in that order.
//!
//! # Public surface (composed by task 4.2)
//!
//! * [`render_plist`] — pure: `(binary, label, stdout, stderr) → plist XML`.
//! * [`install`] — write the plist to `~/Library/LaunchAgents/<label>.plist` and
//!   `bootstrap` it (`launchctl bootstrap gui/<uid> <plist>`).
//! * [`uninstall`] — `bootout` the job (`launchctl bootout gui/<uid> <label>`)
//!   then remove the plist. Idempotent: a not-loaded job / missing plist is OK.
//! * [`bootstrap`] / [`bootout`] — thin `launchctl` wrappers if a caller needs
//!   the load/unload step on its own.
//! * [`plist_path`] / [`label`] — the well-known plist location and job label.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};
use crate::platform::macos::home_dir;

/// launchd job label, `com.<author>.claude-presence` (design §3.1). Also the
/// plist basename (`<label>.plist`) and the `bootout` target's job name.
const LABEL: &str = "com.jakubsladek.claude-presence";

/// Plist template (design §3.1). Placeholders `{{LABEL}}`, `{{BINARY}}`,
/// `{{STDOUT}}`, `{{STDERR}}` are resolved at install time by [`render_plist`].
/// The canonical copy lives at `assets/LaunchAgent.plist.tmpl`.
const PLIST_TEMPLATE: &str = include_str!("../../assets/LaunchAgent.plist.tmpl");

/// The launchd job label (`com.jakubsladek.claude-presence`).
pub fn label() -> &'static str {
    LABEL
}

/// The well-known plist path, `~/Library/LaunchAgents/<label>.plist`.
pub fn plist_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

/// Resolve `~/.local/state/claude-presence/logs` — the same `0700` log dir the
/// rotating tracing appender writes to (`logging.rs`). Replicated here because
/// that module's `state_log_dir` is private; the daemon's stdout/stderr (anything
/// printed before `tracing` is wired) lands alongside the structured logs.
fn log_dir() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".local")
        .join("state")
        .join("claude-presence")
        .join("logs"))
}

/// Absolute path of the current executable, canonicalized.
///
/// Used for `ProgramArguments[0]` so launchd execs the exact binary that ran
/// `install` (FR-8/AC-1), regardless of how it was invoked.
fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    // `canonicalize` resolves symlinks and `.`/`..`; falling back to the raw exe
    // path keeps install working even on exotic filesystems where it fails.
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// XML-escape a string before it is substituted into a plist `<string>` element.
///
/// Escapes the five predefined XML entities. `&` MUST be escaped first, otherwise
/// the `&` of a subsequently-inserted entity would itself be re-escaped. A
/// user-derived install path containing `&`/`<`/`>` would otherwise produce a
/// malformed plist that `launchctl` refuses to load (FR-3/AC-2, F25).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Render the plist XML from the resolved binary path, job label, and log paths.
///
/// Pure (no I/O): substitutes the four template placeholders. Every user-derived
/// string (the binary path and the log paths) is XML-escaped via [`xml_escape`]
/// first, so a path containing `&`/`<`/`>` still yields a well-formed plist
/// (FR-3/AC-2). The result always carries `RunAtLoad=true`,
/// `KeepAlive={SuccessfulExit:false}`, `ProcessType=Background`, and
/// `ProgramArguments=[binary, "run"]`, and never emits `EnvironmentVariables`
/// (design §3.1).
pub fn render_plist(binary: &Path, label: &str, stdout: &Path, stderr: &Path) -> String {
    PLIST_TEMPLATE
        .replace("{{LABEL}}", &xml_escape(label))
        .replace("{{BINARY}}", &xml_escape(&binary.to_string_lossy()))
        .replace("{{STDOUT}}", &xml_escape(&stdout.to_string_lossy()))
        .replace("{{STDERR}}", &xml_escape(&stderr.to_string_lossy()))
}

/// `gui/<uid>` service-target domain for the current user.
fn gui_domain() -> String {
    let uid = nix::unistd::Uid::current().as_raw();
    format!("gui/{uid}")
}

/// Write the plist to `~/Library/LaunchAgents/<label>.plist` and load it with
/// `launchctl bootstrap gui/<uid> <plist>`.
///
/// Uses the current executable's canonicalized absolute path for
/// `ProgramArguments[0]`. The `LaunchAgents` and log directories are created if
/// absent. Re-installing reloads the agent: any already-loaded job is booted out
/// before bootstrap so the freshly-written plist (e.g. a moved binary path) always
/// takes effect (FR-8/AC-1). A first install (nothing loaded) stays idempotent
/// because `bootout` tolerates a not-loaded job.
pub fn install() -> Result<()> {
    let binary = current_binary()?;
    let logs = log_dir()?;
    std::fs::create_dir_all(&logs)?;
    let stdout = logs.join("daemon.out.log");
    let stderr = logs.join("daemon.err.log");

    let plist = render_plist(&binary, LABEL, &stdout, &stderr);
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist)?;

    // bootstrap is a no-op if the Label is already loaded, so it would keep running a
    // stale ProgramArguments path. Bootout first (tolerating not-loaded) so the freshly
    // written plist always takes effect (FR-8/AC-1).
    bootout(LABEL)?; // already tolerates not-loaded
    bootstrap(&path)?;
    tracing::info!(target: "install", "launchd agent bootstrapped");
    Ok(())
}

/// Unload the job, then remove the plist (FR-8/AC-3).
///
/// `bootout` runs first so launchd can no longer relaunch the daemon; callers
/// that also clear the Discord presence MUST run this *before* the process exits.
/// A not-loaded job and a missing plist are both treated as success
/// (idempotent uninstall, NFR-6).
pub fn uninstall() -> Result<()> {
    bootout(LABEL)?;

    let path = plist_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }
    tracing::info!(target: "install", "launchd agent removed");
    Ok(())
}

/// `launchctl bootstrap gui/<uid> <plist>`.
///
/// Loads the agent into the per-user GUI domain. An "already bootstrapped"
/// failure (launchctl exit 5 / "service already loaded") is tolerated so a
/// repeated install is idempotent; any other non-zero exit becomes an
/// [`Error::Other`].
pub fn bootstrap(plist: &Path) -> Result<()> {
    let domain = gui_domain();
    let output = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(plist)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_already_loaded(&output.status, &stderr) {
        tracing::debug!(target: "install", "launchd agent already loaded");
        return Ok(());
    }
    Err(launchctl_error("bootstrap", &output.status, &stderr))
}

/// `launchctl bootout gui/<uid> <label>`.
///
/// Tears the job out of the per-user GUI domain. A "not loaded" failure
/// (launchctl exit 3 / "No such process") is tolerated so uninstall is
/// idempotent; any other non-zero exit becomes an [`Error::Other`].
pub fn bootout(label: &str) -> Result<()> {
    let target = format!("{}/{label}", gui_domain());
    let output = Command::new("launchctl")
        .arg("bootout")
        .arg(&target)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_not_loaded(&output.status, &stderr) {
        tracing::debug!(target: "install", "launchd agent already unloaded");
        return Ok(());
    }
    Err(launchctl_error("bootout", &output.status, &stderr))
}

/// True if a failed `bootstrap` just means the job is already loaded.
fn is_already_loaded(status: &std::process::ExitStatus, stderr: &str) -> bool {
    // launchctl returns 5 (EIO-ish) for an already-bootstrapped service.
    status.code() == Some(5) || stderr.to_lowercase().contains("already")
}

/// True if a failed `bootout` just means the job is not currently loaded.
fn is_not_loaded(status: &std::process::ExitStatus, stderr: &str) -> bool {
    // launchctl returns 3 ("No such process") when the job is not loaded.
    let lower = stderr.to_lowercase();
    status.code() == Some(3)
        || lower.contains("no such process")
        || lower.contains("could not find")
        || lower.contains("not find service")
}

/// Build a categorical error from a non-zero `launchctl` invocation. The stderr
/// is launchctl's own diagnostic (no user-derived data), safe to surface.
fn launchctl_error(verb: &str, status: &std::process::ExitStatus, stderr: &str) -> Error {
    let code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    let msg = stderr.trim();
    Error::Other(format!("launchctl {verb} failed (exit {code}): {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn render() -> String {
        render_plist(
            Path::new("/usr/local/bin/claude-presence"),
            LABEL,
            Path::new("/home/u/.local/state/claude-presence/logs/daemon.out.log"),
            Path::new("/home/u/.local/state/claude-presence/logs/daemon.err.log"),
        )
    }

    #[test]
    fn plist_has_label_and_program_arguments() {
        let p = render();
        assert!(p.contains(&format!("<string>{LABEL}</string>")));
        // ProgramArguments = [abs binary, "run"].
        assert!(p.contains("<string>/usr/local/bin/claude-presence</string>"));
        assert!(p.contains("<string>run</string>"));
    }

    #[test]
    fn plist_run_at_load_is_true() {
        let p = render();
        assert!(p.contains("<key>RunAtLoad</key>"));
        // The key is immediately followed by <true/>.
        let after = p.split("<key>RunAtLoad</key>").nth(1).unwrap();
        assert!(after.trim_start().starts_with("<true/>"));
    }

    #[test]
    fn plist_keepalive_only_restarts_on_crash() {
        let p = render();
        // KeepAlive must be a dict with SuccessfulExit=false (not a bare <true/>),
        // so a clean exit-0 that cleared the presence is NOT relaunched.
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>SuccessfulExit</key>"));
        let after = p.split("<key>SuccessfulExit</key>").nth(1).unwrap();
        assert!(after.trim_start().starts_with("<false/>"));
        // Guard against KeepAlive being rendered as a plain boolean true.
        assert!(!p.contains("<key>KeepAlive</key>\n\t<true/>"));
    }

    #[test]
    fn plist_process_type_is_background() {
        let p = render();
        assert!(p.contains("<key>ProcessType</key>"));
        let after = p.split("<key>ProcessType</key>").nth(1).unwrap();
        assert!(after
            .trim_start()
            .starts_with("<string>Background</string>"));
    }

    #[test]
    fn plist_sets_log_paths() {
        let p = render();
        assert!(p.contains("<key>StandardOutPath</key>"));
        assert!(p.contains("<key>StandardErrorPath</key>"));
        assert!(p.contains("daemon.out.log"));
        assert!(p.contains("daemon.err.log"));
    }

    #[test]
    fn plist_omits_environment_variables() {
        // gui/<uid> agents inherit per-user TMPDIR/HOME; a hardcoded /var/folders
        // TMPDIR would be a bug (design §3.1).
        let p = render();
        assert!(
            !p.contains("EnvironmentVariables"),
            "plist must NOT set EnvironmentVariables"
        );
        assert!(!p.contains("TMPDIR"));
    }

    #[test]
    fn rendered_paths_are_absolute() {
        let p = render();
        for needle in [
            "<string>/usr/local/bin/claude-presence</string>",
            "<string>/home/u/.local/state/claude-presence/logs/daemon.out.log</string>",
            "<string>/home/u/.local/state/claude-presence/logs/daemon.err.log</string>",
        ] {
            assert!(p.contains(needle), "missing absolute path: {needle}");
        }
    }

    #[test]
    fn plist_xml_escapes_user_derived_paths() {
        // A binary path with shell/XML metacharacters must appear XML-escaped in the
        // plist <string> so launchctl loads a well-formed file (FR-3/AC-2, F25).
        let p = render_plist(
            Path::new("/Users/a&b/<dir>/claude-presence"),
            LABEL,
            Path::new("/home/u/logs/daemon.out.log"),
            Path::new("/home/u/logs/daemon.err.log"),
        );
        // The escaped form is present...
        assert!(p.contains("<string>/Users/a&amp;b/&lt;dir&gt;/claude-presence</string>"));
        // ...and no bare `&` from our substitution survives (every `&` is an entity).
        for frag in p.split('&').skip(1) {
            assert!(
                frag.starts_with("amp;")
                    || frag.starts_with("lt;")
                    || frag.starts_with("gt;")
                    || frag.starts_with("quot;")
                    || frag.starts_with("apos;"),
                "unescaped `&` in rendered plist near: {frag:.16}"
            );
        }
    }

    #[test]
    fn xml_escape_handles_ampersand_first() {
        // `&` must be escaped before `<`/`>` so the inserted entity's own `&` is not
        // double-escaped.
        assert_eq!(xml_escape("a&<b>\"'"), "a&amp;&lt;b&gt;&quot;&apos;");
    }

    #[test]
    fn plist_path_is_under_launch_agents() {
        let path = plist_path().unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with("Library/LaunchAgents/com.jakubsladek.claude-presence.plist"));
    }

    #[test]
    fn label_is_reverse_dns() {
        assert_eq!(label(), "com.jakubsladek.claude-presence");
    }

    #[test]
    fn already_loaded_is_idempotent() {
        // String-based detection (the exit-code path needs a real ExitStatus).
        assert!(!"Bootstrap failed: 5: Input/output error"
            .to_lowercase()
            .contains("already"));
        assert!("service already bootstrapped"
            .to_lowercase()
            .contains("already"));
    }

    #[test]
    fn not_loaded_messages_are_idempotent() {
        for s in [
            "Boot-out failed: 3: No such process",
            "Could not find service",
            "Could not find specified service",
        ] {
            let lower = s.to_lowercase();
            assert!(
                lower.contains("no such process")
                    || lower.contains("could not find")
                    || lower.contains("not find service"),
                "should be tolerated: {s}"
            );
        }
    }
}
