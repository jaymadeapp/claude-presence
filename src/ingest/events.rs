//! The wire + overlay types for the ingest push path (design §4.1).
//!
//! Two shapes travel over the daemon socket as newline-delimited JSON:
//! a **hook** event (lowest-latency "Running X" the instant a tool starts) and a
//! **statusLine** event (Anthropic-stable exact cost / ctx% / model / version /
//! effort / duration). Both carry a `kind` discriminant matching the chained
//! shell scripts' `--kind hook|statusline`.
//!
//! Raw payloads are *parsed* here only to be immediately reduced to a
//! [`Overlay`] — a small, already-sanitized delta the run loop layers onto the
//! matching session's [`SessionState`]. Nothing past the parse boundary retains
//! raw `tool_input`, prompt text, or full paths (C-7 / FR-8/AC-4): the only
//! free-text that survives is an [`crate::state::model::Activity`] already run
//! through `privacy.rs`/`activity.rs`, and the basename-or-generic `cwd` is kept
//! solely so the run loop can apply the blacklist when matching a session.

use serde::Deserialize;

use crate::claude::schema;
use crate::config::Config;
use crate::state::model::Activity;

/// A newline-delimited event received on the daemon socket.
///
/// Tagged by `kind` so a single connection can interleave hook and statusLine
/// frames. Both variants mirror the design §4.1 contract; the unused-by-us
/// fields the live binary also sends are ignored (the embedded schema structs
/// are lenient — ADR-5).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum IngestEvent {
    /// A lifecycle hook (`PreToolUse`/`PostToolUse`/`Stop`/…). Drives immediate
    /// activity (FR-4/AC-2).
    Hook(HookFrame),
    /// A statusLine push: exact cost / ctx% / model for its session (FR-3).
    Statusline(StatuslineFrame),
}

/// The hook frame as it arrives on the wire (design §4.1).
///
/// Deserialized leniently — every field is optional so a `Stop`/`SessionStart`
/// frame (no `tool_*`) parses just as a `PreToolUse` does. `tool_input` is
/// captured only to feed the sanitizing mapper in [`IngestEvent::overlay`]; it
/// never leaves this module unredacted.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookFrame {
    /// The event that fired, e.g. `PreToolUse`, `PostToolUse`, `Stop`.
    #[serde(default, alias = "hook_event_name")]
    pub event: Option<String>,
    /// Session id the event belongs to (matches `SessionState::session_id`).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Working directory when the hook fired (used for blacklist matching only).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Tool name on `PreToolUse`/`PostToolUse`.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Raw tool arguments — sanitized at the boundary, never retained.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Event timestamp (epoch seconds), informational.
    #[serde(default)]
    pub ts: Option<i64>,
}

/// The statusLine frame as it arrives on the wire (design §4.1).
///
/// This is the compact subset the wrapper forwards. The wrapper may instead
/// forward the *full* statusLine JSON (the Anthropic-stable shape parsed by
/// [`schema::StatusLine`]); [`StatuslineFrame::from_value`] accepts either by
/// falling back to the schema adapter, so the wrapper need not reshape it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatuslineFrame {
    /// Session id this statusLine belongs to.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Working directory (basename-only / blacklist matching).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Model display name, e.g. `Opus 4.8` (overrides the transcript model).
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort level, informational (`low`/`high`/…).
    #[serde(default)]
    pub effort: Option<String>,
    /// Exact session cost in USD (overrides the computed estimate).
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Exact context-window usage percentage (overrides the computed estimate).
    #[serde(default)]
    pub ctx_pct: Option<f64>,
    /// Context-window size in tokens, informational.
    #[serde(default)]
    pub ctx_size: Option<u64>,
    /// CLI version that produced the statusLine, informational.
    #[serde(default)]
    pub version: Option<String>,
}

