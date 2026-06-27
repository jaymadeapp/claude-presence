//! Configuration model, defaults, and TOML loading (task 0.2).
//!
//! [`Config`] is the human-editable TOML model loaded from
//! `~/.config/claude-presence/config.toml`. Every field has a built-in default,
//! so a missing or invalid config never crashes the daemon — it logs and falls
//! back to [`Config::default`] (FR-7/AC-3). Changes take effect on restart only
//! (no hot reload in v1).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Default Discord application client id.
///
/// The registered Discord app is named **"CC"** (Discord blocked "Claude"/"Claude
/// Code"); `large_text` shows "Claude Code". This is the default `client_id` baked
/// into the config so the MVP runs before any user config exists.
/// See CLAUDE.md → "Project specifics".
pub const DEFAULT_CLIENT_ID: u64 = 1518007333324587168;

/// Minimum spacing between Discord `SET_ACTIVITY` pushes, in seconds (FR-6/AC-3).
const DEFAULT_MIN_INTERVAL_SECS: f64 = 2.5;
/// Keep-alive republish cadence so the presence does not expire, in seconds.
const DEFAULT_KEEPALIVE_INTERVAL_SECS: f64 = 15.0;
/// How recently a subagent must have been active to count as "current", in seconds.
const DEFAULT_SUBAGENT_RECENCY_SECS: u64 = 30;

/// Top-level daemon configuration (the `config.toml` model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Discord application client id used for the IPC handshake.
    pub client_id: u64,
    /// Subscription label shown in the card `state` (e.g. "Max 20x").
    pub plan_label: String,
    /// Configured session capacity for `party.size = [live, capacity]`.
    ///
    /// `None` means "default to the live count" (FR-5/AC-2).
    pub capacity: Option<u32>,
    /// Minimum seconds between presence updates (debounce floor).
    pub min_interval: f64,
    /// Keep-alive republish cadence in seconds.
    pub keepalive_interval: f64,
    /// How recently a subagent counts as active, in seconds.
    pub subagent_recency_secs: u64,
    /// Show the AI-generated session title (off by default; privacy-gated).
    pub show_ai_title: bool,
    /// Per-field card toggles.
    pub fields: FieldToggles,
    /// Discord art-asset keys (uploaded in the Developer Portal).
    pub assets: Assets,
    /// Tool name → display verb (e.g. `Bash` → "running"); falls back to the tool name.
    pub tool_verbs: BTreeMap<String, String>,
    /// Card buttons (opt-in; off by default — see FR-7/AC-2).
    pub buttons: Vec<Button>,
    /// Privacy and redaction settings.
    pub privacy: PrivacySettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: DEFAULT_CLIENT_ID,
            plan_label: String::new(),
            capacity: None,
            min_interval: DEFAULT_MIN_INTERVAL_SECS,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL_SECS,
            subagent_recency_secs: DEFAULT_SUBAGENT_RECENCY_SECS,
            show_ai_title: false,
            fields: FieldToggles::default(),
            assets: Assets::default(),
            tool_verbs: default_tool_verbs(),
            buttons: Vec::new(),
            privacy: PrivacySettings::default(),
        }
    }
}

/// Which optional card fields are rendered (FR-7/AC-1 field toggles).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FieldToggles {
    /// Show the elapsed-time timer (`timestamps.start`).
    pub timestamp: bool,
    /// Show the cost in `state`. Off by default: without the statusLine wrapper
    /// installed, the transcript-only fallback can only price the latest request
    /// (not the running session total), so it is hidden unless explicitly enabled.
    pub cost: bool,
    /// Show the token total in `state`.
    pub tokens: bool,
    /// Show the context-window percentage in `state`.
    pub context_pct: bool,
    /// Show the git branch in `details`.
    pub branch: bool,
}

impl Default for FieldToggles {
    fn default() -> Self {
        Self {
            timestamp: true,
            cost: false,
            tokens: true,
            context_pct: true,
            branch: true,
        }
    }
}

/// Discord art-asset keys. Images are optional; an unset key omits the asset
/// (the MVP must show a valid card before any asset is uploaded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assets {
    /// `large_image` key (the app picture). Omitted when empty.
    pub large_image: Option<String>,
    /// `small_image` key (the Claude asterisk badge). Omitted when empty.
    ///
    /// Defaults to `"claude"` — the documented Art Asset key for the small badge
    /// (the Claude asterisk). The key MUST match an asset uploaded in the Discord
    /// Developer Portal, and Discord only renders the small badge when a
    /// `large_image` asset is also set, so this badge stays hidden until both
    /// assets exist.
    pub small_image: Option<String>,
}

impl Default for Assets {
    fn default() -> Self {
        Self {
            large_image: None,
            small_image: Some("claude".to_string()),
        }
    }
}

/// A single opt-in card button. URLs MUST be `https://` (enforced downstream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    /// Button label (≤32 chars per Discord).
    pub label: String,
    /// Target URL (must be `https://`; never `file://` or a private remote).
    pub url: String,
}

