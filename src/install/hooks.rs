//! Chain-install / uninstall of the claude-presence lifecycle hooks in
//! `~/.claude/settings.json` (FR-4, C-6, ADR-4).
//!
//! For each lifecycle event we care about (`SessionStart`, `PreToolUse`,
//! `PostToolUse`, `Stop`, `SubagentStart`, `SubagentStop`) we **append** a single
//! command entry into that event's existing `hooks[]` group, creating the event
//! group when absent. The user's own entries (e.g. a `Stop` hook that plays
//! `afplay … Submarine.aiff`) are preserved byte-for-byte. Uninstall removes
//! **only our exact command entry by identity** and never deletes user data
//! (ADR-4, NFR-6).
//!
//! # settings.json hook shape
//!
//! The live v2.1.181 shape is a map of event → array of *matcher groups*:
//!
//! ```jsonc
//! { "hooks": {
//!     "Stop": [ { "matcher": "", "hooks": [ { "type": "command", "command": "…" } ] } ]
//! } }
//! ```
//!
//! Some events (`Stop`, `SessionStart`, …) carry an empty matcher; the matcher-ful
//! events (`PreToolUse`/`PostToolUse`) use the matcher as a tool-name filter. We
//! always append into the **empty-matcher (`""`) catch-all group** so our hook
//! fires for every tool — creating that group if the event has only narrower
//! user groups. This keeps the format identical to what CC writes and leaves
//! every user group untouched.
//!
//! # Identity for remove-by-identity
//!
//! Our entry's `command` is the absolute path to the installed forwarder script,
//! **single-quoted** so a path containing a space or shell metacharacter is inert
//! when Claude Code runs the command through a shell (F23, ADR-3;
//! `~/.local/state/claude-presence/hook-forward.sh … forward --kind hook` is
//! resolved *inside* the script; the settings entry just invokes the script). That
//! exact (quoted) command string is the identity uninstall matches on, so install
//! and uninstall round-trip on the same form and a user command that merely shares
//! a name is never touched.
//!
//! # Surface (composed by task 4.2)
//!
//! * [`install`] — write the forwarder script, then append our entry into each
//!   event group of `settings.json` (idempotent).
//! * [`uninstall`] — remove our exact entry from each event group (idempotent).
//! * [`apply_install`] / [`apply_uninstall`] — pure `serde_json::Value`
//!   transforms, unit-tested without touching the real file.
//! * [`installed_script_path`] / [`hook_command`] — the well-known script path and
//!   the exact command string used as the entry identity.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::platform::macos::{claude_dir, home_dir};

/// The lifecycle events we chain a forwarder into (FR-4/AC-1). `SessionEnd` /
/// `CwdChanged` exist too and are optional later adds.
const EVENTS: [&str; 6] = [
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

/// The empty catch-all matcher CC writes for matcher-less events; we append our
/// entry into this group so it fires for every tool / event.
const CATCH_ALL_MATCHER: &str = "";

/// The forwarder script template (substitutes `{{FORWARD_BIN}}` at install).
/// The canonical copy lives at `assets/hook-forward.sh`.
const HOOK_SCRIPT_TEMPLATE: &str = include_str!("../../assets/hook-forward.sh");

/// `~/.claude/settings.json` — the file whose `hooks` key we chain into.
fn settings_path() -> Result<PathBuf> {
    Ok(claude_dir()?.join("settings.json"))
}

/// The installed forwarder script path,
/// `~/.local/state/claude-presence/hook-forward.sh`.
///
/// Lives under the same `0700` state dir as the daemon socket and logs; CC
/// invokes it as the hook command, and its absolute path is our entry's identity.
pub fn installed_script_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".local")
        .join("state")
        .join("claude-presence")
        .join("hook-forward.sh"))
}

/// Absolute path of the current executable, canonicalized — baked into the
/// forwarder script as `{{FORWARD_BIN}}` so CC (which injects no env var) can
/// reach the exact binary that ran `install`.
fn current_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// The exact `command` string our hook entries carry — the identity uninstall
/// matches on. It is the absolute path to the installed forwarder script,
/// **single-quoted** so a path containing a space or shell metacharacter is inert
/// when Claude Code runs the command through a shell (F23, ADR-3). The
/// `forward --kind hook` invocation happens *inside* that script.
///
/// Install always writes this quoted form, so it round-trips exactly. Uninstall and
/// the wiring count, however, also recognize the **legacy bare** unquoted path
/// ([`installed_script_path`]) that every pre-quoting release (v0.1.0–v0.1.2 + the
/// Homebrew formula) wrote into all six events — see [`entry_matches_any`] — so an
/// upgrade-then-uninstall leaves no dangling legacy entry (ADR-3 migration, NFR-4).
/// A same-named-but-*different* command is still never matched.
pub fn hook_command(script: &Path) -> String {
    shell_single_quote(&script.to_string_lossy())
}