impl StatuslineFrame {
    /// Build a frame from an arbitrary JSON value, accepting both the compact
    /// design §4.1 shape and the full Anthropic statusLine JSON.
    ///
    /// The compact shape is tried first; any field it leaves unset is filled
    /// from the full statusLine adapter ([`schema::StatusLine`]) so a wrapper
    /// that simply tees CC's raw stdin still yields a usable overlay (FR-3/AC-2).
    fn from_value(value: serde_json::Value) -> Self {
        let mut frame: StatuslineFrame = serde_json::from_value(value.clone()).unwrap_or_default();

        // Backfill from the full statusLine contract for a wrapper that forwards
        // CC's raw JSON unchanged.
        if let Ok(full) = serde_json::from_value::<schema::StatusLine>(value) {
            if frame.session_id.is_none() {
                frame.session_id = full.session_id;
            }
            if frame.cwd.is_none() {
                frame.cwd = full.cwd;
            }
            if frame.model.is_none() {
                frame.model = full.model.and_then(|m| m.display_name.or(m.id));
            }
            if frame.effort.is_none() {
                frame.effort = full.effort.and_then(|e| e.level);
            }
            if let Some(cost) = full.cost.as_ref() {
                if frame.cost_usd.is_none() {
                    frame.cost_usd = cost.total_cost_usd;
                }
            }
            if let Some(ctx) = full.context_window.as_ref() {
                if frame.ctx_pct.is_none() {
                    frame.ctx_pct = ctx.used_percentage;
                }
                if frame.ctx_size.is_none() {
                    frame.ctx_size = ctx.context_window_size;
                }
            }
            if frame.version.is_none() {
                frame.version = full.version;
            }
        }
        frame
    }
}

impl IngestEvent {
    /// Parse one newline-delimited JSON line into an [`IngestEvent`].
    ///
    /// Accepts the tagged form (`{"kind":"hook",…}` / `{"kind":"statusline",…}`)
    /// directly. A statusLine line is additionally backfilled from the full
    /// statusLine contract so a raw-tee wrapper works without reshaping. A blank
    /// line yields `Ok(None)`; a malformed line yields `Err` so the caller can
    /// skip-and-continue without ever logging the raw bytes (FR-8/AC-4).
    pub fn parse_line(line: &str) -> Result<Option<IngestEvent>, serde_json::Error> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let value: serde_json::Value = serde_json::from_str(line)?;
        let kind = value.get("kind").and_then(|k| k.as_str());
        match kind {
            Some("statusline") => Ok(Some(IngestEvent::Statusline(StatuslineFrame::from_value(
                value,
            )))),
            Some("hook") => {
                let frame: HookFrame = serde_json::from_value(value)?;
                Ok(Some(IngestEvent::Hook(frame)))
            }
            // No (or unknown) discriminant: best-effort as the tagged enum, which
            // errors cleanly for a truly unrecognized shape (still no raw log).
            _ => Ok(Some(serde_json::from_value(value)?)),
        }
    }

    /// Reduce this event to a sanitized [`Overlay`] the run loop applies to the
    /// matching session.
    ///
    /// All sanitization happens here, at the boundary: a hook's activity is mapped
    /// through [`crate::claude::activity::map_activity`] (which drops bash args,
    /// reduces paths to basenames, honors the blacklist) and a statusLine's
    /// free-text model is passed verbatim only because it is a fixed label
    /// (`Opus 4.8`), not user content. The raw `tool_input` is consumed and
    /// dropped; it never reaches the returned overlay.
    pub fn overlay(&self, cfg: &Config) -> Option<Overlay> {
        match self {
            IngestEvent::Hook(frame) => frame.overlay(cfg),
            IngestEvent::Statusline(frame) => frame.overlay(),
        }
    }
}

impl HookFrame {
    fn overlay(&self, cfg: &Config) -> Option<Overlay> {
        let session_id = self.session_id.clone()?;
        let event = self.event.as_deref().unwrap_or_default();

        let mut overlay = Overlay::for_session(session_id);
        // Carry hook recency so focus tracks the most-recently-*active* session,
        // not the most-recently-started one (FR-5/AC-1, AC-4). `ts` is epoch
        // seconds; ignore a non-positive value.
        overlay.last_active = self
            .ts
            .filter(|&s| s > 0)
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s as u64));
        match event {
            // A tool is starting — show it immediately and mark busy (FR-4/AC-2).
            "PreToolUse" => {
                if let Some(tool_name) = self.tool_name.as_deref() {
                    overlay.activity = Some(crate::claude::activity::map_activity(
                        tool_name,
                        self.tool_input.as_ref(),
                        cfg,
                    ));
                }
                overlay.busy = Some(true);
            }
            // The main turn's tool finished / the turn ended — transition out of
            // "busy" and clear the per-tool activity (FR-4/AC-2).
            "PostToolUse" | "Stop" => {
                overlay.busy = Some(false);
                overlay.clear_activity = true;
            }
            // SubagentStart / SubagentStop are the *inner* subagent lifecycle and
            // say nothing about the main thread's busy state — a session can be
            // orchestrating several subagents while one stops. Forcing idle here
            // made an actively-orchestrating session flicker inactive and dropped
            // its agent count; instead they only bump last-active (below) so the
            // session stays focused while it works. SessionStart / unknown events
            // likewise do not change the card on their own.
            _ => {}
        }
        Some(overlay)
    }
}

