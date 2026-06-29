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

/// Minimum spacing between Discord `SET_ACTIVITY` pushes, in seconds (FR-2/AC-1).
///
/// 4.0s so steady-state spacing keeps presence updates under Discord's ~5/20s
/// budget. Must stay in sync with the sink's local `FALLBACK_MIN_INTERVAL`
/// (`discord::sink`), which hard-codes the same 4.0s as its invalid-input floor
/// (the two are kept file-disjoint on purpose and must agree by comment).
const DEFAULT_MIN_INTERVAL_SECS: f64 = 4.0;
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
    /// Finer per-field privacy toggles (the `[privacy.fields]` table).
    pub fields: PrivacyFields,
}

/// Finer per-field privacy toggles (`[privacy.fields]`).
///
/// These default to `true` (INFORMATIVE posture): the code default still shows
/// the project and the running command. It is the *installer* — not this code
/// default — that steers a privacy-conscious user toward hiding these (it writes
/// `false` into the config when asked). Set a field to `false` to hide it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyFields {
    /// When `false`, collapses the project to a generic label (and suppresses the
    /// git branch, which would otherwise reveal the repo).
    pub project: bool,
    /// When `false`, hides the running command (Bash) target in the small-icon
    /// tooltip — only the bare verb ("Running") is shown.
    pub command: bool,
}

