//! Versioned adapters for the internal `~/.claude` layout (ADR-5, C-4).
//!
//! Every read of a transcript line, a `sessions/<PID>.json` registry file, the
//! Anthropic-stable **statusLine JSON**, or the **hook JSON** goes through the
//! serde structs and tolerant parsers in this module. Nothing else in the crate
//! deserializes those shapes directly.
//!
//! Design rules baked in here:
//!
//! * **Forward-compatible by construction.** Every field is `Option<…>` or has a
//!   `#[serde(default)]`, and structs carry `#[serde(deny_unknown_fields)]`
//!   *nowhere* — unknown/added keys are silently ignored. A partial or
//!   forward-incompatible line therefore still deserializes into a reduced view
//!   rather than failing the whole read.
//! * **Never panic.** The line/value parsers return `Result`/`Option`; a
//!   malformed trailing JSON line is skipped (`None`) instead of unwinding, so a
//!   truncated final transcript line (FR-2/AC-1) cannot crash a collector.
//! * **Version-gated, degrade + log.** The internal transcript/sessions layouts
//!   are version-pinned best-effort: [`supported_version`] classifies the
//!   embedded `version` and unknown majors are logged at `warn` and treated as a
//!   degraded read, never an error. The statusLine/hook contracts are
//!   Anthropic-stable and are parsed leniently regardless of version.
//!
//! Field shapes were live-verified against Claude Code **v2.1.181** (see
//! `specs/research-dossier.json`) and cross-checked against the public
//! statusLine/hooks docs.
//!
//! The public surface here (structs, parsers, the version gate) is consumed by
//! the transcript watcher, sessions discovery, and the ingest socket.

use serde::Deserialize;

/// The CLI major version this adapter was authored and verified against.
///
/// Used only to decide whether to emit a one-time degrade warning for an
/// unfamiliar internal layout — it never gates whether a line is parsed.
const VERIFIED_MAJOR: u32 = 2;

/// Outcome of classifying an embedded `version` string against the adapter's
/// pinned contract (C-4 / NFR-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionSupport {
    /// Same major as the verified build — full-confidence read.
    Supported,
    /// A different (or unparseable) major — read still attempted, but the caller
    /// should treat extracted fields as best-effort and surface a reduced card.
    Degraded,
}

/// Classify a `version` string (e.g. `"2.1.181"`) against the pinned contract.
///
/// A missing or unparseable version, or a different major than [`VERIFIED_MAJOR`],
/// returns [`VersionSupport::Degraded`]. This never errors and never panics —
/// it is a hint, not a gate (C-4: degrade gracefully).
pub fn supported_version(version: Option<&str>) -> VersionSupport {
    match version.and_then(parse_major) {
        Some(major) if major == VERIFIED_MAJOR => VersionSupport::Supported,
        _ => VersionSupport::Degraded,
    }
}

/// Extract the leading integer major component of a dotted version string.
fn parse_major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Transcript: assistant line (`~/.claude/projects/<slug>/<sessionId>.jsonl`)
// ---------------------------------------------------------------------------

/// One newline-delimited transcript line.
///
/// Only the fields this daemon consumes are modelled; all are optional so that a
/// `user` / `attachment` / `ai-title` / `system` line (or a future `type`)
/// deserializes without error. Activity/model/usage extraction inspects
/// [`Self::message`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranscriptLine {
    /// Line discriminator: `assistant`, `user`, `attachment`, `ai-title`, …
    #[serde(default)]
    pub r#type: Option<String>,
    /// Working directory recorded on the line (FR-2/AC-3).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Git branch; may be `HEAD`/detached or absent (FR-2/AC-3 handles this).
    #[serde(default, rename = "gitBranch")]
    pub git_branch: Option<String>,
    /// The session's own id; equals the transcript filename stem.
    #[serde(default, rename = "sessionId")]
    pub session_id: Option<String>,
    /// CLI version that wrote the line — drives [`supported_version`].
    #[serde(default)]
    pub version: Option<String>,
    /// ISO-8601 timestamp string as emitted by the CLI.
    #[serde(default)]
    pub timestamp: Option<String>,
    /// `true` only on subagent transcripts (FR-2/AC-6); parent lines are `false`.
    #[serde(default, rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    /// Model-generated session title carried on an `ai-title` line
    /// (`{type:"ai-title", aiTitle, sessionId}`, FR-2/AC-3). Extraction ≠
    /// emission — it is gated by [`crate::privacy::ai_title`] before any card use.
    #[serde(default, rename = "aiTitle")]
    pub ai_title: Option<String>,
    /// The assistant message payload (present on `assistant` lines).
    #[serde(default)]
    pub message: Option<Message>,
}

