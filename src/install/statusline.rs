//! Chain-install / uninstall of the statusLine wrapper (ADR-4, FR-3/AC-1, C-6,
//! FR-8/AC-3).
//!
//! Claude Code renders a status line by running the command stored at
//! `~/.claude/settings.json` → `statusLine.command`, passing the statusLine JSON
//! on **stdin** and displaying the command's stdout. There is no custom env var to
//! key off (verified, v2.1.181), so this installer **chains** that command instead
//! of replacing it:
//!
//! * at install we capture the user's current `statusLine.command` verbatim, store
//!   it in a state file under the `0700` state dir, and point `statusLine.command`
//!   at our bundled [`assets/statusline-wrapper.sh`]. The wrapper sources that
//!   state file, runs the stored original (stdout passed straight through), and
//!   *also* tees the same stdin JSON to `claude-presence forward --kind statusline`
//!   in the background so a down daemon can never block or fail Claude Code (C-6).
//! * at uninstall we restore the original **only if** `statusLine.command` still
//!   equals our installed wrapper (no drift); otherwise the user changed it since
//!   install, so we do NOT clobber their value — we `warn!` and leave it intact.
//!
//! # Anthropic-stable vs internal
//!
//! The statusLine JSON contract is Anthropic-stable, but `settings.json` shape is
//! not formally documented; we only ever touch the `statusLine` key (parse →
//! modify that one key → write back), never `hooks` or anything else, so this stays
//! safe to run alongside the hooks installer (task 3.3 / 4.2).
//!
//! # Public surface (composed by task 4.2)
//!
//! * [`install`] — capture + store the original, write the wrapper + state file,
//!   repoint `statusLine.command` at the wrapper.
//! * [`uninstall`] — restore the original on no drift, else surgically drop our
//!   segment and warn.
//!
//! Both are thin I/O shells over the pure, unit-tested transforms [`apply_install`]
//! and [`apply_uninstall`].

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::warn;

use crate::error::{Error, Result};
use crate::platform::macos::{claude_dir, home_dir};

/// The bundled wrapper script (canonical copy: `assets/statusline-wrapper.sh`).
///
/// It is fully static — it resolves the stored original command and the forwarder
/// path from the state file under `$HOME/.local/state/claude-presence`, so no
/// install-time placeholder substitution is needed (which also keeps the asset
/// `shellcheck`-clean).
const WRAPPER_SCRIPT: &str = include_str!("../../assets/statusline-wrapper.sh");

/// Resolve the `0700` state dir, `~/.local/state/claude-presence` — the same root
/// the ingest socket and logs live under (mirrors `ingest::socket::socket_path`).
fn state_dir() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".local")
        .join("state")
        .join("claude-presence"))
}

/// `~/.claude/settings.json` — the Claude Code settings file holding `statusLine`.
fn settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// Installed wrapper script path, `<state>/statusline-wrapper.sh`.
fn wrapper_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("statusline-wrapper.sh"))
}

/// State file the wrapper sources at runtime, `<state>/statusline-state.sh`.
///
/// Must match the path baked into `assets/statusline-wrapper.sh`
/// (`$HOME/.local/state/claude-presence/statusline-state.sh`).
fn state_file_path() -> Result<PathBuf> {
    Ok(state_dir()?.join("statusline-state.sh"))
}

/// The string written into `statusLine.command` — the absolute wrapper path.
///
/// This exact value is what [`uninstall`] compares against to distinguish "still
/// our wrapper" (restore) from "user changed it" (drift → warn, don't clobber).
fn wrapper_invocation() -> Result<String> {
    Ok(wrapper_path()?.to_string_lossy().into_owned())
}

/// Whether `~/.claude/settings.json`'s `statusLine.command` currently points at
/// our installed wrapper (the `doctor` settings-wiring check, FR-8/AC-2).
///
/// Pure decision in [`statusline_wired`]; this is the thin I/O shell that reads
/// the real settings file. A missing/absent/empty settings file reads as "not
/// wired" rather than an error so `doctor` always produces a verdict.
pub fn is_wired() -> Result<bool> {
    let settings = read_settings(&settings_path()?)?;
    let wrapper_inv = wrapper_invocation()?;
    Ok(statusline_wired(&settings, &wrapper_inv))
}

/// Pure: does `settings`'s `statusLine.command` equal `wrapper_invocation`?
///
/// Tolerates both shapes via [`extract_command`]; `None` (unset) is not wired.
fn statusline_wired(settings: &Map<String, Value>, wrapper_invocation: &str) -> bool {
    extract_command(settings).as_deref() == Some(wrapper_invocation)
}