impl Default for PrivacyFields {
    fn default() -> Self {
        Self {
            project: true,
            command: true,
        }
    }
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

/// Expand a leading `~` (`~` or `~/…`) in `entry` to `home`.
///
/// Returns `Some` with the expanded path when `entry` starts with a `~`
/// component, otherwise `None` (the caller keeps the original entry). Only the
/// literal `~` / `~/` prefix is handled — `~user` is left untouched. No symlink
/// resolution, so the blacklist superset property is preserved.
fn expand_tilde(entry: &std::path::Path, home: &std::path::Path) -> Option<PathBuf> {
    let mut components = entry.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == "~" => {
            Some(home.join(components.as_path()))
        }
        _ => None,
    }
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
        let mut cfg: Self = toml::from_str(contents)?;
        cfg.normalize();
        Ok(cfg)
    }

    /// One-time, post-deserialize cleanup of an otherwise-valid config.
    ///
    /// - Clamps `keepalive_interval` up to `min_interval` so a misconfigured
    ///   keepalive cannot republish inside the rate floor (FR-2/AC-1, F19).
    /// - Expands a leading `~` in each `blacklist_paths` entry to the home dir
    ///   (best-effort; left as-is if the home dir can't be resolved). This is the
    ///   config-load ENTRY normalization the `privacy` blacklist matcher relies on.
    ///   It is `~`-expansion ONLY — symlinks are deliberately not resolved, so the
    ///   matcher's superset guarantee is preserved (FR-1/AC-5, F29/F31).
    fn normalize(&mut self) {
        self.keepalive_interval = self.keepalive_interval.max(self.min_interval);

        if let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()) {
            for entry in &mut self.privacy.blacklist_paths {
                if let Some(expanded) = expand_tilde(entry, &home) {
                    *entry = expanded;
                }
            }
        }
    }

    /// Serialize and write the config to [`Config::path`] atomically and durably.
    ///
    /// Creates the parent dir (`0700`) if absent, serializes to TOML, then writes
    /// to a sibling temp file, `fsync`s it, and `rename`s it over the target with
    /// mode `0600` — mirroring the atomic-write style in `install/statusline.rs`.
    /// Used by `claude-presence install` to persist the user's privacy choices
    /// before the daemon starts (there is no hot reload). Returns an `io::Error`
    /// (TOML serialization failures are mapped to one) so it never panics.
    pub fn save(&self) -> Result<(), std::io::Error> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        let path = Self::path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not resolve the config directory",
            )
        })?;

        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("config path has no parent directory"))?;
        if !parent.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }

        let contents = toml::to_string(self).map_err(std::io::Error::other)?;

        let tmp = path.with_extension("toml.tmp");

        // Once the temp file exists, any failure (write/fsync/rename) must not
        // leave a `.toml.tmp` turd behind (FR-3/AC-5, F11). Do the write inside a
        // closure and best-effort `remove_file(&tmp)` on the way out of an error.
        let write_tmp = || -> Result<(), std::io::Error> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(contents.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };

        write_tmp().inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let cfg = Config::default();
        assert_eq!(cfg.client_id, DEFAULT_CLIENT_ID);
        assert_eq!(cfg.min_interval, 4.0);
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
        assert_eq!(cfg.min_interval, 4.0);
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

    #[test]
    fn privacy_fields_default_to_informative() {
        // The code default is INFORMATIVE (true/true); the installer, not the code
        // default, steers a user toward hiding.
        let fields = PrivacyFields::default();
        assert!(fields.project);
        assert!(fields.command);
        // PrivacySettings::default() (derived) must wire PrivacyFields::default().
        let privacy = PrivacySettings::default();
        assert!(!privacy.redact);
        assert!(privacy.blacklist_paths.is_empty());
        assert!(!privacy.scrub_bash_args);
        assert!(privacy.fields.project);
        assert!(privacy.fields.command);
    }

    #[test]
    fn privacy_fields_partial_toml_keeps_other_field_true() {
        // `command = false` must not drag `project` along — serde default fills it.
        let toml = "[privacy.fields]\ncommand = false\n";
        let cfg = Config::from_toml(toml).unwrap();
        assert!(
            cfg.privacy.fields.project,
            "project stays at its true default"
        );
        assert!(!cfg.privacy.fields.command);
    }

    #[test]
    fn keepalive_clamps_up_to_min_interval() {
        // A keepalive shorter than the rate floor is raised to the floor so it
        // cannot republish faster than `min_interval` (FR-2/AC-1, F19).
        let toml = r#"
            min_interval = 4.0
            keepalive_interval = 1.0
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.min_interval, 4.0);
        assert_eq!(cfg.keepalive_interval, 4.0);
    }

    #[test]
    fn keepalive_above_min_interval_is_untouched() {
        // A keepalive already at/above the floor keeps its configured value.
        let toml = r#"
            min_interval = 4.0
            keepalive_interval = 15.0
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.keepalive_interval, 15.0);
    }

    #[test]
    fn blacklist_tilde_entry_is_expanded_to_home() {
        // `~/private` is expanded to the home-relative absolute path at load so
        // the privacy matcher receives an already-expanded entry (FR-1/AC-5).
        let home = directories::BaseDirs::new()
            .map(|b| b.home_dir().to_path_buf())
            .expect("home dir resolves in test env");
        let toml = r#"
            [privacy]
            blacklist_paths = ["~/private"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(cfg.privacy.blacklist_paths, vec![home.join("private")]);
    }

    #[test]
    fn blacklist_absolute_entry_is_left_alone() {
        // A non-`~` entry passes through normalization unchanged.
        let toml = r#"
            [privacy]
            blacklist_paths = ["/Users/me/private"]
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert_eq!(
            cfg.privacy.blacklist_paths,
            vec![PathBuf::from("/Users/me/private")]
        );
    }

    #[test]
    fn expand_tilde_handles_bare_tilde_and_non_tilde() {
        let home = PathBuf::from("/home/u");
        assert_eq!(
            expand_tilde(std::path::Path::new("~/a/b"), &home),
            Some(PathBuf::from("/home/u/a/b"))
        );
        assert_eq!(
            expand_tilde(std::path::Path::new("~"), &home),
            Some(home.clone())
        );
        // Absolute and `~user` are left untouched (None → caller keeps original).
        assert_eq!(expand_tilde(std::path::Path::new("/abs/path"), &home), None);
        assert_eq!(expand_tilde(std::path::Path::new("~bob/x"), &home), None);
    }

    #[test]
    fn save_failure_leaves_no_tmp_file() {
        // A rename failure (target dir is actually a file) must best-effort remove
        // the `.toml.tmp` scratch file (FR-3/AC-5, F11). We exercise the same
        // write-then-cleanup-on-error shape `Config::save` uses, against a temp dir
        // (the real `save` resolves a fixed user config path we must not clobber).
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("cp-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk test dir");
        let path = dir.join("config.toml");
        let tmp = path.with_extension("toml.tmp");
        // Make the rename target a directory so `rename(file -> dir)` fails.
        std::fs::create_dir_all(&path).expect("mk target dir");

        let write_tmp = || -> Result<(), std::io::Error> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(b"min_interval = 4.0\n")?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        let result = write_tmp().inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        });

        assert!(result.is_err(), "rename onto a directory must fail");
        assert!(
            !tmp.exists(),
            "the .toml.tmp scratch file must be cleaned up"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_toml_without_fields_table_still_loads() {
        // An old config that predates [privacy.fields] must load with the table
        // filled by serde defaults (true/true).
        let toml = r#"
            [privacy]
            redact = false
        "#;
        let cfg = Config::from_toml(toml).unwrap();
        assert!(cfg.privacy.fields.project);
        assert!(cfg.privacy.fields.command);
    }
}