/// The `message` object of an assistant line.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    /// `msg_…` id shared across streaming partials of the same turn (FR-2/AC-2).
    #[serde(default)]
    pub id: Option<String>,
    /// `assistant` for the lines we care about.
    #[serde(default)]
    pub role: Option<String>,
    /// Model id, e.g. `claude-opus-4-8` (FR-2/AC-3).
    #[serde(default)]
    pub model: Option<String>,
    /// `tool_use` | `end_turn` | `null` (`null` ⇒ streaming partial, FR-2/AC-2).
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Ordered content blocks; we look for `tool_use` blocks (FR-2/AC-2).
    ///
    /// Claude Code writes a plain **user prompt**'s `content` as a bare JSON
    /// string rather than an array. We accept either shape so the line still
    /// parses (a string yields no blocks — it has no `tool_use`/`tool_result`,
    /// which is all this adapter reads from `content`). Without this, prompt lines
    /// were silently dropped, hiding the start of a turn.
    #[serde(default, deserialize_with = "string_or_content_blocks")]
    pub content: Vec<ContentBlock>,
    /// Token accounting for this request (FR-2/AC-4).
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// A single content block inside `message.content`.
///
/// Untagged so that `text`, `tool_use`, `tool_result`, `thinking`, and any
/// future block type all deserialize; only the fields used for activity
/// extraction are captured. The discriminating `type` is kept verbatim.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContentBlock {
    /// Block discriminator: `text`, `tool_use`, `tool_result`, `thinking`, …
    #[serde(default)]
    pub r#type: Option<String>,
    /// `tool_use` block id / `tool_result`'s `tool_use_id`.
    #[serde(default)]
    pub id: Option<String>,
    /// Tool name on a `tool_use` block, e.g. `Bash`, `Edit`, `mcp__foo__bar`.
    #[serde(default)]
    pub name: Option<String>,
    /// Raw tool arguments on a `tool_use` block.
    ///
    /// Privacy (C-7): this is captured for the adapter only; raw values MUST be
    /// routed through `privacy.rs` before influencing any Discord field or log.
    #[serde(default)]
    pub input: Option<serde_json::Value>,
}

impl ContentBlock {
    /// `true` when this block is an assistant `tool_use` invocation.
    pub fn is_tool_use(&self) -> bool {
        self.r#type.as_deref() == Some("tool_use")
    }
}

/// Deserialize `message.content` from either an array of blocks or a bare string
/// (a plain user prompt). Anything that is not an array yields no blocks; bad
/// individual blocks are skipped rather than failing the whole line.
fn string_or_content_blocks<'de, D>(deserializer: D) -> Result<Vec<ContentBlock>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Array(blocks) => Ok(blocks
            .into_iter()
            .filter_map(|block| serde_json::from_value::<ContentBlock>(block).ok())
            .collect()),
        _ => Ok(Vec::new()),
    }
}

/// `message.usage` — token accounting (FR-2/AC-4, FR-3/AC-3).
///
/// Live context tokens are `input + cache_read + cache_creation` of the latest
/// request (per the verified used_percentage formula); helpers below compute
/// those without the caller re-summing the optional fields.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    /// Fresh input tokens for this request.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Output tokens produced so far (grows across streaming partials).
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Total tokens written to the prompt cache this request (flat field). The
    /// nested [`Self::cache_creation`] object, when present, carries the same sum
    /// split into 5m/1h buckets and is preferred for both ctx% and cost.
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// Tokens served from the prompt cache this request.
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// Nested 5m/1h cache-creation breakdown (`cache_creation.*`), present on
    /// recent CLI versions. When set, its bucket sum supersedes the flat
    /// [`Self::cache_creation_input_tokens`] and feeds the per-bucket cost rates.
    #[serde(default)]
    pub cache_creation: Option<CacheCreation>,
    /// e.g. `standard` — informational only.
    #[serde(default)]
    pub service_tier: Option<String>,
}