impl StatuslineFrame {
    fn overlay(&self) -> Option<Overlay> {
        let session_id = self.session_id.clone()?;
        let mut overlay = Overlay::for_session(session_id);
        // statusLine numbers are the exact, Anthropic-stable figures: they
        // override whatever the transcript computed for this session (FR-3).
        overlay.cost_usd = self.cost_usd;
        overlay.ctx_pct = self.ctx_pct;
        // `model.display_name` is a fixed label, safe to surface verbatim.
        overlay.model = self.model.clone();
        Some(overlay)
    }
}

/// A sanitized, already-Discord-safe delta applied to one session's
/// [`SessionState`] before aggregation.
///
/// Only `Some`/`true` fields take effect; `None`/`false` leave the underlying
/// transcript-derived value untouched. The overlay carries *no* raw payload —
/// it is built solely from [`IngestEvent::overlay`] after sanitization, so it is
/// safe to keep in memory, log a *summary* of, and merge into the card.
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    /// Session this overlay applies to (`SessionState::session_id`).
    pub session_id: String,
    /// Exact cost from statusLine; overrides the computed estimate (FR-3).
    pub cost_usd: Option<f64>,
    /// Exact ctx% from statusLine; overrides the computed estimate (FR-3).
    pub ctx_pct: Option<f64>,
    /// Model display name from statusLine; overrides the transcript model.
    pub model: Option<String>,
    /// Sanitized current activity from a `PreToolUse` hook (FR-4/AC-2).
    pub activity: Option<Activity>,
    /// Busy transition from a hook (`PreToolUse` → true, `PostToolUse`/`Stop`
    /// → false). `None` leaves the transcript-derived busy state.
    pub busy: Option<bool>,
    /// Clear any per-tool activity (a `PostToolUse`/`Stop` transition).
    pub clear_activity: bool,
    /// Hook recency (`HookFrame.ts`, epoch seconds → [`SystemTime`]) used to
    /// advance the session's `last_active` for focus selection (FR-5/AC-1).
    pub last_active: Option<std::time::SystemTime>,
}

impl Overlay {
    fn for_session(session_id: String) -> Self {
        Self {
            session_id,
            ..Self::default()
        }
    }

    /// A one-line, secret-free summary safe to log (FR-8/AC-4): only the verb,
    /// session-id tail, and which structured fields are present — never the raw
    /// target/cost/model values nor any payload.
    pub fn log_summary(&self) -> String {
        let id_tail: String = self.session_id.chars().rev().take(6).collect();
        let id_tail: String = id_tail.chars().rev().collect();
        let mut tags = Vec::new();
        if self.activity.is_some() {
            tags.push("activity");
        }
        if self.busy.is_some() {
            tags.push("busy");
        }
        if self.clear_activity {
            tags.push("clear");
        }
        if self.cost_usd.is_some() {
            tags.push("cost");
        }
        if self.ctx_pct.is_some() {
            tags.push("ctx");
        }
        if self.model.is_some() {
            tags.push("model");
        }
        format!("session …{id_tail}: {{{}}}", tags.join(","))
    }