/// The `[privacy]` config section (FR-7/AC-1, AC-2).
///
/// Owned by the config model so `config.toml` round-trips cleanly. The actual
/// sanitizers live in [`crate::privacy`] and operate on borrowed primitives, so
/// the two modules stay independently compilable.
/// All fields default to "off" (`false` / empty), which is the informative
/// posture: baseline sanitization is always on (see [`Self::redact`]), but the
/// card still shows the real project + activity. `redact` and `blacklist_paths`
/// are the user's opt-in escalations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacySettings {
    /// Opt-in **global private mode** (off by default). When on, the focused card
    /// collapses to the generic `private_card` — no project, branch, activity
    /// target, ai-title, model, or metrics leave the process.
    ///
    /// This is *additional* to the baseline sanitization that is ALWAYS on
    /// regardless of this switch: paths are reduced to a basename, Bash args are
    /// dropped, secrets are scrubbed, the ai-title is off by default, and any
    /// `blacklist_paths` project is collapsed to a generic label. Out of the box
    /// (this `false`, no blacklist) the card shows the project basename + branch +
    /// activity + model + metrics (the product-goal example
    /// `Running cargo check — private (master)`).
    pub redact: bool,
    /// Absolute project paths whose activity is shown generically or hidden.
    pub blacklist_paths: Vec<PathBuf>,
    /// When set, Bash commands are shown (scrubbed of secrets) instead of dropped.
    pub scrub_bash_args: bool,
}

/// Built-in tool → verb map used when the user supplies none.
fn default_tool_verbs() -> BTreeMap<String, String> {
    [
        ("Bash", "running"),
        ("Read", "reading"),
        ("Edit", "editing"),
        ("Write", "writing"),
        ("Grep", "searching"),
        ("Glob", "searching"),
        ("WebSearch", "searching the web"),
        ("WebFetch", "fetching"),
        ("Task", "delegating"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect()
}

impl Config {
    /// Path to the on-disk config: `~/.config/claude-presence/config.toml`.
    ///
    /// Returns `None` if the platform config dir cannot be resolved.
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "claude-presence")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    /// Load the config from disk, falling back to defaults.
    ///
    /// A missing file, an unresolvable path, an unreadable file, or invalid TOML
    /// all degrade to [`Config::default`] (logged at the appropriate level) so the
    /// daemon never crashes on a bad config (FR-7/AC-3).
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            tracing::warn!("could not resolve config dir; using built-in defaults");
            return Self::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(contents) => Self::from_toml(&contents).unwrap_or_else(|err| {
                tracing::warn!(
                    path = %path.display(),
                    %err,
                    "invalid config.toml; using built-in defaults"
                );
                Self::default()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no config.toml; using built-in defaults");
                Self::default()
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    %err,
                    "could not read config.toml; using built-in defaults"
                );
                Self::default()
            }
        }
    }

    /// Parse a [`Config`] from a TOML string.
    pub fn from_toml(contents: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let cfg = Config::default();
        assert_eq!(cfg.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(cfg.min_interval, 2.5);
        assert_eq!(cfg.keepalive_interval, 15.0);
        assert!(!cfg.show_ai_title, "ai-title must be off by default");
        assert!(cfg.buttons.is_empty(), "buttons are opt-in");
        // Global private mode is the opt-in switch — OFF by default so the card is
        // informative (project basename + branch + activity). Baseline
        // sanitization (basename-only, bash args dropped, secrets scrubbed,
        // ai-title off, blacklist honoured) is always on regardless of this flag.
        assert!(!cfg.privacy.redact);
        assert!(!cfg.privacy.scrub_bash_args);
        assert!(cfg.privacy.blacklist_paths.is_empty());
        // The small badge defaults to the documented "claude" Art Asset key; the
        // large image stays unset until an asset is uploaded.
        assert_eq!(cfg.assets.small_image.as_deref(), Some("claude"));
        assert_eq!(cfg.assets.large_image, None);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn partial_toml_overrides_only_named_fields() {
        let toml = r#"
            plan_label = "Max 20x"
            capacity = 5
            show_ai_title = true

            [privacy]
            scrub_bash_args = true
            blacklist_paths = ["/Users/me/private"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.plan_label, "Max 20x");
        assert_eq!(cfg.capacity, Some(5));
        assert!(cfg.show_ai_title);
        assert!(cfg.privacy.scrub_bash_args);
        assert_eq!(cfg.privacy.blacklist_paths.len(), 1);
        // Untouched fields keep their defaults.
        assert_eq!(cfg.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(cfg.min_interval, 2.5);
        // `redact` is unset in this TOML → keeps the (off-by-default) default.
        assert!(!cfg.privacy.redact);
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config::default();
        let serialized = toml::to_string(&cfg).expect("serialize");
        let parsed = Config::from_toml(&serialized).expect("deserialize");
        assert_eq!(cfg, parsed);
    }

    #[test]
    fn buttons_and_assets_parse() {
        let toml = r#"
            [assets]
            large_image = "claude-logo"

            [[buttons]]
            label = "Repo"
            url = "https://example.com"
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.assets.large_image.as_deref(), Some("claude-logo"));
        // `small_image` is unset in this TOML → keeps its "claude" default.
        assert_eq!(cfg.assets.small_image.as_deref(), Some("claude"));
        assert_eq!(cfg.buttons.len(), 1);
        assert_eq!(cfg.buttons[0].url, "https://example.com");
    }

    #[test]
    fn unknown_field_is_rejected() {
        // deny_unknown_fields guards against silent typos in user configs.
        assert!(Config::from_toml("nope = 1").is_err());
    }
}
