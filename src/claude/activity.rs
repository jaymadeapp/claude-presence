//! Activity mapping: a `tool_use` (tool name + sanitized input) → an [`Activity`]
//! (verb, optional target, badge key) for the Discord card (task 1.4).
//!
//! Privacy is the whole point of this module (C-7 / FR-7/AC-2). The **default**
//! mapping emits a sanitized *verb only* and, where useful, a single sanitized
//! *target* (a program name, a path basename, or an mcp server name) — never the
//! raw tool arguments. In particular Bash drops every argument and keeps only the
//! first command token (`cargo check --foo` → `Running cargo`), so a command like
//! `curl -H 'Authorization: Bearer sk-FAKE'` can never leak a secret into
//! `details`.
//!
//! Showing a fuller Bash command is strictly opt-in behind `scrub_bash_args`, and
//! even then it is routed through [`crate::privacy::scrub_bash_command`], which
//! strips token/key/secret/password/`Authorization` patterns, `WORD=value`
//! env-assignments, credentialed URLs, and long base64/hex blobs before
//! truncating. This module never deserializes or formats raw argument text on its
//! own.
//!
//! Consumed by the transcript watcher and the ingest socket (hooks).

use std::path::Path;

use serde_json::Value;

use crate::config::Config;
use crate::privacy;
use crate::state::model::Activity;

/// Per-tool Discord `small_image` badge keys (design §4.3). These are asset keys
/// the user uploads in the Developer Portal; an unknown tool falls back to the
/// generic `tool` badge.
mod badge {
    pub const BASH: &str = "bash";
    pub const EDIT: &str = "edit";
    pub const READ: &str = "read";
    pub const SEARCH: &str = "search";
    pub const AGENTS: &str = "agents";
    pub const TOOL: &str = "tool";
}

/// Map a `tool_use` block to a sanitized [`Activity`] for the Discord card.
///
/// `tool_name` is the raw tool name (e.g. `Bash`, `Edit`, `mcp__foo__bar`) and
/// `input` is its (untrusted) argument object straight off the transcript/hook
/// adapter. Only sanitized, structured fields are read out of `input`; raw values
/// never reach the returned [`Activity`].
///
/// The default mapping (design §4.3, FR-2/AC-2):
///
/// | Tool | Verb | Target |
/// |---|---|---|
/// | `Bash` | `Running` | first command token only (args dropped) |
/// | `Edit` / `Write` | `Editing` | `file_path` basename |
/// | `Read` | `Reading` | `file_path` basename |
/// | `Grep` / `Glob` | `Searching` | — |
/// | `Agent` / `Task` | `Orchestrating agents` | — |
/// | `mcp__<server>__*` | `Using` | `<server>` |
/// | anything else | the tool name | — |
///
/// When `cfg.privacy.scrub_bash_args` is set, `Bash` additionally exposes a
/// scrubbed command (via [`crate::privacy::scrub_bash_command`]) as the target
/// instead of just the first token.
pub fn map_activity(tool_name: &str, input: Option<&Value>, cfg: &Config) -> Activity {
    // mcp tools are named `mcp__<server>__<tool>`; surface the server only.
    if let Some(server) = mcp_server(tool_name) {
        return Activity {
            verb: "Using".to_string(),
            target: Some(server.to_string()),
            small_image_key: Some(badge::TOOL.to_string()),
        };
    }

    // The verbs below are fixed by FR-2/AC-2 and are NOT subject to the
    // `tool_verbs` override — the spec pins them so the card reads consistently.
    // `tool_verbs` only customizes verbs for tools this mapping doesn't recognize.
    match tool_name {
        "Bash" => bash_activity(input, cfg),
        "Edit" | "Write" => Activity {
            verb: "Editing".to_string(),
            target: path_target(input, cfg),
            small_image_key: Some(badge::EDIT.to_string()),
        },
        "Read" => Activity {
            verb: "Reading".to_string(),
            target: path_target(input, cfg),
            small_image_key: Some(badge::READ.to_string()),
        },
        "Grep" | "Glob" => Activity {
            verb: "Searching".to_string(),
            target: None,
            small_image_key: Some(badge::SEARCH.to_string()),
        },
        "Agent" | "Task" => Activity {
            verb: "Orchestrating agents".to_string(),
            target: None,
            small_image_key: Some(badge::AGENTS.to_string()),
        },
        // Unknown tool: a bare, sanitized verb (a user `tool_verbs` override or
        // the tool name itself) with the generic badge — never any argument text.
        other => Activity {
            verb: verb_for(other, other, cfg),
            target: None,
            small_image_key: Some(badge::TOOL.to_string()),
        },
    }
}

/// Build the `Bash` activity. The default keeps only the first command token
/// (program name) and drops every argument; `scrub_bash_args` opts into a
/// scrubbed command via [`crate::privacy::scrub_bash_command`].
fn bash_activity(input: Option<&Value>, cfg: &Config) -> Activity {
    let command = str_field(input, "command");

    let target = match command {
        Some(cmd) if cfg.privacy.scrub_bash_args => {
            // Opt-in: a fuller command, but only ever the scrubbed form.
            privacy::scrub_bash_command(cmd, true).or_else(|| first_token(cmd))
        }
        // Default: the program name only — arguments are dropped entirely.
        Some(cmd) => first_token(cmd),
        None => None,
    };

    Activity {
        verb: "Running".to_string(),
        target,
        small_image_key: Some(badge::BASH.to_string()),
    }
}