    /// Apply this overlay onto a [`SessionState`] in place. statusLine values
    /// override cost/ctx%/model; a hook sets/clears activity and the busy flag.
    pub fn apply_to(&self, session: &mut crate::state::model::SessionState) {
        if let Some(cost) = self.cost_usd {
            session.cost_usd = Some(cost);
        }
        if let Some(ctx) = self.ctx_pct {
            session.ctx_pct = Some(ctx);
        }
        if let Some(model) = self.model.as_ref() {
            session.model = Some(model.clone());
        }
        if let Some(busy) = self.busy {
            session.busy = busy;
        }
        if self.clear_activity {
            session.activity = None;
        }
        if let Some(activity) = self.activity.as_ref() {
            session.activity = Some(activity.clone());
        }
        // Advance last_active only forward, so an out-of-order hook can never
        // rewind focus recency below the transcript-derived value (FR-5/AC-1).
        if let Some(t) = self.last_active {
            if t > session.last_active {
                session.last_active = t;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn parses_compact_hook_frame() {
        let line = r#"{"kind":"hook","event":"PreToolUse","session_id":"d4f6","cwd":"/x","tool_name":"Bash","tool_input":{"command":"cargo check"},"ts":1781989000}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        match ev {
            IngestEvent::Hook(h) => {
                assert_eq!(h.event.as_deref(), Some("PreToolUse"));
                assert_eq!(h.session_id.as_deref(), Some("d4f6"));
                assert_eq!(h.tool_name.as_deref(), Some("Bash"));
            }
            _ => panic!("expected hook"),
        }
    }

    #[test]
    fn parses_compact_statusline_frame() {
        let line = r#"{"kind":"statusline","session_id":"d4f6","cwd":"/x","model":"Opus 4.8","effort":"high","cost_usd":0.98,"ctx_pct":8.3,"ctx_size":1000000,"version":"2.1.181"}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        match ev {
            IngestEvent::Statusline(s) => {
                assert_eq!(s.session_id.as_deref(), Some("d4f6"));
                assert_eq!(s.model.as_deref(), Some("Opus 4.8"));
                assert_eq!(s.cost_usd, Some(0.98));
                assert_eq!(s.ctx_pct, Some(8.3));
            }
            _ => panic!("expected statusline"),
        }
    }

    #[test]
    fn statusline_backfills_from_full_contract() {
        // A wrapper that tees CC's raw statusLine JSON (no compact reshape) still
        // yields a usable overlay (FR-3/AC-2).
        let line = r#"{"kind":"statusline","session_id":"d4f6","model":{"id":"claude-opus-4-8","display_name":"Opus 4.8"},"cost":{"total_cost_usd":1.23},"context_window":{"used_percentage":12.5,"context_window_size":1000000},"effort":{"level":"high"},"version":"2.1.181"}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        let overlay = ev.overlay(&cfg()).unwrap();
        assert_eq!(overlay.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(overlay.cost_usd, Some(1.23));
        assert_eq!(overlay.ctx_pct, Some(12.5));
    }

    #[test]
    fn blank_and_malformed_lines() {
        assert!(IngestEvent::parse_line("   ").unwrap().is_none());
        assert!(IngestEvent::parse_line("{not json").is_err());
    }

    #[test]
    fn pretooluse_overlay_sets_activity_and_busy() {
        let line = r#"{"kind":"hook","event":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"cargo check --all-features"}}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        let overlay = ev.overlay(&cfg()).unwrap();
        assert_eq!(overlay.session_id, "s1");
        assert_eq!(overlay.busy, Some(true));
        let activity = overlay.activity.expect("activity set");
        assert_eq!(activity.verb, "Running");
        // Default config drops bash args → only the program token survives.
        assert_eq!(activity.target.as_deref(), Some("cargo"));
    }

    #[test]
    fn pretooluse_overlay_never_leaks_secret() {
        // The core privacy guarantee (FR-8/AC-4): a fake token in tool_input must
        // not survive into the overlay or its logged summary.
        let line = r#"{"kind":"hook","event":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"curl -H 'Authorization: Bearer sk-FAKE-SECRET' https://api"}}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        let overlay = ev.overlay(&cfg()).unwrap();
        let activity = overlay.activity.clone().expect("activity");
        let rendered = format!("{} {:?}", activity.verb, activity.target);
        assert!(!rendered.contains("sk-FAKE-SECRET"), "{rendered}");
        assert!(!rendered.contains("Bearer"), "{rendered}");
        // The logged summary never carries the raw payload either.
        let summary = overlay.log_summary();
        assert!(!summary.contains("sk-FAKE-SECRET"), "{summary}");
        assert!(!summary.contains("curl"), "{summary}");
        assert!(summary.contains("activity"), "{summary}");
    }

    #[test]
    fn posttooluse_overlay_transitions_to_idle() {
        let line = r#"{"kind":"hook","event":"PostToolUse","session_id":"s1","tool_name":"Bash"}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        let overlay = ev.overlay(&cfg()).unwrap();
        assert_eq!(overlay.busy, Some(false));
        assert!(overlay.clear_activity);
        assert!(overlay.activity.is_none());
    }

    #[test]
    fn subagent_stop_does_not_force_idle() {
        // A subagent stopping must NOT flip the orchestrating session to idle or
        // wipe its activity — only the main turn's PostToolUse/Stop does that.
        let line = r#"{"kind":"hook","event":"SubagentStop","session_id":"s1","ts":1781989000}"#;
        let ev = IngestEvent::parse_line(line).unwrap().unwrap();
        let overlay = ev.overlay(&cfg()).unwrap();
        assert_eq!(overlay.busy, None, "SubagentStop must not change busy");
        assert!(
            !overlay.clear_activity,
            "SubagentStop must not clear activity"
        );
        // It still advances focus recency so the working session stays focused.
        assert!(overlay.last_active.is_some());
    }

    #[test]
    fn statusline_overlay_overrides_cost_ctx_model() {
        use crate::state::model::SessionState;
        use std::time::SystemTime;

        let line = r#"{"kind":"statusline","session_id":"s1","model":"Opus 4.8","cost_usd":0.98,"ctx_pct":8.3}"#;
        let overlay = IngestEvent::parse_line(line)
            .unwrap()
            .unwrap()
            .overlay(&cfg())
            .unwrap();

        let mut session = SessionState {
            session_id: "s1".to_string(),
            pid: 1,
            project: "p".to_string(),
            cwd: std::path::PathBuf::from("/p"),
            branch: None,
            model: Some("claude-opus-4-8".to_string()),
            started_at: SystemTime::UNIX_EPOCH,
            last_active: SystemTime::UNIX_EPOCH,
            busy: false,
            working: false,
            activity: None,
            title: None,
            cost_usd: None,
            ctx_pct: None,
            tokens_total: Some(100),
            subagents: 0,
            subagent_tokens: None,
        };
        overlay.apply_to(&mut session);
        assert_eq!(session.cost_usd, Some(0.98));
        assert_eq!(session.ctx_pct, Some(8.3));
        assert_eq!(session.model.as_deref(), Some("Opus 4.8"));
        // Untouched transcript fields stay.
        assert_eq!(session.tokens_total, Some(100));
    }

    #[test]
    fn hook_overlay_sets_activity_on_session() {
        use crate::state::model::{Activity, SessionState};
        use std::time::SystemTime;

        let overlay = Overlay {
            session_id: "s1".to_string(),
            busy: Some(true),
            activity: Some(Activity {
                verb: "Running".to_string(),
                target: Some("cargo".to_string()),
                small_image_key: Some("bash".to_string()),
            }),
            ..Overlay::default()
        };
        let mut session = SessionState {
            session_id: "s1".to_string(),
            pid: 1,
            project: "p".to_string(),
            cwd: std::path::PathBuf::from("/p"),
            branch: None,
            model: None,
            started_at: SystemTime::UNIX_EPOCH,
            last_active: SystemTime::UNIX_EPOCH,
            busy: false,
            working: false,
            activity: None,
            title: None,
            cost_usd: None,
            ctx_pct: None,
            tokens_total: None,
            subagents: 0,
            subagent_tokens: None,
        };
        overlay.apply_to(&mut session);
        assert!(session.busy);
        assert_eq!(
            session.activity.as_ref().map(|a| a.verb.as_str()),
            Some("Running")
        );
    }

    #[test]
    fn hook_ts_advances_last_active_forward_only() {
        use crate::state::model::SessionState;
        use std::time::{Duration, SystemTime};

        let line = r#"{"kind":"hook","event":"PreToolUse","session_id":"s1","tool_name":"Bash","ts":1781989000}"#;
        let overlay = IngestEvent::parse_line(line)
            .unwrap()
            .unwrap()
            .overlay(&cfg())
            .unwrap();
        assert_eq!(
            overlay.last_active,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_989_000))
        );

        // Applying a newer hook ts advances last_active.
        let mut session = SessionState {
            session_id: "s1".to_string(),
            pid: 1,
            project: "p".to_string(),
            cwd: std::path::PathBuf::from("/p"),
            branch: None,
            model: None,
            started_at: SystemTime::UNIX_EPOCH,
            last_active: SystemTime::UNIX_EPOCH,
            busy: false,
            working: false,
            activity: None,
            title: None,
            cost_usd: None,
            ctx_pct: None,
            tokens_total: None,
            subagents: 0,
            subagent_tokens: None,
        };
        overlay.apply_to(&mut session);
        assert_eq!(
            session.last_active,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_989_000)
        );

        // An older hook ts must NOT rewind last_active below the current value.
        let stale = Overlay {
            session_id: "s1".to_string(),
            last_active: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
            ..Overlay::default()
        };
        stale.apply_to(&mut session);
        assert_eq!(
            session.last_active,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_989_000),
            "an out-of-order hook must not rewind focus recency"
        );
    }
}