/// The nested `message.usage.cache_creation` object: cache-creation tokens split
/// by TTL bucket (verified on CC v2.1.181, see `assistant_line.json`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CacheCreation {
    /// 5-minute ephemeral cache-creation tokens.
    #[serde(default)]
    pub ephemeral_5m_input_tokens: Option<u64>,
    /// 1-hour ephemeral cache-creation tokens.
    #[serde(default)]
    pub ephemeral_1h_input_tokens: Option<u64>,
}

impl Usage {
    /// Cache-creation tokens for this request: the nested 5m+1h bucket sum when
    /// the `cache_creation` object is present, else the flat field (FR-2/AC-4).
    pub fn cache_creation_tokens(&self) -> u64 {
        match self.cache_creation.as_ref() {
            Some(cc) => {
                cc.ephemeral_5m_input_tokens.unwrap_or(0)
                    + cc.ephemeral_1h_input_tokens.unwrap_or(0)
            }
            None => self.cache_creation_input_tokens.unwrap_or(0),
        }
    }

    /// Live context tokens = `input + cache_read + cache_creation` of the latest
    /// request (FR-2/AC-4). Missing components count as zero.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens.unwrap_or(0)
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_tokens()
    }

    /// Total tokens attributable to this request (context + output), used for the
    /// `… tok` figure in `state` (FR-2/AC-4).
    pub fn total_tokens(&self) -> u64 {
        self.context_tokens() + self.output_tokens.unwrap_or(0)
    }
}

impl TranscriptLine {
    /// `true` when this is an assistant line carrying a message payload.
    pub fn is_assistant(&self) -> bool {
        self.r#type.as_deref() == Some("assistant")
    }

    /// The first assistant `tool_use` block, if any — the primary activity
    /// signal (FR-2/AC-2).
    pub fn first_tool_use(&self) -> Option<&ContentBlock> {
        self.message
            .as_ref()?
            .content
            .iter()
            .find(|b| b.is_tool_use())
    }
}

/// Parse one transcript JSONL line, tolerating a truncated/garbage final line.
///
/// Returns `Ok(None)` for an empty/whitespace line and `Err` for a malformed
/// one so the caller can `skip-and-continue` (FR-2/AC-1) rather than panic. The
/// `Err` path is the *expected* outcome for a truncated trailing line and must
/// be handled, not unwrapped.
pub fn parse_transcript_line(line: &str) -> Result<Option<TranscriptLine>, serde_json::Error> {
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line).map(Some)
}

// ---------------------------------------------------------------------------
// Sessions registry (`~/.claude/sessions/<PID>.json`)
// ---------------------------------------------------------------------------

/// `~/.claude/sessions/<PID>.json` — the authoritative live-session index
/// (FR-1/AC-2). `sessionId` here is the session's *own* id, distinct from any
/// `--resume`/`--fork-session` argv id (which is verified-stale).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRegistry {
    /// Engine PID this file is named for.
    #[serde(default)]
    pub pid: Option<i32>,
    /// The session's own id → maps to `<sessionId>.jsonl`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Working directory at session start.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Session start as **epoch milliseconds** (feeds the elapsed timer; the
    /// Discord field is also epoch-ms — FR-5/AC-4).
    #[serde(default)]
    pub started_at: Option<i64>,
    /// Human-readable process start (`procStart`), informational.
    #[serde(default)]
    pub proc_start: Option<String>,
    /// CLI version — drives [`supported_version`].
    #[serde(default)]
    pub version: Option<String>,
    /// Peer protocol number, informational.
    #[serde(default)]
    pub peer_protocol: Option<i64>,
    /// e.g. `interactive`.
    #[serde(default)]
    pub kind: Option<String>,
    /// e.g. `claude-desktop`, `cli`.
    #[serde(default)]
    pub entrypoint: Option<String>,
}

/// Parse a `sessions/<PID>.json` document. Degrades to a partial struct on
/// missing fields; only structurally-invalid JSON errors (C-4).
pub fn parse_session_registry(bytes: &[u8]) -> Result<SessionRegistry, serde_json::Error> {
    serde_json::from_slice(bytes)
}

// ---------------------------------------------------------------------------
// statusLine JSON (Anthropic-stable contract, stdin in the wrapper) — FR-3/AC-2
// ---------------------------------------------------------------------------