/// Absolute path of the current executable (the `claude-presence` binary), so the
/// wrapper forwards to the exact binary that ran `install` (mirrors
/// `launchd::current_binary`). Falls back to the raw exe path if `canonicalize`
/// fails on an exotic filesystem.
fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Read `settings.json` into a JSON object, or an empty object if it is absent.
///
/// A missing file is not an error (install can bootstrap settings); any other read
/// failure, or a non-object top-level JSON, is surfaced so we never silently
/// discard the user's settings.
fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            if text.trim().is_empty() {
                return Ok(Map::new());
            }
            match serde_json::from_str::<Value>(&text)? {
                Value::Object(map) => Ok(map),
                _ => Err(Error::Other(
                    "settings.json is not a JSON object; refusing to modify".into(),
                )),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Serialize `settings` back to `path` pretty-printed with a trailing newline.
fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<()> {
    let mut text = serde_json::to_string_pretty(&Value::Object(settings.clone()))?;
    text.push('\n');
    write_atomic(path, &text)?;
    Ok(())
}

/// Write `contents` to `path` atomically and durably: stream into a sibling
/// `<name>.json.tmp`, `fsync` it, then `rename` over `path`. The rename is atomic
/// within a filesystem, so a crash / power-loss / ENOSPC mid-write can never leave
/// the user's `settings.json` truncated or half-written (C-6, NFR-6).
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Extract the current `statusLine.command` string, tolerating both shapes Claude
/// Code accepts: the object form `{ "type": "command", "command": "…" }` and a
/// bare command string. Returns `None` when `statusLine` is unset or has no
/// usable command.
fn extract_command(settings: &Map<String, Value>) -> Option<String> {
    match settings.get("statusLine") {
        Some(Value::Object(obj)) => obj
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Pure install transform: set `statusLine.command` to `wrapper_invocation` and
/// return the captured original command (if any) for storage.
///
/// Only the `statusLine` key is touched; every other key (notably `hooks`) is
/// preserved untouched. `statusLine` is normalised to the canonical object form
/// `{ "type": "command", "command": <wrapper> }` so Claude Code always sees a valid
/// command entry, regardless of the original shape.
fn apply_install(
    mut settings: Map<String, Value>,
    wrapper_invocation: &str,
) -> (Map<String, Value>, Option<String>) {
    let original = extract_command(&settings);
    let mut obj = Map::new();
    obj.insert("type".into(), Value::String("command".into()));
    obj.insert(
        "command".into(),
        Value::String(wrapper_invocation.to_owned()),
    );
    settings.insert("statusLine".into(), Value::Object(obj));
    (settings, original)
}

/// Pure uninstall transform with drift handling (ADR-4).
///
/// * If the current `statusLine.command` still equals `wrapper_invocation` (no
///   drift): restore `stored_original` exactly, or remove the `statusLine` key
///   entirely when there was no original.
/// * If it has drifted (user replaced it since install): leave it untouched (the
///   caller warns). Only when the *drifted* value still literally contains our
///   wrapper path do we surgically drop the `statusLine` key as well, so we never
///   leave a dangling reference to a removed wrapper.
///
/// Returns `(new_settings, drifted)` so the caller can `warn!` on drift.
fn apply_uninstall(
    mut settings: Map<String, Value>,
    wrapper_invocation: &str,
    stored_original: Option<&str>,
) -> (Map<String, Value>, bool) {
    let current = extract_command(&settings);
    match current.as_deref() {
        // No drift: the value is exactly our wrapper invocation.
        Some(cmd) if cmd == wrapper_invocation => {
            match stored_original {
                Some(orig) => {
                    let mut obj = Map::new();
                    obj.insert("type".into(), Value::String("command".into()));
                    obj.insert("command".into(), Value::String(orig.to_owned()));
                    settings.insert("statusLine".into(), Value::Object(obj));
                }
                None => {
                    settings.remove("statusLine");
                }
            }
            (settings, false)
        }
        // Drifted but the new value still embeds our wrapper path → surgically drop
        // the statusLine key (it would otherwise point at a removed wrapper).
        Some(cmd) if cmd.contains(wrapper_invocation) => {
            settings.remove("statusLine");
            (settings, true)
        }
        // Drifted to an unrelated user value, or unset → never clobber.
        _ => (settings, true),
    }
}

/// Render the wrapper's runtime state file: `CP_FORWARD_BIN` and
/// `CP_INNER_COMMAND` as POSIX-safe single-quoted shell assignments.
///
/// Both values are single-quoted and any embedded `'` is escaped as `'\''`, so an
/// arbitrary original command (which routinely contains single quotes, e.g.
/// `jq -r '.model.display_name'`) round-trips exactly when the wrapper sources it.
fn render_state_file(forward_bin: &str, inner_command: Option<&str>) -> String {
    format!(
        "# generated by `claude-presence install` — do not edit\nCP_FORWARD_BIN={}\nCP_INNER_COMMAND={}\n",
        shell_single_quote(forward_bin),
        shell_single_quote(inner_command.unwrap_or("")),
    )
}

/// Wrap `s` in single quotes, escaping any embedded single quote as `'\''`.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Create `dir` (and parents) `0700`, tightening an existing looser dir (mirrors
/// `ingest::socket::ensure_dir_0700`).
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

/// Chain-install the statusline wrapper (FR-3/AC-1, C-6).
///
/// Captures the current `statusLine.command`, writes the bundled wrapper (`0755`)
/// and a state file (`0600`) carrying that stored original plus the forwarder path
/// into the `0700` state dir, then repoints `statusLine.command` at the wrapper —
/// preserving every other key in `settings.json`. Idempotent: re-running on an
/// already-installed wrapper captures the wrapper invocation as the "original",
/// which is harmless because uninstall restores by the stored state file written
/// here, not by that captured value.
pub fn install() -> Result<()> {
    let dir = state_dir()?;
    ensure_dir_0700(&dir)?;

    let settings_file = settings_path()?;
    let settings = read_settings(&settings_file)?;
    let wrapper_inv = wrapper_invocation()?;

    let (new_settings, captured) = apply_install(settings, &wrapper_inv);
    // If we are re-installing over our own wrapper, the captured value is the
    // wrapper itself; do not store that as the "original" (it would self-reference).
    let original = match captured {
        Some(ref c) if *c == wrapper_inv => None,
        other => other,
    };

    // Write the wrapper script (0755) and the runtime state file (0600).
    let wrapper = wrapper_path()?;
    std::fs::write(&wrapper, WRAPPER_SCRIPT)?;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;

    let forward_bin = current_binary()?;
    let state = render_state_file(&forward_bin.to_string_lossy(), original.as_deref());
    let state_file = state_file_path()?;
    std::fs::write(&state_file, state)?;
    std::fs::set_permissions(&state_file, std::fs::Permissions::from_mode(0o600))?;

    write_settings(&settings_file, &new_settings)?;
    tracing::info!(target: "install", "statusline wrapper chained");
    Ok(())
}

/// Reverse [`install`] with drift handling (ADR-4, FR-8/AC-3, NFR-6).
///
/// Reads the stored original from the state file, then: if `statusLine.command`
/// still equals our wrapper, restores that original exactly (or removes the key if
/// there was none); if the user has since changed it, leaves their value untouched
/// and `warn!`s about the drift. Finally removes the wrapper script and state file.
/// Idempotent: a missing settings file / state file / wrapper is treated as
/// success.
pub fn uninstall() -> Result<()> {
    let settings_file = settings_path()?;
    let settings = read_settings(&settings_file)?;
    let wrapper_inv = wrapper_invocation()?;
    let stored_original = read_stored_original()?;

    let (new_settings, drifted) =
        apply_uninstall(settings, &wrapper_inv, stored_original.as_deref());
    if drifted {
        warn!(
            target: "install",
            "statusLine.command no longer points at our wrapper (user-modified); leaving it untouched"
        );
    }
    write_settings(&settings_file, &new_settings)?;

    // Remove our installed artifacts (best-effort; missing is fine).
    remove_if_present(&wrapper_path()?)?;
    remove_if_present(&state_file_path()?)?;
    tracing::info!(target: "install", "statusline wrapper removed");
    Ok(())
}

/// Read the stored original `CP_INNER_COMMAND` back from the state file.
///
/// Parses the single-quoted shell assignment we wrote in [`render_state_file`].
/// Returns `None` when the file is absent, the variable is unset, or the stored
/// value is empty (i.e. there was no original command to restore).
fn read_stored_original() -> Result<Option<String>> {
    let path = state_file_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(parse_inner_command(&text))
}

/// Extract `CP_INNER_COMMAND='…'` from the state-file text, undoing the
/// single-quote escaping. Returns `None` for an unset or empty value.
fn parse_inner_command(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("CP_INNER_COMMAND=") {
            let unquoted = shell_single_unquote(rest)?;
            return if unquoted.is_empty() {
                None
            } else {
                Some(unquoted)
            };
        }
    }
    None
}

/// Undo [`shell_single_quote`]: a `'…'` value where `'\''` decodes to a literal
/// `'`. Returns `None` if `s` is not a well-formed single-quoted token.
fn shell_single_unquote(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut out = String::new();
    let inner = &s[1..];
    let mut chars = inner.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '\'' {
            // Either the closing quote, or the start of a `'\''` escape.
            match chars.peek() {
                Some(&(_, '\\')) => {
                    // Expect `\''` → emit a single literal `'`.
                    chars.next(); // consume '\\'
                    match (chars.next(), chars.next()) {
                        (Some((_, '\'')), Some((_, '\''))) => out.push('\''),
                        _ => return None,
                    }
                }
                // Closing quote (possibly followed by trailing whitespace).
                _ => return Some(out),
            }
        } else {
            out.push(ch);
        }
    }
    // Reached end of string without a closing quote.
    None
}

/// Remove a file, treating "not found" as success (idempotent uninstall).
fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const WRAPPER: &str = "/home/u/.local/state/claude-presence/statusline-wrapper.sh";
    const ORIGINAL: &str = "input=$(cat); echo \"$input\" | jq -r '.model.display_name'";

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn install_captures_and_replaces_object_form() {
        let settings = obj(json!({
            "model": "Opus",
            "statusLine": { "type": "command", "command": ORIGINAL },
            "hooks": { "Stop": [] }
        }));
        let (new, captured) = apply_install(settings, WRAPPER);
        assert_eq!(captured.as_deref(), Some(ORIGINAL));
        // statusLine now points at our wrapper, normalised to object form.
        assert_eq!(new["statusLine"]["command"].as_str(), Some(WRAPPER));
        assert_eq!(new["statusLine"]["type"].as_str(), Some("command"));
        // Other keys preserved untouched — crucially `hooks` (task 3.3 owns it).
        assert_eq!(new["model"].as_str(), Some("Opus"));
        assert!(new.contains_key("hooks"));
    }

    #[test]
    fn install_captures_plain_string_form() {
        let settings = obj(json!({ "statusLine": ORIGINAL }));
        let (new, captured) = apply_install(settings, WRAPPER);
        assert_eq!(captured.as_deref(), Some(ORIGINAL));
        assert_eq!(new["statusLine"]["command"].as_str(), Some(WRAPPER));
    }

    #[test]
    fn install_with_no_original_statusline() {
        let settings = obj(json!({ "model": "Opus" }));
        let (new, captured) = apply_install(settings, WRAPPER);
        assert_eq!(captured, None);
        assert_eq!(new["statusLine"]["command"].as_str(), Some(WRAPPER));
        assert_eq!(new["model"].as_str(), Some("Opus"));
    }

    #[test]
    fn uninstall_no_drift_restores_original_byte_for_byte() {
        // Install then uninstall round-trips the exact original command.
        let settings = obj(json!({
            "model": "Opus",
            "statusLine": { "type": "command", "command": ORIGINAL }
        }));
        let (installed, captured) = apply_install(settings, WRAPPER);
        let (restored, drifted) = apply_uninstall(installed, WRAPPER, captured.as_deref());
        assert!(!drifted);
        assert_eq!(restored["statusLine"]["command"].as_str(), Some(ORIGINAL));
        assert_eq!(restored["model"].as_str(), Some("Opus"));
    }

    #[test]
    fn uninstall_no_original_removes_key_cleanly() {
        let settings = obj(json!({ "model": "Opus" }));
        let (installed, captured) = apply_install(settings, WRAPPER);
        assert_eq!(captured, None);
        let (restored, drifted) = apply_uninstall(installed, WRAPPER, None);
        assert!(!drifted);
        assert!(
            !restored.contains_key("statusLine"),
            "statusLine key must be removed when there was no original"
        );
        assert_eq!(restored["model"].as_str(), Some("Opus"));
    }

    #[test]
    fn uninstall_drift_leaves_user_value_intact() {
        // User replaced our wrapper with their own command after install.
        let user_cmd = "echo custom";
        let settings = obj(json!({
            "statusLine": { "type": "command", "command": user_cmd }
        }));
        let (out, drifted) = apply_uninstall(settings, WRAPPER, Some(ORIGINAL));
        assert!(drifted, "a user-modified value must be detected as drift");
        // Their value is left exactly as-is — never clobbered.
        assert_eq!(out["statusLine"]["command"].as_str(), Some(user_cmd));
    }

    #[test]
    fn uninstall_drift_unset_is_noop() {
        let settings = obj(json!({ "model": "Opus" }));
        let (out, drifted) = apply_uninstall(settings, WRAPPER, Some(ORIGINAL));
        assert!(drifted);
        assert!(!out.contains_key("statusLine"));
        assert_eq!(out["model"].as_str(), Some("Opus"));
    }

    #[test]
    fn uninstall_drift_embedding_wrapper_drops_segment() {
        // The user wrapped OUR wrapper in something else; the value still embeds
        // our path, so removing the now-dangling segment is correct.
        let drifted_cmd = format!("{WRAPPER} ; echo extra");
        let settings = obj(json!({
            "statusLine": { "type": "command", "command": drifted_cmd }
        }));
        let (out, drifted) = apply_uninstall(settings, WRAPPER, Some(ORIGINAL));
        assert!(drifted);
        assert!(!out.contains_key("statusLine"));
    }

    #[test]
    fn state_file_round_trips_command_with_single_quotes() {
        // The real-world original is full of single quotes (jq filters); the
        // state file must round-trip it exactly through the wrapper's `.` source.
        let state = render_state_file("/usr/local/bin/claude-presence", Some(ORIGINAL));
        assert!(state.contains("CP_FORWARD_BIN="));
        assert!(state.contains("CP_INNER_COMMAND="));
        let parsed = parse_inner_command(&state);
        assert_eq!(parsed.as_deref(), Some(ORIGINAL));
    }

    #[test]
    fn state_file_empty_command_parses_as_none() {
        let state = render_state_file("/usr/local/bin/claude-presence", None);
        assert_eq!(parse_inner_command(&state), None);
    }

    #[test]
    fn single_quote_escaping_is_posix_safe() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        // A single quote becomes the canonical '\'' close-escape-reopen sequence.
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        // Round-trips.
        let s = "jq -r '.model' && echo 'x'";
        let quoted = shell_single_quote(s);
        let line = format!("CP_INNER_COMMAND={quoted}");
        assert_eq!(parse_inner_command(&line).as_deref(), Some(s));
    }

    #[test]
    fn wired_when_command_equals_wrapper_else_not() {
        let wired = obj(json!({
            "statusLine": { "type": "command", "command": WRAPPER }
        }));
        assert!(statusline_wired(&wired, WRAPPER));

        // A user value (drift) is not wired.
        let drifted = obj(json!({
            "statusLine": { "type": "command", "command": "echo custom" }
        }));
        assert!(!statusline_wired(&drifted, WRAPPER));

        // Unset statusLine is not wired.
        let unset = obj(json!({ "model": "Opus" }));
        assert!(!statusline_wired(&unset, WRAPPER));

        // The plain-string form is also recognised.
        let plain = obj(json!({ "statusLine": WRAPPER }));
        assert!(statusline_wired(&plain, WRAPPER));
    }

    #[test]
    fn write_atomic_round_trips_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("cp-statusline-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let contents = "{\n  \"model\": \"Opus\"\n}\n";

        write_atomic(&path, contents).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
        // No leftover temp file beside the target.
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the .json.tmp scratch file must be renamed away"
        );

        // Overwriting an existing file works and still leaves no temp.
        let next = "{\n  \"model\": \"Sonnet\"\n}\n";
        write_atomic(&path, next).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), next);
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_settings_rejects_non_object_root() {
        let dir = std::env::temp_dir().join(format!("cp-statusline-nonobj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "[1,2,3]").unwrap();

        let err = read_settings(&path).unwrap_err();
        assert!(
            matches!(err, Error::Other(msg) if msg.contains("not a JSON object")),
            "a non-object root must be refused, not silently discarded"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrapper_script_tees_and_runs_inner_without_failing_cc() {
        // The bundled wrapper must reference both the forward call and the stored
        // inner command, and forward in a backgrounded, output-discarded way so a
        // down daemon can never block or fail Claude Code.
        assert!(WRAPPER_SCRIPT.contains("forward --kind statusline"));
        assert!(WRAPPER_SCRIPT.contains("CP_INNER_COMMAND"));
        assert!(WRAPPER_SCRIPT.contains("CP_FORWARD_BIN"));
        // Backgrounded + output discarded (non-blocking, never fails CC).
        assert!(WRAPPER_SCRIPT.contains(">/dev/null 2>&1 &"));
        // Reads stdin exactly once and passes it to the inner command.
        assert!(WRAPPER_SCRIPT.contains("input=$(cat)"));
    }
}