/// Wrap `s` in single quotes, escaping any embedded single quote as `'\''`
/// (mirrors `statusline::shell_single_quote`).
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

/// Render the forwarder script from the resolved binary path (pure).
pub fn render_script(binary: &Path) -> String {
    HOOK_SCRIPT_TEMPLATE.replace("{{FORWARD_BIN}}", &binary.to_string_lossy())
}

/// How many of the chained lifecycle [`EVENTS`] currently carry our forwarder
/// entry in `~/.claude/settings.json` (the `doctor` settings-wiring check,
/// FR-8/AC-2). Returns `(present, total)` so `doctor` can report e.g. "6/6".
///
/// The pure decision lives in [`wired_event_count`]; this reads the real settings
/// file, treating a missing/absent file as zero-wired rather than an error.
pub fn wired_count() -> Result<(usize, usize)> {
    let script_path = installed_script_path()?;
    let our_command = hook_command(&script_path);
    let bare = script_path.to_string_lossy().into_owned();
    let settings = read_settings(&settings_path()?)?;
    Ok((
        wired_event_count(&settings, &our_command, &bare),
        EVENTS.len(),
    ))
}

/// Pure: count the [`EVENTS`] whose `settings.json` groups contain our entry,
/// matched by identity in any matcher group of that event. An entry counts when its
/// `command` equals **either** the current quoted form or the legacy bare path a
/// pre-quoting install wrote (ADR-3 migration), so `doctor` reports an upgraded but
/// not-yet-reinstalled install as wired.
fn wired_event_count(settings: &Value, our_command: &str, bare: &str) -> usize {
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return 0;
    };
    EVENTS
        .iter()
        .filter(|event| {
            hooks
                .get(**event)
                .and_then(Value::as_array)
                .is_some_and(|groups| {
                    groups
                        .iter()
                        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                        .flatten()
                        .any(|e| entry_matches_any(e, our_command, bare))
                })
        })
        .count()
}

/// Chain-install the hook forwarder into `~/.claude/settings.json` (FR-4/AC-1).
///
/// Writes the forwarder script (`0700`, executable) under the state dir, then
/// appends our entry into every event group of `settings.json`, preserving all
/// user entries and creating the `hooks` map / event groups as needed. Idempotent:
/// re-running never adds a duplicate (a settings file that already carries our
/// exact entry is left unchanged). A missing settings file is treated as an empty
/// object so a first-time install still works.
pub fn install() -> Result<()> {
    let binary = current_binary()?;
    let script_path = installed_script_path()?;
    write_script(&script_path, &render_script(&binary))?;

    let our_command = hook_command(&script_path);
    let bare = script_path.to_string_lossy().into_owned();
    let path = settings_path()?;
    let settings = read_settings(&path)?;
    let updated = apply_install(settings, &our_command, &bare);
    write_settings(&path, &updated)?;
    tracing::info!(target: "install", "hooks chained into settings.json");
    Ok(())
}

/// Remove our exact hook entry from every event group (FR-4/AC-1, NFR-6).
///
/// Matches **only** by the exact command string identity, so user entries (even a
/// same-named-but-different command) are never touched. Empty groups/maps that we
/// emptied are cleaned up conservatively (see [`apply_uninstall`]). The forwarder
/// script is removed. Idempotent: a settings file without our entry, or a missing
/// file, is a no-op success.
pub fn uninstall() -> Result<()> {
    let script_path = installed_script_path()?;
    let our_command = hook_command(&script_path);

    let bare = script_path.to_string_lossy().into_owned();
    let path = settings_path()?;
    if path.exists() {
        let settings = read_settings(&path)?;
        let updated = apply_uninstall(settings, &our_command, &bare);
        write_settings(&path, &updated)?;
    }

    match std::fs::remove_file(&script_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::Io(e)),
    }
    tracing::info!(target: "install", "hooks unchained from settings.json");
    Ok(())
}