/// The statusLine JSON Claude Code pipes to the configured command on stdin.
///
/// Only the subset this daemon forwards is modelled (FR-3/AC-2); the live binary
/// emits more (`workspace`, `rate_limits`, `output_style`, …) which is ignored.
/// Every field is optional because early-session calls null out `cost` /
/// `context_window.*` and `effort` is absent for models without the parameter.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusLine {
    /// Working directory (top-level; mirrors `workspace.current_dir`).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Session id.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Path to this session's transcript.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Current model `{ id, display_name }`.
    #[serde(default)]
    pub model: Option<StatusModel>,
    /// CLI version (top-level on the statusLine contract).
    #[serde(default)]
    pub version: Option<String>,
    /// Session cost / duration block.
    #[serde(default)]
    pub cost: Option<StatusCost>,
    /// Live context-window block.
    #[serde(default)]
    pub context_window: Option<StatusContextWindow>,
    /// Reasoning effort `{ level }`; absent for models without effort.
    #[serde(default)]
    pub effort: Option<StatusEffort>,
}

/// statusLine `model` object (FR-3/AC-2).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusModel {
    /// Model identifier, e.g. `claude-opus-4-8`.
    #[serde(default)]
    pub id: Option<String>,
    /// Human label, e.g. `Opus 4.8` — the `model.display_name` for `state`.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// statusLine `cost` object (FR-3/AC-2).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusCost {
    /// Client-side estimated session cost in USD.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    /// Wall-clock time since session start, in milliseconds.
    #[serde(default)]
    pub total_duration_ms: Option<i64>,
}

/// statusLine `context_window` object (FR-3/AC-2, FR-3/AC-4).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusContextWindow {
    /// Pre-calculated percentage of context used (may be `null` early-session).
    #[serde(default)]
    pub used_percentage: Option<f64>,
    /// Max context window in tokens — preferred denominator for ctx% (200k or
    /// 1M). Prefer this over the local table when present (FR-3/AC-4).
    #[serde(default)]
    pub context_window_size: Option<u64>,
}

/// statusLine `effort` object (FR-3/AC-2).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StatusEffort {
    /// `low` | `medium` | `high` | `xhigh` | `max`.
    #[serde(default)]
    pub level: Option<String>,
}

/// Parse a statusLine JSON document (Anthropic-stable; parsed leniently).
pub fn parse_statusline(bytes: &[u8]) -> Result<StatusLine, serde_json::Error> {
    serde_json::from_slice(bytes)
}

// ---------------------------------------------------------------------------
// Hook JSON (Anthropic-stable contract, stdin to the forwarder) — FR-4
// ---------------------------------------------------------------------------

/// The JSON Claude Code passes to a hook command on stdin.
///
/// Common fields land on every event; event-specific fields (`tool_name`,
/// `tool_input`, `source`, `agent_type`, …) are optional and present only for
/// the relevant `hook_event_name` (FR-4/AC-1). Privacy (C-7): `tool_input` is
/// captured for the adapter only and MUST pass through `privacy.rs` before it
/// reaches any Discord field or log.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookEvent {
    /// Current session id (common field).
    #[serde(default)]
    pub session_id: Option<String>,
    /// Transcript path (common field).
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// Working directory when the hook fired (common field).
    #[serde(default)]
    pub cwd: Option<String>,
    /// The event that fired: `PreToolUse`, `PostToolUse`, `Stop`,
    /// `SessionStart`, `SubagentStart`, `SubagentStop`, `CwdChanged`, …
    #[serde(default)]
    pub hook_event_name: Option<String>,
    /// Current permission mode (common field).
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Subagent id (present under `--agent`/inside a subagent).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Subagent name (`SubagentStart`/`SubagentStop`, or `--agent`).
    #[serde(default)]
    pub agent_type: Option<String>,
    /// Tool name (`PreToolUse`/`PostToolUse`).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Raw tool arguments (`PreToolUse`/`PostToolUse`); see privacy note above.
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    /// Tool output (`PostToolUse`).
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    /// Session start source (`SessionStart`): `startup`|`resume`|`clear`|`compact`.
    #[serde(default)]
    pub source: Option<String>,
    /// Active model id (`SessionStart`, optional).
    #[serde(default)]
    pub model: Option<String>,
    /// Subagent outcome (`SubagentStop`).
    #[serde(default)]
    pub result: Option<String>,
    /// New directory (`CwdChanged`).
    #[serde(default)]
    pub new_cwd: Option<String>,
    /// Previous directory (`CwdChanged`).
    #[serde(default)]
    pub previous_cwd: Option<String>,
}