/// Resolve the display verb for an unrecognized tool: a user `tool_verbs`
/// override (capitalized) wins, otherwise the tool name itself. Verbs are static
/// strings, never derived from tool input.
fn verb_for(tool_name: &str, default: &str, cfg: &Config) -> String {
    match cfg.tool_verbs.get(tool_name) {
        Some(custom) => capitalize(custom),
        None => default.to_string(),
    }
}

/// Sanitized basename target from a tool's `file_path`, honoring the blacklist
/// and the master redaction switch (no path leaves as anything but a basename).
fn path_target(input: Option<&Value>, cfg: &Config) -> Option<String> {
    let raw = str_field(input, "file_path")?;
    let path = Path::new(raw);

    if cfg.privacy.redact || privacy::is_blacklisted(path, &cfg.privacy.blacklist_paths) {
        return None;
    }
    let name = privacy::basename(path);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The mcp server name for a `mcp__<server>__<tool>` tool, else `None`.
fn mcp_server(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("mcp__")?
        .split("__")
        .next()
        .filter(|s| !s.is_empty())
}

/// First whitespace-delimited token of a command (the program name); `None` if
/// the command is blank. Only the token itself is returned — no arguments.
fn first_token(command: &str) -> Option<String> {
    command.split_whitespace().next().map(str::to_string)
}

/// Borrow a string field out of an untrusted tool-input object without cloning
/// the whole value. Returns `None` when absent or not a string.
fn str_field<'a>(input: Option<&'a Value>, key: &str) -> Option<&'a str> {
    input?.get(key)?.as_str()
}