/// Pure transform: append our command entry into each event's catch-all group.
///
/// For every event in [`EVENTS`]:
/// * ensure `settings.hooks` is an object and `settings.hooks[<Event>]` an array
///   of matcher groups;
/// * find (or create) the empty-matcher (`""`) catch-all group;
/// * if the group already carries a **legacy bare** entry (the unquoted path a
///   pre-quoting install wrote), normalize it in place to the quoted `our_command`
///   — so an upgrade re-install migrates the bare form rather than appending a
///   duplicate beside it (ADR-3 migration);
/// * otherwise append `{ "type": "command", "command": our_command }` into that
///   group's `hooks[]` **unless an entry with the (quoted or bare) command already
///   exists** (idempotent).
///
/// Only the quoted form is ever written. All existing user groups and entries —
/// including a same-named-but-different command — are preserved untouched.
pub fn apply_install(mut settings: Value, our_command: &str, bare: &str) -> Value {
    let hooks = ensure_object_at(&mut settings, "hooks");
    for event in EVENTS {
        let groups = ensure_array_at(hooks, event);
        let group = ensure_catch_all_group(groups);
        let entries = ensure_array_at(group, "hooks");
        if let Some(legacy) = entries
            .iter_mut()
            .find(|e| entry_matches(e, bare) && !entry_matches(e, our_command))
        {
            // Migrate the legacy bare entry to the quoted identity in place.
            if let Some(obj) = legacy.as_object_mut() {
                obj.insert("command".into(), Value::String(our_command.to_owned()));
            }
        } else if !entries
            .iter()
            .any(|e| entry_matches_any(e, our_command, bare))
        {
            entries.push(our_entry(our_command));
        }
    }
    settings
}

/// Pure transform: remove our command entry from every event group.
///
/// Scans **all** matcher groups of every event (not just the catch-all, in case a
/// prior version installed elsewhere) and drops entries whose `command` equals
/// **either** the current quoted `our_command` or the legacy bare path a pre-quoting
/// release wrote (ADR-3 migration) — so an upgrade-then-uninstall leaves no dangling
/// legacy entry. A group whose `hooks[]` we emptied is removed; an event whose groups
/// all became empty is removed; an emptied `hooks` map is removed. User entries —
/// including a same-named-but-different command — are never touched.
pub fn apply_uninstall(mut settings: Value, our_command: &str, bare: &str) -> Value {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return settings;
    };

    let mut empty_events: Vec<String> = Vec::new();
    for (event, groups_val) in hooks.iter_mut() {
        let Some(groups) = groups_val.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                entries.retain(|e| !entry_matches_any(e, our_command, bare));
            }
        }
        // Drop only groups WE emptied (no remaining entries); never delete a
        // group that still holds user entries.
        groups.retain(|g| !group_is_empty(g));
        if groups.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
    settings
}

/// Our entry: `{ "type": "command", "command": <our_command> }`.
fn our_entry(our_command: &str) -> Value {
    json!({ "type": "command", "command": our_command })
}

/// True if `entry` is a command entry whose `command` equals `our_command`.
fn entry_matches(entry: &Value, our_command: &str) -> bool {
    entry.get("command").and_then(Value::as_str) == Some(our_command)
}

/// True if `entry`'s `command` equals **either** our current quoted identity
/// (`quoted`) or the legacy bare unquoted path (`bare`) that a pre-quoting release
/// wrote. Used so uninstall / the wiring count recognize an upgraded-but-not-yet-
/// reinstalled install and never leave a dangling legacy entry (ADR-3 migration).
fn entry_matches_any(entry: &Value, quoted: &str, bare: &str) -> bool {
    match entry.get("command").and_then(Value::as_str) {
        Some(cmd) => cmd == quoted || cmd == bare,
        None => false,
    }
}

/// True if a matcher group has no remaining hook entries (so it is safe to drop
/// when WE were the only entry).
fn group_is_empty(group: &Value) -> bool {
    match group.get("hooks").and_then(Value::as_array) {
        Some(entries) => entries.is_empty(),
        // A group with no `hooks` key at all is degenerate; treat as empty.
        None => true,
    }
}

