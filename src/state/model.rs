//! Shared domain types (design §4.2): the data the collectors produce and the
//! aggregator merges into a single [`PresenceModel`]. Pure data, no behavior.

use std::time::SystemTime;

/// A sanitized, Discord-safe description of what a session is currently doing,
/// derived from the latest assistant `tool_use` block (`activity.rs`).
// Consumed by the collectors + aggregator (later tasks); unreferenced in the skeleton.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct Activity {
    /// Verb shown in `details`, e.g. `Running`, `Editing`, `Searching`.
    pub verb: String,
    /// Optional sanitized target (program name, basename, mcp server, …).
    pub target: Option<String>,
    /// Optional per-tool badge asset key for `assets.small_image`.
    pub small_image_key: Option<String>,
}

/// Per-session state assembled from all collectors for one live Claude Code engine.
// Consumed by the collectors + aggregator (later tasks); unreferenced in the skeleton.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: String,
    pub pid: i32,
    /// Raw project basename (final cwd component), used internally. The card
    /// label is derived from [`Self::cwd`] via `privacy::project_label` so the
    /// blacklist/redaction is always honoured (C-7) — never emit this verbatim.
    pub project: String,
    /// Working directory of the session. Carried so the aggregator can apply the
    /// blacklist + redaction when building the project label and gating the
    /// branch / activity target / ai-title (FR-7/AC-2, C-7).
    pub cwd: std::path::PathBuf,
    pub branch: Option<String>,
    pub model: Option<String>,
    pub started_at: SystemTime,
    pub last_active: SystemTime,
    pub busy: bool,
    /// Whether a turn is in progress (user prompt awaiting the assistant's
    /// completion) — true through thinking and between tool calls, so a long
    /// reasoning pause still counts as active. See [`crate::claude::transcript`].
    pub working: bool,
    pub activity: Option<Activity>,
    /// Model-generated session title (FR-2/AC-3). Carried raw; emission is gated
    /// by `privacy::ai_title` + `show_ai_title` at the aggregator (off by default).
    pub title: Option<String>,
    pub cost_usd: Option<f64>,
    pub ctx_pct: Option<f64>,
    pub tokens_total: Option<u64>,
    pub subagents: u32,
    /// Total tokens attributed to this session's live subagents (the sum of each
    /// counted agent transcript's latest-request total). `None` when no subagent
    /// is live; folded into the displayed token figure alongside
    /// [`Self::tokens_total`] so the card's token count includes running agents.
    pub subagent_tokens: Option<u64>,
}

/// The single aggregated card pushed to Discord (`SET_ACTIVITY`). One presence
/// per app per user, so all sessions collapse into this (C-2, FR-5).
// Consumed by the aggregator + discord sink (later tasks); unreferenced in the skeleton.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct PresenceModel {
    /// All live sessions; `focused` indexes into this.
    pub sessions: Vec<SessionState>,
    /// Index into `sessions` of the headline/focused session (FR-5/AC-1).
    pub focused: usize,
    /// Number of live top-level sessions → `party.size[0]` (FR-5/AC-2).
    pub live_count: u32,
    /// Configured max → `party.size[1]`; defaults to `live_count` when unset.
    pub capacity: u32,
    /// Discord `details` (≤128), built and sanitized by the aggregator.
    pub details: String,
    /// Discord `state` (≤128), built and sanitized by the aggregator.
    pub state: String,
    /// `timestamps.start` as **epoch milliseconds** (FR-5/AC-4).
    pub started_at_ms: i64,
    /// `assets.large_image` key.
    pub large_image: String,
    /// `assets.large_text` tooltip (e.g. "Claude Code").
    pub large_text: String,
    /// `assets.small_image` key (per-tool badge).
    pub small_image: Option<String>,
    /// `assets.small_text` tooltip.
    pub small_text: Option<String>,
    /// Optional `buttons` (label, url); off by default (FR-7/AC-2).
    pub buttons: Vec<(String, String)>,
}