/// Parse a hook JSON document (Anthropic-stable; parsed leniently).
pub fn parse_hook_event(bytes: &[u8]) -> Result<HookEvent, serde_json::Error> {
    serde_json::from_slice(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Read a fixture from `tests/fixtures/` relative to the crate root.
    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
    }

    #[test]
    fn version_gate_classifies_major() {
        assert_eq!(
            supported_version(Some("2.1.181")),
            VersionSupport::Supported
        );
        assert_eq!(supported_version(Some("2.0.0")), VersionSupport::Supported);
        assert_eq!(supported_version(Some("3.0.0")), VersionSupport::Degraded);
        assert_eq!(supported_version(Some("garbage")), VersionSupport::Degraded);
        assert_eq!(supported_version(None), VersionSupport::Degraded);
    }

    #[test]
    fn assistant_line_parses_all_fields() {
        let line = parse_transcript_line(&fixture("assistant_line.json"))
            .expect("valid json")
            .expect("non-empty");

        assert!(line.is_assistant());
        assert_eq!(
            line.cwd.as_deref(),
            Some("/Users/redacted/Projects/private")
        );
        assert_eq!(line.git_branch.as_deref(), Some("master"));
        assert_eq!(
            line.session_id.as_deref(),
            Some("d4f690f4-2701-4720-9274-b845c84cb78b")
        );
        assert_eq!(line.version.as_deref(), Some("2.1.181"));
        assert_eq!(line.is_sidechain, Some(false));
        assert!(line.timestamp.is_some());
        assert_eq!(
            supported_version(line.version.as_deref()),
            VersionSupport::Supported
        );

        let msg = line.message.as_ref().expect("message present");
        assert_eq!(msg.id.as_deref(), Some("msg_01EvLJredacted"));
        assert_eq!(msg.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));

        let usage = msg.usage.as_ref().expect("usage present");
        assert_eq!(usage.input_tokens, Some(10552));
        assert_eq!(usage.output_tokens, Some(1402));
        assert_eq!(usage.cache_creation_input_tokens, Some(7021));
        assert_eq!(usage.cache_read_input_tokens, Some(19423));
        // The nested cache_creation object is parsed and supersedes the flat
        // cache_creation_input_tokens for context/cost (verified fixture: 1h 10493).
        let cc = usage
            .cache_creation
            .as_ref()
            .expect("cache_creation present");
        assert_eq!(cc.ephemeral_1h_input_tokens, Some(10493));
        assert_eq!(cc.ephemeral_5m_input_tokens, Some(0));
        assert_eq!(usage.cache_creation_tokens(), 10493);
        // input + cache_read + cache_creation (object sum) (FR-2/AC-4).
        assert_eq!(usage.context_tokens(), 10552 + 19423 + 10493);
        assert_eq!(usage.total_tokens(), 10552 + 19423 + 10493 + 1402);

        let tool = line.first_tool_use().expect("tool_use block");
        assert_eq!(tool.name.as_deref(), Some("Bash"));
        assert!(tool.input.is_some());
    }

    #[test]
    fn partial_assistant_line_has_null_stop_reason() {
        // FR-2/AC-2: streaming partials share message.id with stop_reason:null.
        let line = parse_transcript_line(&fixture("assistant_line_partial.json"))
            .expect("valid json")
            .expect("non-empty");
        let msg = line.message.as_ref().expect("message present");
        assert_eq!(msg.id.as_deref(), Some("msg_01EvLJredacted"));
        assert_eq!(msg.stop_reason, None);
        // Detached/HEAD branch handled by extraction, parsed verbatim here.
        assert_eq!(line.git_branch.as_deref(), Some("HEAD"));
    }

    #[test]
    fn truncated_line_errs_instead_of_panicking() {
        // A truncated final JSON line (FR-2/AC-1) must Err, never panic.
        assert!(parse_transcript_line("{\"type\":\"assistant\",\"mess").is_err());
    }

    #[test]
    fn blank_line_is_skipped() {
        assert!(parse_transcript_line("   ").unwrap().is_none());
        assert!(parse_transcript_line("").unwrap().is_none());
    }

    #[test]
    fn unknown_keys_and_types_do_not_fail() {
        // Forward-incompatible line: unknown type + extra keys must still parse.
        let json = r#"{"type":"future-thing","brand_new_field":42,"message":{"surprise":true}}"#;
        let line = parse_transcript_line(json)
            .expect("valid")
            .expect("present");
        assert_eq!(line.r#type.as_deref(), Some("future-thing"));
        assert!(!line.is_assistant());
        assert!(line.first_tool_use().is_none());
    }

    #[test]
    fn session_registry_parses() {
        let reg =
            parse_session_registry(fixture("session.json").as_bytes()).expect("valid registry");
        assert_eq!(reg.pid, Some(98863));
        assert_eq!(
            reg.session_id.as_deref(),
            Some("d4f690f4-2701-4720-9274-b845c84cb78b")
        );
        assert_eq!(reg.cwd.as_deref(), Some("/Users/redacted/Projects/private"));
        assert_eq!(reg.started_at, Some(1781987269616));
        assert_eq!(reg.version.as_deref(), Some("2.1.181"));
        assert_eq!(reg.kind.as_deref(), Some("interactive"));
        assert_eq!(reg.entrypoint.as_deref(), Some("claude-desktop"));
        assert_eq!(reg.peer_protocol, Some(1));
    }

    #[test]
    fn statusline_parses_all_fields() {
        let sl = parse_statusline(fixture("statusline.json").as_bytes()).expect("valid");
        assert_eq!(sl.version.as_deref(), Some("2.1.181"));
        assert_eq!(
            sl.session_id.as_deref(),
            Some("d4f690f4-2701-4720-9274-b845c84cb78b")
        );

        let model = sl.model.as_ref().expect("model");
        assert_eq!(model.id.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(model.display_name.as_deref(), Some("Opus 4.8"));

        let cost = sl.cost.as_ref().expect("cost");
        assert_eq!(cost.total_cost_usd, Some(0.98));
        assert_eq!(cost.total_duration_ms, Some(45000));

        let ctx = sl.context_window.as_ref().expect("context_window");
        assert_eq!(ctx.used_percentage, Some(8.0));
        assert_eq!(ctx.context_window_size, Some(1_000_000));

        let effort = sl.effort.as_ref().expect("effort");
        assert_eq!(effort.level.as_deref(), Some("high"));
    }

    #[test]
    fn statusline_minimal_tolerates_nulls_and_absent_fields() {
        // Early-session: used_percentage null, effort/duration absent.
        let sl = parse_statusline(fixture("statusline_minimal.json").as_bytes()).expect("valid");
        assert!(sl.effort.is_none());
        let ctx = sl.context_window.as_ref().expect("context_window");
        assert_eq!(ctx.used_percentage, None);
        assert_eq!(ctx.context_window_size, Some(1_000_000));
        let cost = sl.cost.as_ref().expect("cost");
        assert_eq!(cost.total_cost_usd, Some(0.0));
        assert_eq!(cost.total_duration_ms, None);
    }

    #[test]
    fn hook_pretooluse_parses() {
        let ev = parse_hook_event(fixture("hook_pretooluse.json").as_bytes()).expect("valid");
        assert_eq!(ev.hook_event_name.as_deref(), Some("PreToolUse"));
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            ev.session_id.as_deref(),
            Some("d4f690f4-2701-4720-9274-b845c84cb78b")
        );
        assert_eq!(ev.cwd.as_deref(), Some("/Users/redacted/Projects/private"));
        assert_eq!(ev.permission_mode.as_deref(), Some("default"));
        assert!(ev.tool_input.is_some());
    }

    #[test]
    fn hook_sessionstart_parses() {
        let ev = parse_hook_event(fixture("hook_sessionstart.json").as_bytes()).expect("valid");
        assert_eq!(ev.hook_event_name.as_deref(), Some("SessionStart"));
        assert_eq!(ev.source.as_deref(), Some("startup"));
        assert_eq!(ev.model.as_deref(), Some("claude-opus-4-8"));
        assert!(ev.tool_name.is_none());
    }

    #[test]
    fn hook_unknown_event_does_not_fail() {
        let json = r#"{"hook_event_name":"FutureEvent","brand_new":true}"#;
        let ev = parse_hook_event(json.as_bytes()).expect("valid");
        assert_eq!(ev.hook_event_name.as_deref(), Some("FutureEvent"));
        assert!(ev.tool_name.is_none());
    }

    #[test]
    fn malformed_json_errs_for_each_adapter() {
        assert!(parse_session_registry(b"{not json").is_err());
        assert!(parse_statusline(b"{not json").is_err());
        assert!(parse_hook_event(b"{not json").is_err());
    }
}