/// Ensure `parent[key]` is an object, replacing a non-object, and return it.
///
/// Callers always pass an object root (`read_settings` guarantees it), but a degenerate
/// non-object `parent` is normalized to an empty object **in place** first, so the
/// `Value::Object` match below is total — there is no runtime panic path (F24).
fn ensure_object_at<'a>(parent: &'a mut Value, key: &str) -> &'a mut Value {
    if !parent.is_object() {
        *parent = json!({});
    }
    // `parent` is now an object; matching it directly keeps this total (no `expect`).
    let Value::Object(obj) = parent else {
        // Unreachable after the normalization above; normalize once more, no panic.
        *parent = json!({ key: {} });
        return &mut parent[key];
    };
    let slot = obj.entry(key).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot
}

/// Ensure `parent[key]` is an array, replacing a non-array, and return it.
///
/// `parent` is normalized to an object **in place** first (see [`ensure_object_at`]),
/// then the key is normalized to an array, so both matches are total — a degenerate
/// non-object/non-array shape is normalized rather than panicking (F24).
fn ensure_array_at<'a>(parent: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    if !parent.is_object() {
        *parent = json!({});
    }
    let Value::Object(obj) = parent else {
        *parent = json!({ key: [] });
        let Value::Array(v) = &mut parent[key] else {
            unreachable!("just inserted an array under key")
        };
        return v;
    };
    let slot = obj.entry(key).or_insert_with(|| json!([]));
    if !slot.is_array() {
        *slot = json!([]);
    }
    // `slot` is now an array; matching it directly keeps this total (no `expect`).
    let Value::Array(v) = slot else {
        unreachable!("slot normalized to an array above")
    };
    v
}

/// Find the empty-matcher catch-all group, creating it if absent, and return it.
fn ensure_catch_all_group(groups: &mut Vec<Value>) -> &mut Value {
    let pos = groups
        .iter()
        .position(|g| g.get("matcher").and_then(Value::as_str) == Some(CATCH_ALL_MATCHER));
    let idx = match pos {
        Some(i) => i,
        None => {
            groups.push(json!({ "matcher": CATCH_ALL_MATCHER, "hooks": [] }));
            groups.len() - 1
        }
    };
    &mut groups[idx]
}