/// Capitalize the first character of a verb (config verbs may be lowercase, e.g.
/// the default `tool_verbs` map uses `"running"`); the rest is left untouched.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Config with bash-arg scrubbing enabled (everything else default).
    fn cfg_scrub() -> Config {
        let mut cfg = Config::default();
        cfg.privacy.scrub_bash_args = true;
        cfg
    }

    /// Config that does not redact targets, so basenames are surfaced.
    fn cfg_show_paths() -> Config {
        let mut cfg = Config::default();
        cfg.privacy.redact = false;
        cfg
    }

    #[test]
    fn bash_keeps_only_the_program_token_by_default() {
        let cfg = Config::default();
        let input = json!({ "command": "cargo check --all-features" });
        let act = map_activity("Bash", Some(&input), &cfg);
        assert_eq!(act.verb, "Running");
        assert_eq!(act.target.as_deref(), Some("cargo"));
        assert_eq!(act.small_image_key.as_deref(), Some("bash"));
    }

    #[test]
    fn bash_never_leaks_a_token_by_default() {
        // The core privacy guarantee (FR-7/AC-2): a fake secret in the command
        // must NOT appear anywhere in the resulting activity by default.
        let cfg = Config::default();
        let input = json!({ "command": "curl -H 'Authorization: Bearer sk-FAKE' https://api" });
        let act = map_activity("Bash", Some(&input), &cfg);

        assert_eq!(act.target.as_deref(), Some("curl"));
        let rendered = format!("{} {:?}", act.verb, act.target);
        assert!(!rendered.contains("sk-FAKE"), "{rendered}");
        assert!(!rendered.contains("Authorization"), "{rendered}");
        assert!(!rendered.contains("Bearer"), "{rendered}");
    }

    #[test]
    fn bash_scrub_opt_in_still_strips_secrets() {
        // Even with scrub_bash_args on, the secret is removed (it routes through
        // privacy::scrub_bash_command), and we never see the raw token.
        let input = json!({ "command": "curl -H Authorization=Bearer-sk-FAKE-token-value-1234567890 host" });
        let act = map_activity("Bash", Some(&input), &cfg_scrub());
        let target = act.target.expect("scrubbed command present");
        assert!(!target.contains("sk-FAKE"), "{target}");
        // The program name still survives so the card is informative.
        assert!(target.contains("curl"), "{target}");
    }

    #[test]
    fn bash_with_no_command_has_no_target() {
        let cfg = Config::default();
        let act = map_activity("Bash", Some(&json!({})), &cfg);
        assert_eq!(act.verb, "Running");
        assert_eq!(act.target, None);
    }

    #[test]
    fn edit_and_read_use_basename_only() {
        let cfg = cfg_show_paths();
        let input = json!({ "file_path": "/Users/secret/Projects/private/src/main.rs" });

        let edit = map_activity("Edit", Some(&input), &cfg);
        assert_eq!(edit.verb, "Editing");
        assert_eq!(edit.target.as_deref(), Some("main.rs"));
        assert_eq!(edit.small_image_key.as_deref(), Some("edit"));

        let write = map_activity("Write", Some(&input), &cfg);
        assert_eq!(write.verb, "Editing");
        assert_eq!(write.target.as_deref(), Some("main.rs"));

        let read = map_activity("Read", Some(&input), &cfg);
        assert_eq!(read.verb, "Reading");
        assert_eq!(read.target.as_deref(), Some("main.rs"));
        assert_eq!(read.small_image_key.as_deref(), Some("read"));
    }

    #[test]
    fn paths_are_dropped_when_redacting() {
        // With global private mode on, no basename target leaks (the default is
        // off — an informative card — so set redact explicitly here).
        let mut cfg = Config::default();
        cfg.privacy.redact = true;
        let input = json!({ "file_path": "/Users/x/private/secret.rs" });
        let act = map_activity("Read", Some(&input), &cfg);
        assert_eq!(act.verb, "Reading");
        assert_eq!(act.target, None);
    }

    #[test]
    fn basename_target_shown_by_default() {
        // Product goal: out of the box (redact off, no blacklist) the activity
        // target surfaces the basename (still path-sanitized to basename-only).
        let cfg = Config::default();
        assert!(!cfg.privacy.redact);
        let input = json!({ "file_path": "/Users/x/Projects/demo/src/main.rs" });
        let act = map_activity("Read", Some(&input), &cfg);
        assert_eq!(act.verb, "Reading");
        assert_eq!(act.target.as_deref(), Some("main.rs"));
    }

    #[test]
    fn blacklisted_path_target_is_dropped() {
        let mut cfg = cfg_show_paths();
        cfg.privacy.blacklist_paths = vec![std::path::PathBuf::from("/Users/x/private")];
        let input = json!({ "file_path": "/Users/x/private/secret.rs" });
        let act = map_activity("Edit", Some(&input), &cfg);
        assert_eq!(act.target, None, "blacklisted path must not surface");
    }

    #[test]
    fn search_and_orchestrate_have_no_target() {
        let cfg = Config::default();
        for tool in ["Grep", "Glob"] {
            let act = map_activity(tool, Some(&json!({ "pattern": "secret" })), &cfg);
            assert_eq!(act.verb, "Searching");
            assert_eq!(act.target, None);
            assert_eq!(act.small_image_key.as_deref(), Some("search"));
        }
        for tool in ["Agent", "Task"] {
            let act = map_activity(tool, Some(&json!({ "prompt": "do secret thing" })), &cfg);
            assert_eq!(act.verb, "Orchestrating agents");
            assert_eq!(act.target, None);
            assert_eq!(act.small_image_key.as_deref(), Some("agents"));
        }
    }

    #[test]
    fn mcp_tool_extracts_server_name() {
        let cfg = Config::default();
        let act = map_activity("mcp__github__create_issue", Some(&json!({})), &cfg);
        assert_eq!(act.verb, "Using");
        assert_eq!(act.target.as_deref(), Some("github"));
        assert_eq!(act.small_image_key.as_deref(), Some("tool"));
    }

    #[test]
    fn mcp_server_extraction_unit() {
        assert_eq!(mcp_server("mcp__github__create_issue"), Some("github"));
        assert_eq!(mcp_server("mcp__slack__post"), Some("slack"));
        // Server with no trailing tool still extracts the server.
        assert_eq!(mcp_server("mcp__server__"), Some("server"));
        assert_eq!(mcp_server("mcp__"), None);
        assert_eq!(mcp_server("Bash"), None);
    }

    #[test]
    fn unknown_tool_falls_back_to_name_only() {
        let cfg = Config::default();
        let act = map_activity("SomeFutureTool", Some(&json!({ "arg": "x" })), &cfg);
        assert_eq!(act.verb, "SomeFutureTool");
        assert_eq!(act.target, None);
        assert_eq!(act.small_image_key.as_deref(), Some("tool"));
    }

    #[test]
    fn spec_verbs_are_fixed_regardless_of_config_tool_verbs() {
        // The default tool_verbs map ships Task → "delegating", but FR-2/AC-2
        // pins Task/Agent to "Orchestrating agents"; the spec verb must win.
        let cfg = Config::default();
        assert_eq!(
            cfg.tool_verbs.get("Task").map(String::as_str),
            Some("delegating")
        );
        let act = map_activity("Task", Some(&json!({})), &cfg);
        assert_eq!(act.verb, "Orchestrating agents");
    }

    #[test]
    fn unknown_tool_uses_capitalized_config_verb() {
        // An override for a tool this mapping doesn't recognize is honored and
        // capitalized for display; still verb-only (no argument text).
        let mut cfg = Config::default();
        cfg.tool_verbs
            .insert("Notebook".to_string(), "tinkering with".to_string());
        let act = map_activity("Notebook", Some(&json!({ "path": "/x/y.ipynb" })), &cfg);
        assert_eq!(act.verb, "Tinkering with");
        assert_eq!(act.target, None);
        assert_eq!(act.small_image_key.as_deref(), Some("tool"));
    }

    #[test]
    fn missing_input_is_tolerated() {
        let cfg = Config::default();
        let act = map_activity("Bash", None, &cfg);
        assert_eq!(act.verb, "Running");
        assert_eq!(act.target, None);
    }
}