/// Read `settings.json` into a `Value`, treating a missing file as an empty object
/// so a first-time install still works. A non-object root is rejected (rather than
/// silently overwritten with `{}`) so we never destroy the user's file — symmetric
/// with `statusline::read_settings`.
fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)?;
            if value.is_object() {
                Ok(value)
            } else {
                Err(Error::Other(
                    "settings.json is not a JSON object; refusing to modify".into(),
                ))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Write `settings` back as pretty JSON (2-space, CC's own format) with a trailing
/// newline (matching `statusline::write_settings`), atomically and durably.
fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    write_atomic(path, &text)?;
    Ok(())
}

/// Write `contents` to `path` atomically and durably: stream into a sibling
/// `<file-name>.tmp`, `fsync` it, then `rename` over `path`. The rename is atomic
/// within a filesystem, so a crash / power-loss / ENOSPC mid-write can never leave
/// the user's `settings.json` truncated or half-written (C-6, F26, NFR-6). The `.tmp`
/// suffix is *appended* to the full file name (not substituted for the extension) so
/// it is correct for any target, including an extensionless or non-`.json` path
/// (mirrors `statusline::write_atomic`).
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write the forwarder script `0700` (executable, owner-only) under the state dir.
fn write_script(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir_0700(parent)?;
    }
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The raw, unquoted absolute forwarder-script path.
    const BARE: &str = "/home/u/.local/state/claude-presence/hook-forward.sh";
    /// The identity our hook entries actually carry: the bare path wrapped in
    /// `shell_single_quote` (F23, ADR-3). Install AND uninstall match on this exact
    /// quoted form, so the round-trip is preserved.
    const OUR: &str = "'/home/u/.local/state/claude-presence/hook-forward.sh'";

    /// The real user Stop entry from the author's settings.json — the fixture the
    /// task requires we preserve byte-for-byte.
    fn afplay_entry() -> Value {
        json!({ "type": "command", "command": "afplay /System/Library/Sounds/Submarine.aiff" })
    }

    fn settings_with_afplay_stop() -> Value {
        json!({
            "model": "Opus",
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [ afplay_entry() ] }
                ]
            }
        })
    }

    /// Collect every command string under `hooks[event]` (across all groups).
    fn commands_for(settings: &Value, event: &str) -> Vec<String> {
        settings["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .filter_map(|g| g.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|e| e.get("command").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn fixture_install_appends_and_preserves_afplay() {
        // The headline acceptance test (FR-4/AC-1, C-6): install must APPEND our
        // entry into the existing Stop group while keeping the user's afplay hook.
        let before = settings_with_afplay_stop();
        let after = apply_install(before, OUR, BARE);

        let stop = commands_for(&after, "Stop");
        assert!(
            stop.contains(&"afplay /System/Library/Sounds/Submarine.aiff".to_string()),
            "afplay entry must be preserved: {stop:?}"
        );
        assert!(
            stop.contains(&OUR.to_string()),
            "our entry must be appended"
        );
        // Both live in the SAME empty-matcher group (chained, not a new group).
        let stop_groups = after["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop_groups.len(), 1, "must reuse the existing group");
        assert_eq!(
            stop_groups[0]["hooks"].as_array().unwrap().len(),
            2,
            "afplay + ours"
        );
    }

    #[test]
    fn fixture_uninstall_removes_only_ours_keeps_afplay_byte_for_byte() {
        let installed = apply_install(settings_with_afplay_stop(), OUR, BARE);
        let reverted = apply_uninstall(installed, OUR, BARE);

        // Our entry is gone; the afplay group survives unchanged.
        let stop = commands_for(&reverted, "Stop");
        assert!(
            !stop.contains(&OUR.to_string()),
            "our entry must be removed"
        );
        assert_eq!(stop, vec!["afplay /System/Library/Sounds/Submarine.aiff"]);

        // Byte-for-byte: the Stop event equals the original user shape exactly.
        let expected = settings_with_afplay_stop();
        assert_eq!(
            reverted["hooks"]["Stop"], expected["hooks"]["Stop"],
            "the user's Stop group must be restored exactly"
        );
    }

    #[test]
    fn install_creates_group_when_event_absent() {
        // SessionStart is not present in the fixture → install creates the group.
        let after = apply_install(settings_with_afplay_stop(), OUR, BARE);
        for event in EVENTS {
            let cmds = commands_for(&after, event);
            assert!(
                cmds.contains(&OUR.to_string()),
                "{event} must carry our entry: {cmds:?}"
            );
        }
        // A created group uses the empty catch-all matcher, matching CC's format.
        let ss = after["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(ss.len(), 1);
        assert_eq!(ss[0]["matcher"], json!(""));
    }

    #[test]
    fn install_from_empty_settings() {
        let after = apply_install(json!({}), OUR, BARE);
        for event in EVENTS {
            assert_eq!(commands_for(&after, event), vec![OUR.to_string()]);
        }
    }

    #[test]
    fn install_is_idempotent() {
        let once = apply_install(json!({}), OUR, BARE);
        let twice = apply_install(once.clone(), OUR, BARE);
        assert_eq!(once, twice, "a second install must not add a duplicate");
        for event in EVENTS {
            assert_eq!(
                commands_for(&twice, event),
                vec![OUR.to_string()],
                "no duplicate in {event}"
            );
        }
    }

    #[test]
    fn uninstall_leaves_same_named_different_command_intact() {
        // Remove-by-identity is on the EXACT command string: a user hook that
        // forwards to a *different* path must survive uninstall.
        let user_command = "/somewhere/else/hook-forward.sh";
        let settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "", "hooks": [
                        { "type": "command", "command": OUR },
                        { "type": "command", "command": user_command }
                    ] }
                ]
            }
        });
        let reverted = apply_uninstall(settings, OUR, BARE);
        let cmds = commands_for(&reverted, "PreToolUse");
        assert_eq!(
            cmds,
            vec![user_command.to_string()],
            "only our exact command is removed"
        );
    }

    #[test]
    fn uninstall_is_idempotent_and_noop_without_our_entry() {
        // No hooks at all → unchanged.
        let plain = json!({ "model": "Opus" });
        assert_eq!(apply_uninstall(plain.clone(), OUR, BARE), plain);

        // Only a user entry → untouched, including the empty `hooks` not stripped.
        let user_only = settings_with_afplay_stop();
        assert_eq!(apply_uninstall(user_only.clone(), OUR, BARE), user_only);
    }

    #[test]
    fn uninstall_drops_groups_and_map_we_created() {
        // Installing into empty settings then uninstalling must return to `{}`
        // for hooks: a group/event/map we created and emptied is removed.
        let installed = apply_install(json!({ "model": "Opus" }), OUR, BARE);
        let reverted = apply_uninstall(installed, OUR, BARE);
        assert_eq!(
            reverted,
            json!({ "model": "Opus" }),
            "everything we created must be cleaned up"
        );
    }

    #[test]
    fn wired_event_count_reflects_installed_events() {
        // Nothing installed → zero wired.
        assert_eq!(wired_event_count(&json!({ "model": "Opus" }), OUR, BARE), 0);

        // A full install → all EVENTS wired.
        let installed = apply_install(json!({}), OUR, BARE);
        assert_eq!(wired_event_count(&installed, OUR, BARE), EVENTS.len());

        // A partial install (our entry in only one event) → exactly one wired,
        // and a user's same-named-but-different command does not count.
        let partial = json!({
            "hooks": {
                "Stop": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": OUR }
                ] } ],
                "PreToolUse": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": "/other/hook.sh" }
                ] } ]
            }
        });
        assert_eq!(wired_event_count(&partial, OUR, BARE), 1);
    }

    #[test]
    fn render_script_bakes_in_binary_path() {
        let script = render_script(Path::new("/usr/local/bin/claude-presence"));
        assert!(script.contains("/usr/local/bin/claude-presence forward --kind hook"));
        assert!(
            !script.contains("{{FORWARD_BIN}}"),
            "placeholder substituted"
        );
    }

    #[test]
    fn hook_command_is_the_quoted_script_path() {
        // The identity is the absolute path single-quoted (F23, ADR-3), so a path
        // with a space/metacharacter is inert when CC runs it through a shell.
        let cmd = hook_command(Path::new(BARE));
        assert_eq!(cmd, OUR);
        assert_eq!(cmd, format!("'{BARE}'"));

        // A path containing a space and a single quote round-trips through the quoting.
        let spaced = "/Users/a b/state/it's/hook-forward.sh";
        let quoted = hook_command(Path::new(spaced));
        assert_eq!(quoted, "'/Users/a b/state/it'\\''s/hook-forward.sh'");
    }

    #[test]
    fn install_uninstall_round_trip_on_quoted_identity() {
        // The headline reversibility guarantee for the QUOTED form (FR-3/AC-1, NFR-4):
        // install appends the quoted identity, uninstall removes exactly that and
        // restores the user's afplay group byte-for-byte.
        let before = settings_with_afplay_stop();
        let installed = apply_install(before, OUR, BARE);

        // Our entry is present in its quoted form (never a bare path).
        let stop = commands_for(&installed, "Stop");
        assert!(stop.contains(&OUR.to_string()), "quoted identity appended");
        assert!(
            !stop.contains(&BARE.to_string()),
            "the bare unquoted path is never written as our entry"
        );

        let reverted = apply_uninstall(installed, OUR, BARE);
        assert_eq!(
            reverted["hooks"]["Stop"],
            settings_with_afplay_stop()["hooks"]["Stop"],
            "uninstalling the quoted identity restores the user's group exactly"
        );
    }

    #[test]
    fn upgrade_then_uninstall_cleans_legacy_bare_entries() {
        // ADR-3 migration / FR-3/AC-1: every pre-quoting release (v0.1.0–v0.1.2 + the
        // Homebrew formula) wrote the BARE unquoted path into all six events. After a
        // user UPGRADES and runs uninstall, those legacy entries must be fully cleaned
        // (no dangling) while a user afplay hook (and any same-named-but-different
        // command) survives byte-for-byte.
        let user_command = "/somewhere/else/hook-forward.sh";
        let legacy = json!({
            "model": "Opus",
            "hooks": {
                "SessionStart": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": BARE }
                ] } ],
                "PreToolUse": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": BARE }
                ] } ],
                "PostToolUse": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": BARE }
                ] } ],
                "Stop": [ { "matcher": "", "hooks": [
                    afplay_entry(),
                    { "type": "command", "command": BARE }
                ] } ],
                "SubagentStart": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": BARE }
                ] } ],
                "SubagentStop": [ { "matcher": "", "hooks": [
                    { "type": "command", "command": BARE },
                    { "type": "command", "command": user_command }
                ] } ]
            }
        });

        let reverted = apply_uninstall(legacy, OUR, BARE);

        // No legacy bare entry survives anywhere.
        for event in EVENTS {
            let cmds = commands_for(&reverted, event);
            assert!(
                !cmds.contains(&BARE.to_string()),
                "legacy bare entry must be cleaned from {event}: {cmds:?}"
            );
            assert!(
                !cmds.contains(&OUR.to_string()),
                "no quoted entry was present to remove in {event}: {cmds:?}"
            );
        }

        // The user's afplay Stop hook survives byte-for-byte, in its own group.
        assert_eq!(
            commands_for(&reverted, "Stop"),
            vec!["afplay /System/Library/Sounds/Submarine.aiff"],
            "the afplay hook must be preserved"
        );
        // The same-named-but-different user command survives.
        assert_eq!(
            commands_for(&reverted, "SubagentStop"),
            vec![user_command.to_string()],
            "a different command must never be removed"
        );
    }

    #[test]
    fn reinstall_over_legacy_bare_is_idempotent_and_normalizes_to_quoted() {
        // After an UPGRADE, re-running install over the legacy bare entries must
        // migrate them to the quoted identity in place — never append a duplicate
        // beside the bare form (ADR-3 migration, idempotent across the upgrade).
        let legacy = json!({
            "model": "Opus",
            "hooks": {
                "Stop": [ { "matcher": "", "hooks": [
                    afplay_entry(),
                    { "type": "command", "command": BARE }
                ] } ]
            }
        });

        let migrated = apply_install(legacy, OUR, BARE);
        let stop = commands_for(&migrated, "Stop");
        assert_eq!(
            stop,
            vec![
                "afplay /System/Library/Sounds/Submarine.aiff".to_string(),
                OUR.to_string()
            ],
            "the legacy bare entry is normalized to quoted in place; no duplicate, afplay preserved"
        );
        assert!(
            !stop.contains(&BARE.to_string()),
            "the bare form must not remain after migration"
        );

        // Running install AGAIN is a no-op (already quoted) — fully idempotent.
        let again = apply_install(migrated.clone(), OUR, BARE);
        assert_eq!(again, migrated, "a second install must not add a duplicate");

        // And uninstall now removes the migrated quoted entry, restoring the user's group.
        let reverted = apply_uninstall(migrated, OUR, BARE);
        assert_eq!(
            commands_for(&reverted, "Stop"),
            vec!["afplay /System/Library/Sounds/Submarine.aiff"]
        );
    }

    #[test]
    fn apply_install_does_not_panic_on_degenerate_settings() {
        // F24: callers guarantee an object root via `read_settings`, but apply_install
        // must NEVER panic even on a degenerate shape — it normalizes instead.
        // A non-object root.
        let from_array = apply_install(json!([1, 2, 3]), OUR, BARE);
        for event in EVENTS {
            assert_eq!(commands_for(&from_array, event), vec![OUR.to_string()]);
        }

        // `hooks` present but a non-object (number) — normalized, not panicked.
        let bad_hooks = apply_install(json!({ "hooks": 42 }), OUR, BARE);
        for event in EVENTS {
            assert_eq!(commands_for(&bad_hooks, event), vec![OUR.to_string()]);
        }

        // An event whose value is a non-array (string), and a group whose `hooks`
        // is a non-array — both normalized in place without panic.
        let bad_event = apply_install(
            json!({ "hooks": { "Stop": "nope", "PreToolUse": [ { "matcher": "", "hooks": 7 } ] } }),
            OUR,
            BARE,
        );
        assert!(commands_for(&bad_event, "Stop").contains(&OUR.to_string()));
        assert!(commands_for(&bad_event, "PreToolUse").contains(&OUR.to_string()));
    }

    #[test]
    fn installed_script_path_is_under_state_dir() {
        let path = installed_script_path().unwrap();
        assert!(path.is_absolute());
        assert!(path.ends_with(".local/state/claude-presence/hook-forward.sh"));
    }

    #[test]
    fn write_atomic_round_trips_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("cp-hooks-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let contents = "{\n  \"model\": \"Opus\"\n}\n";

        write_atomic(&path, contents).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
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
        // A settings.json whose top-level value is an array must be refused, never
        // silently overwritten with `{}` (which would destroy the user's file).
        let dir = std::env::temp_dir().join(format!("cp-hooks-nonobj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "[1,2,3]").unwrap();

        let err = read_settings(&path).unwrap_err();
        assert!(
            matches!(err, Error::Other(msg) if msg.contains("not a JSON object")),
            "a non-object root must be refused, not normalized to {{}}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
