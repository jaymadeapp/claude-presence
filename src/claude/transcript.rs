//! Transcript watcher: tail a live `<sessionId>.jsonl` and derive a per-session
//! [`SessionState`] from it (FR-2/AC-1, AC-2, AC-5, AC-6).
//!
//! ## What this collector does
//!
//! Claude Code transcripts are append-only JSONL written **open-append-close**
//! (no held fd → tail-watching is safe) and every assistant/user message lands
//! as its own line within sub-seconds (verified, dossier lane A1). From the tail
//! we derive everything the MVP card needs without any push integration:
//!
//! * **model** — `message.model` of the latest assistant request (FR-2/AC-3);
//! * **branch** — top-level `gitBranch`, normalised (`HEAD`/detached/empty → `None`);
//! * **tokens / live-context** — `message.usage` of the latest request: total =
//!   `context + output`, live context = `input + cache_read + cache_creation`
//!   (FR-2/AC-4), both via [`crate::claude::schema::Usage`];
//! * **activity** — the latest assistant `tool_use` block mapped through
//!   [`crate::claude::activity::map_activity`] (FR-2/AC-2);
//! * **busy/idle** — busy iff the most recent assistant `tool_use` id has no
//!   matching `tool_result` yet (FR-2/AC-5);
//! * **working** — whether a turn is in progress (a user prompt awaits the
//!   assistant's `end_turn`), so thinking and between-tool gaps still read active;
//! * **subagents** — live subagent count + their token total: workflow agents
//!   tallied exactly from each run's `journal.jsonl` (`started` minus resolved),
//!   flat `agent-*.jsonl` files via `isSidechain` + recent mtime (FR-2/AC-6).
//!
//! ## How the tail works (FR-2/AC-1)
//!
//! Each transcript is read incrementally: a per-file **byte offset** records how
//! far we have parsed; on every filesystem event we read from that offset to the
//! end, split on newlines, and parse only **complete** lines. A trailing chunk
//! without a final `\n` is a *partial append still being written* — its offset is
//! not advanced, so it is re-read (and completed) on the next event. Every line
//! goes through [`crate::claude::schema::parse_transcript_line`], whose `Err` for
//! a malformed/garbage line is swallowed (skip-and-continue), so a truncated JSON
//! line can never panic a collector.
//!
//! ## Streaming-partial dedupe (FR-2/AC-2)
//!
//! While a turn streams, Claude Code persists intermediate copies of the
//! assistant message that all share one `message.id` and carry `stop_reason:null`
//! until the final copy lands with a non-null `stop_reason`. We collapse them by
//! id, preferring the line whose `stop_reason` is non-null, so the derived
//! activity reflects the final tool_use rather than a half-streamed one.
//!
//! ## Public surface
//!
//! The aggregator (task 2.1) consumes this collector through a small, documented
//! API: [`derive_state`] (pure: lines → [`DerivedState`]) for the data, and
//! [`watch_session`] which spawns a `notify` watcher and pushes
//! [`DerivedState`] snapshots onto a [`tokio::sync::watch`] channel the
//! aggregator can poll or await.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{Event, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::claude::activity::map_activity;
use crate::claude::pricing;
use crate::claude::schema::{parse_transcript_line, TranscriptLine};
use crate::config::Config;
use crate::state::model::Activity;

/// The transcript-derived view of a session, ready for the aggregator to fold
/// into a [`crate::state::model::SessionState`]. Only the fields a transcript can
/// supply live here — pid/cwd/started_at come from `claude::sessions`, and
/// cost/ctx% may be refined later by the statusLine push (FR-3).
///
/// `PartialEq` is implemented by hand (not derived) because the read-only
/// [`Activity`] type does not derive `PartialEq`; the watcher uses equality only
/// to publish snapshots on change, so comparing the `Activity` by its public
/// fields is sufficient and avoids editing a sibling task's module.
#[derive(Debug, Clone, Default)]
pub struct DerivedState {
    /// `message.model` of the latest assistant request, e.g. `claude-opus-4-8`.
    pub model: Option<String>,
    /// Normalised git branch (`HEAD`/detached/empty collapsed to `None`).
    pub branch: Option<String>,
    /// Total tokens of the latest request (`context + output`, FR-2/AC-4).
    pub tokens_total: Option<u64>,
    /// Live context tokens of the latest request
    /// (`input + cache_read + cache_creation`, FR-2/AC-4).
    pub context_tokens: Option<u64>,
    /// Per-component usage of the latest assistant request, mapped into the
    /// pricing buckets so cost can be computed as a transcript fallback when no
    /// statusLine push is present (FR-3/AC-3).
    pub usage: Option<pricing::Usage>,
    /// Model-generated session title from the latest `ai-title` line (FR-2/AC-3).
    /// Carried raw — emission is gated by [`crate::privacy::ai_title`].
    pub title: Option<String>,
    /// Current activity from the latest assistant `tool_use` (FR-2/AC-2).
    pub activity: Option<Activity>,
    /// `true` iff the latest assistant `tool_use` has no matching `tool_result`
    /// yet (FR-2/AC-5).
    pub busy: bool,
    /// `true` iff a turn is in progress: a user prompt has arrived and the
    /// assistant has not yet completed its turn (`end_turn`). Unlike [`Self::busy`]
    /// this stays true through *thinking* and the gaps between tool calls, so a
    /// multi-minute reasoning pause still reads as active.
    pub working: bool,
    /// Timestamp of the newest parsed (deduped, non-sidechain) line that carried
    /// a usable per-line `timestamp` (FR-5/AC-1, AC-4). Drives focus recency; the
    /// watcher falls back to the file mtime when no line timestamp parses.
    pub last_event: Option<SystemTime>,
    /// Count of live subagents (FR-2/AC-6); filled by [`watch_session`] /
    /// [`scan_subagents`], left `0` by the pure [`derive_state`].
    pub subagents: u32,
    /// Total tokens across this session's live subagents (FR-2/AC-6); filled by
    /// [`watch_session`] / [`scan_subagents`] alongside [`Self::subagents`],
    /// `None` by the pure [`derive_state`].
    pub subagent_tokens: Option<u64>,
}

/// Equality over the public scalar fields plus a structural compare of
/// [`Activity`] (which does not derive `PartialEq`). Used only to decide whether
/// a new snapshot differs from the published one.
impl PartialEq for DerivedState {
    fn eq(&self, other: &Self) -> bool {
        self.model == other.model
            && self.branch == other.branch
            && self.tokens_total == other.tokens_total
            && self.context_tokens == other.context_tokens
            && self.usage == other.usage
            && self.title == other.title
            && self.last_event == other.last_event
            && self.busy == other.busy
            && self.working == other.working
            && self.subagents == other.subagents
            && self.subagent_tokens == other.subagent_tokens
            && activity_eq(self.activity.as_ref(), other.activity.as_ref())
    }
}

/// Structural equality of two optional [`Activity`] values by their public fields.
fn activity_eq(a: Option<&Activity>, b: Option<&Activity>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.verb == b.verb && a.target == b.target && a.small_image_key == b.small_image_key
        }
        _ => false,
    }
}

/// The slice of derived state that must persist **across** bounded ticks so a
/// 64 KiB tail window can never change the card (NFR-1, F1).
///
/// Two kinds of fact age out of a fixed-size window in a long session and would
/// be lost by a naive re-derive of the last 64 KiB:
///
/// * **Sticky latest-wins fields** — `model`, ai-`title`, git `branch`, and the
///   co-located metrics `usage`/`tokens_total`/`context_tokens`/`activity` — are
///   emitted once (or only on recent lines) and scroll out of the window; losing
///   them would blank the card model, token total, the ctx%/pricing denominator,
///   and the activity tooltip.
/// * **Turn / busy markers** — the open-turn flag and the pending-`tool_use` id
///   set: an in-flight turn whose `user` opener or originating `tool_use` line has
///   aged out must still report `working`/`busy`; a turn degrades to idle only
///   once its terminal `stop_reason` is observed inside a later window.
///
/// [`watch_session`] seeds this from a full-file derive at start and carries it
/// across each bounded tick via [`derive_state_with_carry`].
#[derive(Debug, Clone, Default)]
pub struct CarryState {
    model: Option<String>,
    branch: Option<String>,
    tokens_total: Option<u64>,
    context_tokens: Option<u64>,
    usage: Option<pricing::Usage>,
    title: Option<String>,
    activity: Option<Activity>,
    /// A turn is open (the model owes output). Carried so an in-flight turn whose
    /// opener scrolled out of the window still reports `working`.
    in_turn: bool,
    /// Unmatched assistant `tool_use` ids. Carried so a `tool_use` whose
    /// originating line scrolled out still reports `busy` until its `tool_result`
    /// is seen.
    pending_tool_uses: HashSet<String>,
}

/// Derive a [`DerivedState`] from the **full** set of transcript lines (already
/// split, in file order). Pure and deterministic — the unit of testability.
///
/// Algorithm:
/// 1. Parse every line tolerantly (malformed/truncated lines skipped).
/// 2. Collapse streaming partials: for each `message.id`, keep the latest line,
///    but a line with a non-null `stop_reason` always wins over a `null` one
///    (FR-2/AC-2).
/// 3. Walk the deduped assistant lines in order to find the latest one carrying
///    model/usage and the latest `tool_use` block; record pending `tool_use` ids
///    and resolve them against `tool_result` blocks for busy/idle (FR-2/AC-5).
/// 4. Branch comes from the most recent line that carried a `gitBranch`.
pub fn derive_state(lines: &[String], cfg: &Config) -> DerivedState {
    let mut carry = CarryState::default();
    derive_state_with_carry(lines, cfg, &mut carry)
}

/// Derive a [`DerivedState`] from a (possibly **bounded**) slice of lines while
/// carrying sticky fields and turn/busy markers across ticks (F1, NFR-1).
///
/// On entry `carry` holds the state accumulated by previous ticks; the slice is
/// folded on top of it: sticky latest-wins fields prefer a freshly-derived
/// non-`None` value and fall back to the carried one, and the open-turn marker /
/// pending-`tool_use` set are seeded from the carry so an in-flight turn whose
/// opener aged out of the window still reports `working`/`busy`. On return `carry`
/// is updated for the next tick and the merged [`DerivedState`] is returned.
///
/// Passing a fresh `CarryState::default()` and the full file makes this identical
/// to a one-shot full-file derive (that is exactly what [`derive_state`] does).
pub fn derive_state_with_carry(
    lines: &[String],
    cfg: &Config,
    carry: &mut CarryState,
) -> DerivedState {
    let parsed = parse_and_dedupe(lines);

    // Seed from the carried state so values that aged out of the window survive.
    let mut state = DerivedState {
        model: carry.model.clone(),
        branch: carry.branch.clone(),
        tokens_total: carry.tokens_total,
        context_tokens: carry.context_tokens,
        usage: carry.usage,
        title: carry.title.clone(),
        activity: carry.activity.clone(),
        ..DerivedState::default()
    };
    let mut pending_tool_uses: HashSet<String> = carry.pending_tool_uses.clone();
    // Turn state: a genuine user prompt opens a turn (the model now owes output —
    // this is what keeps a long *thinking* pause active even when nothing is
    // written for minutes); a terminal assistant message closes it. A pending
    // tool_use or a streaming partial keeps it open. Seeded from the carry so an
    // in-flight turn whose opener scrolled out of the window stays open.
    let mut in_turn = carry.in_turn;

    for entry in &parsed {
        let line = &entry.line;
        // Branch: take the most recent non-empty, non-detached value seen.
        if let Some(branch) = normalise_branch(line.git_branch.as_deref()) {
            state.branch = Some(branch);
        }

        // ai-title line: the model-generated session title (FR-2/AC-3). Carried
        // raw — emission is gated by privacy::ai_title at the aggregator.
        if let Some(title) = line.ai_title.as_deref() {
            let title = title.trim();
            if !title.is_empty() {
                state.title = Some(title.to_string());
            }
        }

        // last_event: newest parsed, non-sidechain line carrying a usable
        // per-line timestamp drives focus recency (FR-5/AC-1, AC-4).
        if line.is_sidechain != Some(true) {
            if let Some(ts) = line.timestamp.as_deref().and_then(parse_iso8601) {
                state.last_event = Some(ts);
            }
        }

        // tool_result blocks (carried on user lines) resolve pending tool_use ids.
        let tool_results = tool_result_ids(&entry.raw);
        for id in &tool_results {
            pending_tool_uses.remove(id);
        }

        // A real user prompt — a `user` line on the main transcript that is not
        // just a tool_result echo — opens a turn: the model now owes a response.
        if line.r#type.as_deref() == Some("user")
            && line.is_sidechain != Some(true)
            && tool_results.is_empty()
        {
            in_turn = true;
        }

        if !line.is_assistant() {
            continue;
        }
        let Some(message) = line.message.as_ref() else {
            continue;
        };

        // The turn stays open while the assistant is still streaming (`null`) or
        // has paused to call a tool (`tool_use`); any terminal stop_reason
        // (`end_turn`, `stop_sequence`, …) closes it — the model is now idle,
        // waiting for the user.
        in_turn = matches!(message.stop_reason.as_deref(), None | Some("tool_use"));
        // A terminal stop_reason means the turn completed; the API contract
        // guarantees a completed turn left no `tool_use` unresolved. Clear any
        // still-pending ids so a completed-turn-in-window degrades BOTH `working`
        // and `busy` even if the resolving `tool_result` for an id whose
        // originating line aged out of the window was never seen here (belt-and-
        // suspenders alongside the watch-loop resync guard; correct and cheap).
        if !in_turn {
            pending_tool_uses.clear();
        }

        // Model / usage: latest assistant request wins, but ignore Claude Code's
        // `<synthetic>` sentinel — it tags injected / non-API messages (interrupts,
        // local-command output) and would otherwise clobber the real model
        // (e.g. `claude-opus-4-8`) with a trailing synthetic line until the next
        // genuine response arrives.
        if let Some(model) = message.model.as_deref() {
            if !model.is_empty() && model != "<synthetic>" {
                state.model = Some(model.to_string());
            }
        }
        if let Some(usage) = message.usage.as_ref() {
            state.tokens_total = Some(usage.total_tokens());
            state.context_tokens = Some(usage.context_tokens());
            // Map the schema usage into the pricing buckets for the cost fallback
            // (FR-3/AC-3): the nested cache_creation 5m/1h split when present,
            // else the flat field as a single 5m bucket.
            state.usage = Some(usage_to_pricing(usage));
        }

        // Activity + pending set: every assistant tool_use is provisionally
        // pending until a later tool_result clears it. The latest tool_use also
        // becomes the current activity.
        for block in message.content.iter().filter(|b| b.is_tool_use()) {
            if let Some(id) = block.id.as_deref() {
                pending_tool_uses.insert(id.to_string());
            }
            if let Some(name) = block.name.as_deref() {
                state.activity = Some(map_activity(name, block.input.as_ref(), cfg));
            }
        }
    }

    // Busy iff any assistant tool_use is still unmatched (FR-2/AC-5).
    state.busy = !pending_tool_uses.is_empty();
    // Working iff a turn is in progress — covers thinking and the gaps between
    // tool calls, not just a pending tool. The primary "session is active" signal.
    state.working = in_turn;

    // Write the merged state back into the carry for the next bounded tick. The
    // sticky fields are taken from `state` (already merged: fresh-non-None else
    // carried); the turn/busy markers reflect this window's resolution.
    carry.model = state.model.clone();
    carry.branch = state.branch.clone();
    carry.tokens_total = state.tokens_total;
    carry.context_tokens = state.context_tokens;
    carry.usage = state.usage;
    carry.title = state.title.clone();
    carry.activity = state.activity.clone();
    carry.in_turn = in_turn;
    carry.pending_tool_uses = pending_tool_uses;

    state
}

/// Map a [`crate::claude::schema::Usage`] into the [`pricing::Usage`] buckets used
/// by [`pricing::cost`] (FR-3/AC-3). Cache-creation is taken from the nested
/// `cache_creation` object's 5m/1h split when present; otherwise the flat
/// `cache_creation_input_tokens` is treated as a single 5m bucket.
fn usage_to_pricing(usage: &crate::claude::schema::Usage) -> pricing::Usage {
    let (cc5m, cc1h) = match usage.cache_creation.as_ref() {
        Some(cc) => (
            cc.ephemeral_5m_input_tokens.unwrap_or(0),
            cc.ephemeral_1h_input_tokens.unwrap_or(0),
        ),
        None => (usage.cache_creation_input_tokens.unwrap_or(0), 0),
    };
    pricing::Usage {
        input: usage.input_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
        cache_read: usage.cache_read_input_tokens.unwrap_or(0),
        cache_create_5m: cc5m,
        cache_create_1h: cc1h,
    }
}

/// Parse the CLI's ISO-8601 UTC timestamp (`2026-06-20T22:35:23.000Z`) into a
/// [`SystemTime`] without pulling in a date crate.
///
/// Accepts the fixed `YYYY-MM-DDTHH:MM:SS[.fff]Z` shape the CLI emits; anything
/// else returns `None` so the watcher falls back to the file mtime. Computed via
/// a days-since-epoch civil-date conversion (UTC only — the CLI always emits `Z`).
fn parse_iso8601(ts: &str) -> Option<SystemTime> {
    let ts = ts.trim();
    // Split date and time on 'T'; require a trailing 'Z' (UTC).
    let (date, rest) = ts.split_once('T')?;
    let time = rest.strip_suffix('Z')?;

    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Strip any fractional-seconds suffix; we only use whole-second precision.
    let time_main = time.split('.').next()?;
    let mut t = time_main.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Days from the Unix epoch (1970-01-01) for a civil (proleptic Gregorian) date.
/// Howard Hinnant's `days_from_civil` algorithm (public domain).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// A parsed transcript line kept alongside its raw JSON [`Value`].
///
/// The schema adapter (read-only, task 1.1) models a content block's `id` but
/// does not capture a `tool_result` block's correlation field (`tool_use_id`), so
/// busy/idle matching (FR-2/AC-5) reads that id from the raw JSON. Carrying the
/// raw value lets us do so without re-parsing or editing the adapter.
struct DedupedLine {
    line: TranscriptLine,
    raw: Value,
}

/// Parse all lines tolerantly and collapse streaming partials by `message.id`.
///
/// Lines without a `message.id` (user lines, system lines, non-assistant lines)
/// are kept verbatim and in order — only assistant partials sharing an id are
/// deduped. A non-null `stop_reason` line replaces a previously-kept `null` one
/// for the same id; a later line otherwise replaces an earlier one.
fn parse_and_dedupe(lines: &[String]) -> Vec<DedupedLine> {
    // Indices into `out` keyed by message.id, so we can replace in place and
    // preserve overall ordering for everything else.
    let mut out: Vec<DedupedLine> = Vec::with_capacity(lines.len());
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for raw in lines {
        // Parse each line exactly once (F17). The raw `Value` is needed for
        // tool_result correlation; the typed `TranscriptLine` is derived from that
        // same `Value` (no second `serde_json::from_str`). A blank or
        // malformed/truncated line is skipped, never fatal (FR-2/AC-1).
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                debug!(%err, "skipping malformed transcript line");
                continue;
            }
        };
        let line = match TranscriptLine::deserialize(&value) {
            Ok(line) => line,
            Err(err) => {
                debug!(%err, "skipping malformed transcript line");
                continue;
            }
        };
        let entry = DedupedLine { line, raw: value };

        let id = entry.line.message.as_ref().and_then(|m| m.id.clone());
        match id {
            Some(id) => match by_id.get(&id).copied() {
                Some(idx) => {
                    // Same streaming turn: prefer the final (non-null stop_reason)
                    // copy; otherwise the newer copy wins (FR-2/AC-2).
                    if prefer_replacement(&out[idx].line, &entry.line) {
                        out[idx] = entry;
                    }
                }
                None => {
                    by_id.insert(id, out.len());
                    out.push(entry);
                }
            },
            None => out.push(entry),
        }
    }
    out
}

/// Whether `candidate` should replace the currently-kept `existing` line for the
/// same `message.id`. A non-null `stop_reason` always wins; a candidate that is
/// also non-null (a later final copy) replaces; a `null` candidate never demotes
/// a kept non-null line.
fn prefer_replacement(existing: &TranscriptLine, candidate: &TranscriptLine) -> bool {
    let existing_final = has_stop_reason(existing);
    let candidate_final = has_stop_reason(candidate);
    match (existing_final, candidate_final) {
        // Kept is final, candidate is a partial → keep the final.
        (true, false) => false,
        // Otherwise the candidate (final, or a newer partial) wins.
        _ => candidate_final || !existing_final,
    }
}

/// `true` when the line's message carries a non-null `stop_reason`.
fn has_stop_reason(line: &TranscriptLine) -> bool {
    line.message
        .as_ref()
        .and_then(|m| m.stop_reason.as_deref())
        .is_some()
}

/// Extract the correlation ids of any `tool_result` content blocks on a line,
/// read from the **raw** JSON (FR-2/AC-5).
///
/// In live transcripts the matching `tool_result` is the next user line's
/// `message.content[]` block `{type:"tool_result", tool_use_id, content,
/// is_error}` (verified, dossier lane A1). The schema adapter (read-only) does
/// not rename `tool_use_id` onto its `ContentBlock.id`, so we read it straight
/// from the raw `message.content[]` array. Both `tool_use_id` (the real field)
/// and a bare `id` (a defensive alternate) are accepted.
fn tool_result_ids(raw: &Value) -> Vec<String> {
    let Some(content) = raw
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| {
            block
                .get("tool_use_id")
                .or_else(|| block.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

/// Normalise a raw `gitBranch` value: an empty string, a literal `HEAD`, or a
/// detached marker collapses to `None` so the card never shows a meaningless
/// branch (FR-2/AC-3). Any other non-empty value is returned trimmed.
fn normalise_branch(branch: Option<&str>) -> Option<String> {
    let b = branch?.trim();
    if b.is_empty() || b == "HEAD" {
        return None;
    }
    Some(b.to_string())
}

// ---------------------------------------------------------------------------
// Subagent counting + token attribution (FR-2/AC-6)
// ---------------------------------------------------------------------------

/// The result of scanning a session's `subagents` tree: how many subagents are
/// live right now and the tokens attributed to them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentScan {
    /// Number of live subagents (FR-2/AC-6).
    pub count: u32,
    /// Sum of the latest-request token totals across the counted agent
    /// transcripts, or `None` when there are no live subagents.
    pub tokens: Option<u64>,
}

/// Scan a session's `subagents/**` tree for live subagents and their tokens.
///
/// Subagents are in-process (not separate PIDs), so the filesystem is the only
/// way to surface them; the result is reported in `details`/`state` and **never**
/// added to `party.size`. Two signals are combined so the count survives the
/// open-append-close write cadence (a running agent's file mtime goes stale
/// between messages):
///
/// * **Workflow agents** (`subagents/workflows/<wf>/`) are tallied from the
///   workflow's `journal.jsonl`: an agent is live iff its `started` event has no
///   matching terminal event (keyed by `key`). This is exact and
///   mtime-independent — a finished workflow contributes 0 even while its files
///   are still fresh, and a running one counts every in-flight agent even while
///   none happen to be writing.
/// * **Flat agents** (`subagents/agent-<hex>.jsonl`, the Agent/Task tool) carry
///   no journal, so they fall back to the `isSidechain` + recent-mtime heuristic.
///
/// For every agent counted live, its transcript's latest-request total
/// (`context + output`, matching the top-level `tokens_total`) is summed into
/// `tokens`. Any I/O error degrades to the partial result (NFR-2); a missing
/// directory is simply an empty scan.
///
/// `subagents_dir` is `<transcript_dir>/<sessionId>/subagents`.
pub fn scan_subagents(subagents_dir: &Path, recency: Duration) -> SubagentScan {
    let now = SystemTime::now();
    let mut count = 0u32;
    let mut tokens = 0u64;
    scan_dir(subagents_dir, now, recency, &mut count, &mut tokens);
    SubagentScan {
        count,
        tokens: (count > 0).then_some(tokens),
    }
}

/// Backwards-compatible live-subagent count (just [`SubagentScan::count`]).
pub fn count_subagents(subagents_dir: &Path, recency: Duration) -> u32 {
    scan_subagents(subagents_dir, recency).count
}

/// Recurse one directory of the subagents tree, accumulating the live count and
/// token sum. A directory carrying a `journal.jsonl` is a workflow run and is
/// tallied authoritatively from the journal (and **not** descended into further,
/// so its agents are never double-counted); any other directory is recursed and
/// its flat `agent-*.jsonl` files are mtime-counted.
fn scan_dir(dir: &Path, now: SystemTime, recency: Duration, count: &mut u32, tokens: &mut u64) {
    let journal = dir.join("journal.jsonl");
    if journal.is_file() {
        // The journal is authoritative for *which* agents started and resolved,
        // but a failed or interrupted agent can leave a `started` with no terminal
        // event — and after the run ends those orphans would otherwise be counted
        // as live forever (observed: journals hundreds of hours old still inflating
        // the "N×" count). Gate them by recency: an unresolved agent is live only
        // while the run is still active (its journal was freshly appended as agents
        // start/finish) OR that specific agent transcript is still being written.
        // This preserves the mtime-independence a *running* workflow needs — a busy
        // run keeps its journal fresh, and a lone long agent keeps its own file
        // fresh — while letting a finished/aborted run decay to zero.
        let journal_fresh = recently_modified_path(&journal, now, recency);
        for agent_id in live_workflow_agent_ids(&journal) {
            let agent_path = dir.join(format!("agent-{agent_id}.jsonl"));
            if !journal_fresh && !recently_modified_path(&agent_path, now, recency) {
                continue;
            }
            *count += 1;
            if let Some(t) = last_request_tokens(&agent_path) {
                *tokens = tokens.saturating_add(t);
            }
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            scan_dir(&path, now, recency, count, tokens);
            continue;
        }
        if !is_agent_file(&path) || !recently_modified(&entry, now, recency) {
            continue;
        }
        if is_sidechain_file(&path) {
            *count += 1;
            if let Some(t) = last_request_tokens(&path) {
                *tokens = tokens.saturating_add(t);
            }
        }
    }
}

/// The `agentId`s of workflow subagents that are live right now: those whose
/// `started` journal event has no matching terminal event (keyed by `key`). A
/// malformed / partly-written line is skipped (best-effort, NFR-2).
fn live_workflow_agent_ids(journal: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(journal) else {
        return Vec::new();
    };
    let mut started: Vec<(String, String)> = Vec::new(); // (key, agentId)
    let mut done: HashSet<String> = HashSet::new(); // resolved keys
    for line in content.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let key = value.get("key").and_then(Value::as_str);
        match value.get("type").and_then(Value::as_str) {
            Some("started") => {
                if let (Some(key), Some(id)) = (key, value.get("agentId").and_then(Value::as_str)) {
                    started.push((key.to_string(), id.to_string()));
                }
            }
            // Any terminal event resolves the agent (workflows emit `result`).
            Some("result" | "error" | "finished" | "completed") => {
                if let Some(key) = key {
                    done.insert(key.to_string());
                }
            }
            _ => {}
        }
    }
    started
        .into_iter()
        .filter(|(key, _)| !done.contains(key))
        .map(|(_, id)| id)
        .collect()
}

/// The latest-request total tokens (`context + output`) of a subagent transcript,
/// read from a bounded tail so a multi-MB file is never slurped (NFR-1). Returns
/// `None` when the file is missing/unreadable or carries no assistant usage.
fn last_request_tokens(path: &Path) -> Option<u64> {
    const TAIL_CAP: u64 = 64 * 1024;
    for line in read_tail_lines(path, TAIL_CAP).iter().rev() {
        if let Ok(Some(parsed)) = parse_transcript_line(line) {
            if parsed.is_assistant() {
                if let Some(usage) = parsed.message.as_ref().and_then(|m| m.usage.as_ref()) {
                    return Some(usage.total_tokens());
                }
            }
        }
    }
    None
}

/// Read the last `cap` bytes of a file as complete lines, dropping a leading
/// partial line when the file is longer than `cap`. Lossy UTF-8 so a tail that
/// begins mid-multibyte-char never fails the read.
fn read_tail_lines(path: &Path, cap: u64) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    let start = len.saturating_sub(cap);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    // When we seeked past the start, the first line is a partial — drop it.
    if start > 0 {
        lines.next();
    }
    lines.map(str::to_string).collect()
}

/// `true` for an `agent-*.jsonl` filename (the subagent transcript convention).
fn is_agent_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.starts_with("agent-") && name.ends_with(".jsonl")
}

/// Whether a dir entry's mtime is within `recency` of `now`.
fn recently_modified(entry: &std::fs::DirEntry, now: SystemTime, recency: Duration) -> bool {
    let Ok(meta) = entry.metadata() else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match now.duration_since(mtime) {
        Ok(age) => age <= recency,
        // mtime in the future (clock skew): treat as fresh.
        Err(_) => true,
    }
}

/// Recency check by path — mtime within `recency` of `now`. Mirrors
/// [`recently_modified`] (which reads a `DirEntry`'s cached metadata) for callers
/// that only hold a `Path`: the workflow journal and individual agent transcripts.
/// A missing/unreadable file is not recent; a future mtime (clock skew) is fresh.
fn recently_modified_path(path: &Path, now: SystemTime, recency: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match now.duration_since(mtime) {
        Ok(age) => age <= recency,
        Err(_) => true,
    }
}

/// Cheaply confirm a subagent transcript by checking its first line carries
/// `isSidechain:true` (FR-2/AC-6). Reads only the first line, tolerating a
/// truncated/garbage line by treating it as not-sidechain.
fn is_sidechain_file(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // Read only the first non-blank line: subagent transcripts can be multi-MB
    // and this runs every ~500ms per file, so never slurp the whole file (NFR-1).
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return false, // EOF before any non-blank line.
            Ok(_) => {
                if line.trim().is_empty() {
                    continue;
                }
                return matches!(
                    parse_transcript_line(&line),
                    Ok(Some(parsed)) if parsed.is_sidechain == Some(true)
                );
            }
            Err(_) => return false,
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental tailing (FR-2/AC-1)
// ---------------------------------------------------------------------------

/// Stateful incremental reader of one append-only transcript file.
///
/// Holds a byte offset and a **raw-byte** carry buffer for a trailing partial
/// line. Each [`Tail::read_appended`] reads from the offset to EOF, returns every
/// **complete** (newline-terminated) line appended since the last call, and leaves
/// any final line without a trailing `\n` un-consumed so it is completed on the
/// next read (FR-2/AC-1: tolerate a truncated final JSON line). The carry is kept
/// as bytes so a chunk that ends mid-multibyte-UTF-8 is buffered (not a decode
/// error) and completed on the next read (FR-4/AC-3, F18).
#[derive(Debug, Default)]
pub struct Tail {
    /// Byte offset up to which complete lines have been consumed.
    offset: u64,
    /// Raw bytes of a trailing line read without a terminating newline yet (may
    /// also hold an incomplete trailing UTF-8 sequence).
    carry: Vec<u8>,
}

impl Tail {
    /// A fresh tail starting at offset 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// A tail seeked to the current end of `path`, so only **future** appends are
    /// surfaced (used when first attaching to an already-large live transcript).
    pub fn at_end(path: &Path) -> Self {
        let offset = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Self {
            offset,
            carry: Vec::new(),
        }
    }

    /// Read and return the complete lines appended since the last call.
    ///
    /// A file shorter than the recorded offset (truncated/rotated) resets the
    /// tail to the start so nothing is missed. Returns an empty vec when there is
    /// nothing new. The trailing partial line (no `\n`) — including an incomplete
    /// final UTF-8 sequence — is buffered, not returned.
    pub fn read_appended(&mut self, path: &Path) -> std::io::Result<Vec<String>> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        if len < self.offset {
            // File shrank (rotation/compaction): re-read from the top.
            self.offset = 0;
            self.carry.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let mut chunk = Vec::new();
        let read = file.take(len - self.offset).read_to_end(&mut chunk)?;
        self.offset += read as u64;

        // Prepend any buffered partial bytes from the previous read.
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(&chunk);

        let mut lines = Vec::new();
        // Carry back everything after the last newline (the still-being-written
        // final line), so only complete lines are surfaced. An incomplete trailing
        // UTF-8 sequence lives inside that carry and is completed next read.
        let split_at = match buf.iter().rposition(|&b| b == b'\n') {
            Some(idx) => idx + 1,
            None => 0,
        };
        let remainder = buf.split_off(split_at);
        // `buf` now holds every complete (newline-terminated) line. Decode the
        // valid prefix; any stray invalid bytes inside a complete line are lossily
        // replaced (a complete line never ends mid-UTF-8). Split on '\n'.
        let complete = String::from_utf8_lossy(&buf);
        for segment in complete.split_terminator('\n') {
            lines.push(segment.to_string());
        }
        self.carry = remainder;
        Ok(lines)
    }
}

// ---------------------------------------------------------------------------
// notify-based watcher → watch channel (the aggregator-facing API)
// ---------------------------------------------------------------------------

/// The per-tick re-derive reads only this many trailing bytes of the transcript
/// (the same 64 KiB cap as [`last_request_tokens`]), so steady-state CPU is bounded
/// by recent activity, not total session length (F1). The carried [`CarryState`]
/// preserves anything that scrolled out of the window (NFR-1).
const RE_DERIVE_TAIL_CAP: u64 = 64 * 1024;

/// Handle to a running transcript watcher. Dropping it stops the watch (the
/// `notify` watcher and its event-loop thread are torn down).
pub struct SessionWatcher {
    /// Latest derived snapshot; the aggregator polls or awaits this.
    rx: tokio::sync::watch::Receiver<DerivedState>,
    /// Kept alive for the lifetime of the watch; dropping closes the channel and
    /// ends the loop thread.
    _shutdown: mpsc::Sender<()>,
    _handle: std::thread::JoinHandle<()>,
}

impl SessionWatcher {
    /// The aggregator-facing receiver. `borrow()` gives the current snapshot;
    /// `changed().await` yields when a new one is published.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<DerivedState> {
        self.rx.clone()
    }

    /// The current snapshot without awaiting.
    pub fn current(&self) -> DerivedState {
        self.rx.borrow().clone()
    }
}

/// Watch one live transcript and publish a fresh [`DerivedState`] on every
/// append (FR-2). The initial snapshot is seeded from a full-file derive; each
/// later tick re-derives from only the last 64 KiB ([`RE_DERIVE_TAIL_CAP`]) while
/// **carrying** the sticky fields + turn/busy markers forward in a [`CarryState`],
/// so per-line work is bounded by recent activity yet the card stays byte-identical
/// to a full-file derive under default config (F1, NFR-1).
///
/// `transcript` is the `<sessionId>.jsonl` path; its sibling
/// `<dir>/<sessionId>/subagents` directory (if present) is counted for
/// FR-2/AC-6. The returned [`SessionWatcher`] owns the `notify` watcher and a
/// small event-loop thread; drop it to stop watching.
pub fn watch_session(transcript: PathBuf, cfg: Config) -> notify::Result<SessionWatcher> {
    let recency = Duration::from_secs(cfg.subagent_recency_secs);
    let subagents_dir = subagents_dir_for(&transcript);

    // Seed the initial snapshot from the whole existing file so a session that
    // started before us is immediately reflected, capturing the carried state so
    // every later bounded (64 KiB) tick can never blank a field that scrolled out
    // of the window (F1, NFR-1).
    let mut carry = CarryState::default();
    let mut state = derive_state_with_carry(&read_all_lines(&transcript), &cfg, &mut carry);
    // Length of the file at the moment the full-file seed derive ran, so the seed
    // is not reprocessed and the resync guard can measure each tick's delta.
    let mut last_len = std::fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0);
    let scan = subagents_dir
        .as_deref()
        .map(|d| scan_subagents(d, recency))
        .unwrap_or_default();
    state.subagents = scan.count;
    state.subagent_tokens = scan.tokens;
    // Fall back to the transcript mtime when no parsed line carried a usable
    // timestamp, so focus recency still tracks the latest write (FR-5/AC-1).
    if state.last_event.is_none() {
        state.last_event = file_mtime(&transcript);
    }
    let (tx, rx) = tokio::sync::watch::channel(state);

    // notify pushes events onto a std mpsc; we drain them on a dedicated thread
    // so this function (and the async runtime) never block on FS I/O.
    let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        // A full channel / dropped receiver just means the watch is shutting down.
        let _ = event_tx.send(res);
    })?;
    // Watch the directory holding the transcript so renames/rotations are seen,
    // plus the subagents tree for FR-2/AC-6.
    if let Some(parent) = transcript.parent() {
        watcher.watch(parent, RecursiveMode::NonRecursive)?;
    }
    if let Some(dir) = subagents_dir.as_deref() {
        if dir.exists() {
            let _ = watcher.watch(dir, RecursiveMode::Recursive);
        }
    }

    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        // Keep the watcher alive for the loop's lifetime.
        let _watcher = watcher;
        loop {
            // Wake periodically even without an event so subagent mtimes ageing
            // out of the recency window are reflected, and so shutdown is prompt.
            match event_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(_event)) => {}
                Ok(Err(err)) => {
                    warn!(%err, "transcript watcher event error");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // Stop when the handle is dropped.
            if matches!(
                shutdown_rx.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ) {
                break;
            }

            // Re-derive carrying the sticky fields + turn/busy markers forward so
            // the work stays bounded without ever changing the card (F1, NFR-1).
            //
            // RESYNC GUARD (NFR-1 correctness): the 500ms tick coalesces FS appends,
            // so more than 64 KiB can land between ticks (a single large tool_result
            // — Read/Bash output is routinely tens-to-hundreds of KiB — plus the next
            // turn). A stateless 64 KiB tail would then scroll the resolving line
            // (a `tool_result` or a terminal `stop_reason`) PAST `len - 64 KiB` and
            // never re-read it, sticking `busy`/`working` forever. So pick the read
            // window from this tick's delta:
            //
            // * delta > cap (or the file shrank / rotated): the resolving line may
            //   be outside the last 64 KiB — re-derive from the WHOLE file, which
            //   rebuilds pending/in_turn from the truth and self-heals stuck state;
            // * delta <= cap: the last 64 KiB contains EVERY newly-appended line, so
            //   the bounded tail cannot miss any resolution — keep the cheap tail.
            //
            // This is exactly the old always-full-file behavior only on a >64 KiB
            // burst, and bounded in the common (small-append) case.
            let current_len = std::fs::metadata(&transcript).map(|m| m.len()).unwrap_or(0);
            let lines = if current_len < last_len
                || current_len.saturating_sub(last_len) > RE_DERIVE_TAIL_CAP
            {
                read_all_lines(&transcript)
            } else {
                read_tail_lines(&transcript, RE_DERIVE_TAIL_CAP)
            };
            last_len = current_len;
            let mut next = derive_state_with_carry(&lines, &cfg, &mut carry);
            let scan = subagents_dir
                .as_deref()
                .map(|d| scan_subagents(d, recency))
                .unwrap_or_default();
            next.subagents = scan.count;
            next.subagent_tokens = scan.tokens;
            if next.last_event.is_none() {
                next.last_event = file_mtime(&transcript);
            }

            // Publish only on change so the aggregator debounces naturally.
            if *tx.borrow() != next && tx.send(next).is_err() {
                break;
            }
        }
    });

    Ok(SessionWatcher {
        rx,
        _shutdown: shutdown_tx,
        _handle: handle,
    })
}

/// Read all newline-delimited lines of a transcript, dropping the trailing
/// partial (no `\n`) line so an in-flight append is never parsed half-written
/// (FR-2/AC-1). A missing/unreadable file yields no lines.
fn read_all_lines(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let ends_with_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    if !ends_with_newline {
        // Last line is a partial append still being written — hold it back.
        lines.pop();
    }
    lines
}

/// The transcript file's last-modified time, used as the focus-recency fallback
/// when no parsed line carried a usable per-line timestamp (FR-5/AC-1).
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The `<dir>/<sessionId>/subagents` directory for a `<sessionId>.jsonl`
/// transcript, or `None` if the stem cannot be read.
fn subagents_dir_for(transcript: &Path) -> Option<PathBuf> {
    let parent = transcript.parent()?;
    let stem = transcript.file_stem()?.to_str()?;
    Some(parent.join(stem).join("subagents"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn cfg() -> Config {
        // Default config redacts paths; that does not affect the fields tested
        // here (model/branch/tokens/busy) but keeps Bash targets to the program
        // token, which is what we assert on.
        Config::default()
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cp-transcript-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One assistant line carrying a single Bash tool_use with the given id.
    fn assistant_tool_use(msg_id: &str, tool_id: &str, stop_reason: Option<&str>) -> String {
        let stop = match stop_reason {
            Some(s) => format!("\"{s}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{"type":"assistant","gitBranch":"master","sessionId":"s1","version":"2.1.181","message":{{"id":"{msg_id}","role":"assistant","model":"claude-opus-4-8","stop_reason":{stop},"content":[{{"type":"tool_use","id":"{tool_id}","name":"Bash","input":{{"command":"cargo check"}}}}],"usage":{{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":50,"cache_creation_input_tokens":5}}}}}}"#
        )
    }

    /// A user line carrying a tool_result for the given tool id, using the real
    /// live-transcript field name `tool_use_id` (dossier lane A1).
    fn user_tool_result(tool_id: &str) -> String {
        format!(
            r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_id}","content":"ok","is_error":false}}]}}}}"#
        )
    }

    // ---- derive_state: core fields (FR-2/AC-2..AC-5) -------------------------

    #[test]
    fn derives_model_branch_tokens_and_activity() {
        let lines = vec![assistant_tool_use("m1", "t1", Some("tool_use"))];
        let s = derive_state(&lines, &cfg());
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(s.branch.as_deref(), Some("master"));
        // total = input+cache_read+cache_creation+output = 100+50+5+10 = 165
        assert_eq!(s.tokens_total, Some(165));
        // live context = input+cache_read+cache_creation = 155
        assert_eq!(s.context_tokens, Some(155));
        let act = s.activity.expect("activity present");
        assert_eq!(act.verb, "Running");
        assert_eq!(act.target.as_deref(), Some("cargo"));
    }

    #[test]
    fn synthetic_model_does_not_clobber_real_model() {
        // Claude Code writes `model:"<synthetic>"` on injected / non-API messages.
        // A trailing synthetic line must NOT overwrite the real model.
        let synthetic = r#"{"type":"assistant","sessionId":"s1","message":{"id":"m2","role":"assistant","model":"<synthetic>","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}"#.to_string();
        let lines = vec![assistant_tool_use("m1", "t1", Some("end_turn")), synthetic];
        let s = derive_state(&lines, &cfg());
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
    }

    /// The expected epoch seconds for `2026-06-20T22:35:23Z`, computed
    /// independently of `parse_iso8601` so the parser test cannot mask a bug in
    /// `days_from_civil` with a hard-coded magic constant.
    fn expected_secs_2026_06_20_223523() -> u64 {
        (days_from_civil(2026, 6, 20) * 86_400 + 22 * 3_600 + 35 * 60 + 23) as u64
    }

    #[test]
    fn parses_iso8601_utc_timestamp() {
        let st = parse_iso8601("2026-06-20T22:35:23.000Z").expect("parses");
        let secs = st.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, expected_secs_2026_06_20_223523());
        // The Unix epoch round-trips to 0.
        assert_eq!(
            parse_iso8601("1970-01-01T00:00:00Z")
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            0
        );
        // Malformed / non-UTC inputs return None (watcher falls back to mtime).
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("2026-06-20 22:35:23").is_none());
        assert!(parse_iso8601("2026-13-01T00:00:00Z").is_none());
    }

    #[test]
    fn last_event_from_newest_non_sidechain_line() {
        let line = r#"{"type":"assistant","timestamp":"2026-06-20T22:35:23.000Z","message":{"id":"m1","model":"claude-opus-4-8","stop_reason":"end_turn"}}"#;
        let s = derive_state(&[line.to_string()], &cfg());
        let got = s
            .last_event
            .expect("last_event set")
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(got, expected_secs_2026_06_20_223523());
    }

    #[test]
    fn derives_usage_buckets_for_cost_fallback() {
        let line = r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-4-8","stop_reason":"end_turn","usage":{"input_tokens":131,"output_tokens":12570,"cache_read_input_tokens":59709,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":10493}}}}"#;
        let s = derive_state(&[line.to_string()], &cfg());
        let u = s.usage.expect("usage mapped");
        assert_eq!(u.input, 131);
        assert_eq!(u.output, 12570);
        assert_eq!(u.cache_read, 59709);
        assert_eq!(u.cache_create_5m, 0);
        assert_eq!(u.cache_create_1h, 10493);
    }

    #[test]
    fn extracts_ai_title_line() {
        let line = r#"{"type":"ai-title","aiTitle":"Refactor the parser","sessionId":"s1"}"#;
        let s = derive_state(&[line.to_string()], &cfg());
        assert_eq!(s.title.as_deref(), Some("Refactor the parser"));
    }

    #[test]
    fn head_branch_is_normalised_to_none() {
        let line = r#"{"type":"assistant","gitBranch":"HEAD","message":{"id":"m1","model":"claude-opus-4-8","stop_reason":"end_turn"}}"#;
        let s = derive_state(&[line.to_string()], &cfg());
        assert_eq!(s.branch, None);
    }

    // ---- busy / idle (FR-2/AC-5) ---------------------------------------------

    #[test]
    fn busy_when_tool_use_has_no_result() {
        let lines = vec![assistant_tool_use("m1", "t1", Some("tool_use"))];
        let s = derive_state(&lines, &cfg());
        assert!(s.busy, "a dangling tool_use must read as busy");
    }

    #[test]
    fn idle_when_tool_result_follows() {
        let lines = vec![
            assistant_tool_use("m1", "t1", Some("tool_use")),
            user_tool_result("t1"),
        ];
        let s = derive_state(&lines, &cfg());
        assert!(!s.busy, "a matched tool_use must read as idle");
    }

    #[test]
    fn busy_when_only_some_results_present() {
        let lines = vec![
            assistant_tool_use("m1", "t1", Some("tool_use")),
            user_tool_result("t1"),
            assistant_tool_use("m2", "t2", Some("tool_use")),
        ];
        let s = derive_state(&lines, &cfg());
        assert!(s.busy, "an unmatched later tool_use must read as busy");
        // The latest activity reflects the second tool_use.
        assert!(s.activity.is_some());
    }

    // ---- turn state / `working` (active incl. long thinking) -----------------

    fn assistant_text_end_turn(msg_id: &str) -> String {
        format!(
            r#"{{"type":"assistant","sessionId":"s1","message":{{"id":"{msg_id}","role":"assistant","model":"claude-opus-4-8","stop_reason":"end_turn","content":[{{"type":"text","text":"done"}}]}}}}"#
        )
    }

    fn user_prompt(text: &str) -> String {
        format!(
            r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    #[test]
    fn working_while_thinking_after_user_prompt() {
        // The 5-minute-think case: a previous turn finished, the user sent a new
        // prompt, and the model is reasoning — nothing written yet, no pending
        // tool. `busy` is false but `working` must be true (active).
        let lines = vec![assistant_text_end_turn("m1"), user_prompt("please think")];
        let s = derive_state(&lines, &cfg());
        assert!(!s.busy, "no pending tool_use → not busy");
        assert!(
            s.working,
            "a prompt awaiting the model's turn must read as working"
        );
    }

    #[test]
    fn idle_after_assistant_completes_turn() {
        // Prompt answered with end_turn and nothing newer → idle, waiting for user.
        let lines = vec![user_prompt("hi"), assistant_text_end_turn("m1")];
        let s = derive_state(&lines, &cfg());
        assert!(!s.busy);
        assert!(!s.working, "a completed turn (end_turn) must read as idle");
    }

    #[test]
    fn working_between_tool_calls_even_when_not_busy() {
        // Mid-turn: a tool just returned (so not busy) but the turn has not ended,
        // so the session is still working (covers the gaps between tools).
        let lines = vec![
            user_prompt("go"),
            assistant_tool_use("m1", "t1", Some("tool_use")),
            user_tool_result("t1"),
        ];
        let s = derive_state(&lines, &cfg());
        assert!(!s.busy, "the only tool_use is matched → not busy");
        assert!(
            s.working,
            "an unfinished turn between tools must read as working"
        );
    }

    #[test]
    fn tool_result_alone_is_not_a_user_prompt() {
        // A lone tool_result echo (a `user` line) must NOT be mistaken for a new
        // prompt and re-open a finished turn.
        let lines = vec![assistant_text_end_turn("m1"), user_tool_result("t1")];
        let s = derive_state(&lines, &cfg());
        assert!(
            !s.working,
            "a tool_result must not re-open a completed turn"
        );
    }

    // ---- streaming-partial dedupe (FR-2/AC-2) --------------------------------

    #[test]
    fn streaming_partials_dedupe_to_final_line() {
        // Two partials (stop_reason:null) then the final (non-null) — all share
        // message.id m1. The final tool_use id is t-final; only it should be the
        // pending (busy) tool, and tokens come from the final copy.
        let partial1 = assistant_tool_use("m1", "t-stream", None);
        let partial2 = assistant_tool_use("m1", "t-stream", None);
        let finalc = r#"{"type":"assistant","gitBranch":"master","message":{"id":"m1","model":"claude-opus-4-8","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t-final","name":"Bash","input":{"command":"cargo test"}}],"usage":{"input_tokens":200,"output_tokens":20}}}"#.to_string();
        let lines = vec![partial1, partial2, finalc];
        let s = derive_state(&lines, &cfg());

        // Exactly one pending tool_use (the final's), so busy with that activity.
        assert!(s.busy);
        let act = s.activity.expect("activity");
        assert_eq!(act.target.as_deref(), Some("cargo"));
        // Tokens taken from the final copy (input 200 + output 20).
        assert_eq!(s.tokens_total, Some(220));
        assert_eq!(s.context_tokens, Some(200));
    }

    #[test]
    fn final_then_partial_keeps_final() {
        // Even if a stray null-stop_reason copy arrives after the final, the
        // non-null final must be preferred (prefer_replacement guarantee).
        let finalc = assistant_tool_use("m1", "t1", Some("tool_use"));
        let stray = assistant_tool_use("m1", "t1", None);
        let s_final_first = derive_state(&[finalc.clone(), stray.clone()], &cfg());
        let s_stray_first = derive_state(&[stray, finalc], &cfg());
        // Both orders converge: the kept line has the final's content.
        assert!(s_final_first.busy);
        assert!(s_stray_first.busy);
        assert_eq!(s_final_first.tokens_total, s_stray_first.tokens_total);
    }

    #[test]
    fn truncated_final_line_is_tolerated() {
        // A complete line followed by a truncated JSON line must not panic, and
        // the good line still derives (FR-2/AC-1).
        let good = assistant_tool_use("m1", "t1", Some("tool_use"));
        let truncated = r#"{"type":"assistant","mess"#.to_string();
        let s = derive_state(&[good, truncated], &cfg());
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert!(s.busy);
    }

    // ---- bounded re-derive with carried state (F1, FR-4/AC-1) ----------------

    /// Build a transcript whose total byte length exceeds [`RE_DERIVE_TAIL_CAP`]
    /// by padding between the early (pre-window) lines and the recent (in-window)
    /// lines, write it, and return the lines that a 64 KiB bounded read surfaces.
    /// This reproduces what a real multi-MB session feeds a bounded tick: the early
    /// lines have aged out of the window.
    fn windowed_tail(dir: &Path, early: &[String], recent: &[String]) -> Vec<String> {
        let path = dir.join("big.jsonl");
        // ~3x the cap of inert (parse-as-None) filler so the early lines are well
        // outside the 64 KiB window.
        let filler_line = r#"{"type":"summary","leafUuid":"x"}"#;
        let filler_count = (RE_DERIVE_TAIL_CAP as usize * 3) / (filler_line.len() + 1);
        let mut body = String::new();
        for line in early {
            body.push_str(line);
            body.push('\n');
        }
        for _ in 0..filler_count {
            body.push_str(filler_line);
            body.push('\n');
        }
        for line in recent {
            body.push_str(line);
            body.push('\n');
        }
        fs::write(&path, &body).unwrap();
        let tail = read_tail_lines(&path, RE_DERIVE_TAIL_CAP);
        // Sanity: the early lines really did scroll out of the window.
        for e in early {
            assert!(
                !tail.iter().any(|l| l == e),
                "early line should be outside the 64 KiB window"
            );
        }
        tail
    }

    #[test]
    fn carried_state_keeps_model_usage_title_outside_window() {
        // A multi-MB session whose model/usage/ai-title lines precede the 64 KiB
        // window: after a bounded tick those fields must still be reported, sourced
        // from the carried state (NFR-1 — the window may not blank the card).
        let dir = unique_dir("carry-sticky");
        // Seed from a full-file derive of the EARLY lines (as watch_session does at
        // start) so the carry holds the model/usage/title.
        let title = r#"{"type":"ai-title","aiTitle":"Refactor the parser","sessionId":"s1"}"#;
        let early = vec![
            title.to_string(),
            assistant_tool_use("m1", "t1", Some("end_turn")),
        ];
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&early, &cfg(), &mut carry);
        assert_eq!(seed.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(seed.title.as_deref(), Some("Refactor the parser"));
        assert!(seed.tokens_total.is_some());

        // A later bounded tick sees only recent, model/usage/title-less lines.
        let recent = vec![user_prompt("next please")];
        let tail = windowed_tail(&dir, &early, &recent);
        let next = derive_state_with_carry(&tail, &cfg(), &mut carry);

        assert_eq!(
            next.model.as_deref(),
            Some("claude-opus-4-8"),
            "model carried across the window"
        );
        assert_eq!(
            next.title.as_deref(),
            Some("Refactor the parser"),
            "ai-title carried across the window"
        );
        assert!(
            next.tokens_total.is_some(),
            "tokens_total carried (ctx%/pricing denominator never blanked)"
        );
        assert!(next.context_tokens.is_some(), "context_tokens carried");
        assert!(next.usage.is_some(), "usage carried");
        assert!(next.activity.is_some(), "activity carried");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn carried_state_keeps_in_flight_turn_working_and_busy() {
        // An in-flight turn whose `user` opener AND originating `tool_use` predate
        // the window must still report working/busy (the markers are carried, not
        // recomputed from the naive slice).
        let dir = unique_dir("carry-turn");
        let early = vec![
            user_prompt("do the thing"),
            assistant_tool_use("m1", "t1", Some("tool_use")), // pending tool_use
        ];
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&early, &cfg(), &mut carry);
        assert!(
            seed.working && seed.busy,
            "turn open + tool pending at seed"
        );

        // The window contains only a neutral line that, on its own, neither opens
        // a turn nor adds/clears a pending tool — so a naive re-derive of the slice
        // would report idle/not-busy. Only the carry keeps the turn alive.
        let recent = vec![r#"{"type":"summary","leafUuid":"y"}"#.to_string()];
        let tail = windowed_tail(&dir, &early, &recent);
        let naive = derive_state(&tail, &cfg());
        assert!(
            !naive.working && !naive.busy,
            "guard: a naive (no-carry) derive of this window is idle — carry is what saves it"
        );
        let next = derive_state_with_carry(&tail, &cfg(), &mut carry);

        assert!(
            next.working,
            "in-flight turn whose opener aged out still reports working"
        );
        assert!(
            next.busy,
            "pending tool_use whose line aged out still reports busy"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn carried_state_resolves_busy_when_result_enters_window() {
        // The carried pending tool_use clears once its tool_result lands in a later
        // window, even though the originating tool_use line aged out.
        let dir = unique_dir("carry-resolve");
        let early = vec![
            user_prompt("go"),
            assistant_tool_use("m1", "t1", Some("tool_use")),
        ];
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&early, &cfg(), &mut carry);
        assert!(seed.busy);

        let recent = vec![user_tool_result("t1")];
        let tail = windowed_tail(&dir, &early, &recent);
        let next = derive_state_with_carry(&tail, &cfg(), &mut carry);
        assert!(
            !next.busy,
            "tool_result in-window clears the carried pending tool_use"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn carried_state_degrades_completed_turn_to_idle() {
        // A turn that fully completes (terminal stop_reason observed in a window)
        // degrades to idle even though it opened before the window. Here the
        // terminal `end_turn` lands inside the window.
        let dir = unique_dir("carry-idle");
        let early = vec![user_prompt("answer me")];
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&early, &cfg(), &mut carry);
        assert!(seed.working, "turn open at seed");

        // The window carries the terminal assistant line that closes the turn.
        let recent = vec![assistant_text_end_turn("m9")];
        let tail = windowed_tail(&dir, &early, &recent);
        let next = derive_state_with_carry(&tail, &cfg(), &mut carry);
        assert!(
            !next.working,
            "a completed (terminal stop_reason) turn degrades to idle"
        );
        assert!(!next.busy);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bounded_derive_matches_full_derive_for_in_window_session() {
        // When the whole session fits in the window, a bounded carried derive is
        // byte-identical to a one-shot full-file derive (NFR-1 baseline).
        let lines = vec![
            user_prompt("hello"),
            assistant_tool_use("m1", "t1", Some("tool_use")),
            user_tool_result("t1"),
            assistant_text_end_turn("m2"),
        ];
        let full = derive_state(&lines, &cfg());
        let mut carry = CarryState::default();
        let bounded = derive_state_with_carry(&lines, &cfg(), &mut carry);
        assert_eq!(full, bounded);
    }

    #[test]
    fn terminal_stop_reason_clears_carried_pending_tool_use() {
        // Companion fix: a turn opens with a pending tool_use, then a LATER tick's
        // window carries only a terminal `end_turn` assistant line — the resolving
        // `tool_result` for t1 was never seen in any window (it scrolled past). The
        // completed-turn contract guarantees no tool_use was left unresolved, so the
        // terminal stop_reason must clear the carried pending set: BOTH working and
        // busy degrade to false. Without the fix, `busy` would stick true forever.
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(
            &[
                user_prompt("go"),
                assistant_tool_use("m1", "t1", Some("tool_use")),
            ],
            &cfg(),
            &mut carry,
        );
        assert!(
            seed.working && seed.busy,
            "turn open + tool pending at seed"
        );

        // A window with ONLY the terminal assistant line (no tool_result for t1).
        let next = derive_state_with_carry(&[assistant_text_end_turn("m2")], &cfg(), &mut carry);
        assert!(!next.working, "terminal stop_reason closes the turn");
        assert!(
            !next.busy,
            "terminal stop_reason clears the carried pending tool_use (completed turn left none unresolved)"
        );
    }

    /// The watch loop's own tail-vs-full choice, lifted out so the regression test
    /// exercises the exact resync-guard predicate (delta > cap → full re-derive).
    /// Returns the lines the watcher would feed `derive_state_with_carry` this tick
    /// and the updated `last_len`.
    fn watch_tick_lines(path: &Path, last_len: u64) -> (Vec<String>, u64) {
        let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let lines = if current_len < last_len
            || current_len.saturating_sub(last_len) > RE_DERIVE_TAIL_CAP
        {
            read_all_lines(path)
        } else {
            read_tail_lines(path, RE_DERIVE_TAIL_CAP)
        };
        (lines, current_len)
    }

    #[test]
    fn resync_guard_full_rederive_on_oversized_burst_clears_stuck_busy() {
        // The HIGH defect: the 500ms tick coalesces FS appends, so a single large
        // tool_result (Read/Bash output is routinely >64 KiB) can land between ticks.
        // With a STATELESS 64 KiB tail the resolving `tool_result` scrolls PAST
        // `len - 64KiB` and is never re-read → the carried pending tool_use sticks →
        // `busy` stuck `true` forever. The watch-loop resync guard detects
        // `delta > RE_DERIVE_TAIL_CAP` and does a FULL-file re-derive that sees the
        // resolution and self-heals.
        //
        // Crucially the turn STAYS OPEN across the burst (the trailing assistant line
        // is a `tool_use`, not a terminal `end_turn`), so the companion stop_reason
        // clear does NOT fire — `busy` here is cleared SOLELY by the resync guard's
        // full re-derive, isolating the fix under test.
        let dir = unique_dir("resync-burst-busy");
        let path = dir.join("burst.jsonl");

        // Seed tick: open turn + pending tool_use t1. Record last_len as the watcher
        // does right after its full-file seed derive.
        let mut body = String::new();
        body.push_str(&user_prompt("do the big thing"));
        body.push('\n');
        body.push_str(&assistant_tool_use("m1", "t1", Some("tool_use")));
        body.push('\n');
        fs::write(&path, &body).unwrap();
        let last_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&read_all_lines(&path), &cfg(), &mut carry);
        assert!(seed.working && seed.busy, "seed: turn open + tool pending");

        // One coalesced burst > 64 KiB: a HUGE tool_result that resolves t1, then a
        // SHORT assistant tool_use t2 (turn stays open) whose own tool_result lands
        // in-window. After it, the only UNresolved-in-a-stale-tail id is t1 — and t1's
        // resolution is the line that scrolled out of the window.
        let huge = "x".repeat(RE_DERIVE_TAIL_CAP as usize * 2);
        let big_result = format!(
            r#"{{"type":"user","sessionId":"s1","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"{huge}","is_error":false}}]}}}}"#
        );
        let mut burst = String::new();
        burst.push_str(&big_result);
        burst.push('\n');
        // t2 keeps the turn open (stop_reason tool_use) and is itself resolved
        // in-window, so neither the companion clear nor t2 can mask t1's stuck busy.
        burst.push_str(&assistant_tool_use("m2", "t2", Some("tool_use")));
        burst.push('\n');
        burst.push_str(&user_tool_result("t2"));
        burst.push('\n');
        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(burst.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        // Precondition: t1's resolving tool_result really is outside the last 64 KiB.
        let stale_tail = read_tail_lines(&path, RE_DERIVE_TAIL_CAP);
        assert!(
            !stale_tail
                .iter()
                .any(|l| l.contains("\"tool_use_id\":\"t1\"")),
            "precondition: t1's tool_result scrolled past the 64 KiB window"
        );
        // The bug being fixed: a stateless 64 KiB tail re-derive misses t1's
        // resolution and leaves `busy` stuck. The turn is still open, so the
        // companion stop_reason clear cannot help here.
        {
            let mut stuck_carry = carry.clone();
            let stuck = derive_state_with_carry(&stale_tail, &cfg(), &mut stuck_carry);
            assert!(
                stuck.busy,
                "guard: a stateless 64 KiB tail re-derive leaves busy stuck on t1 — \
                 exactly the defect the resync guard fixes"
            );
        }

        // The resync guard: delta > cap → full re-derive sees t1's resolution.
        let (lines, _new_len) = watch_tick_lines(&path, last_len);
        assert!(
            lines.iter().any(|l| l.contains("\"tool_use_id\":\"t1\"")),
            "resync guard chose a full re-derive that includes t1's resolution"
        );
        let next = derive_state_with_carry(&lines, &cfg(), &mut carry);
        assert!(
            !next.busy,
            "resync guard: the >64 KiB burst triggers a full re-derive that clears \
             the resolved t1 — busy is no longer stuck"
        );
        assert!(
            next.working,
            "the turn legitimately stays open (trailing tool_use), proving busy \
             cleared via the full re-derive and NOT via a turn-closing stop_reason"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resync_guard_full_rederive_on_oversized_burst_clears_stuck_working() {
        // The same defect for `working`: a terminal `end_turn` that closes the turn
        // can scroll past `len - 64KiB` when a >64 KiB burst lands in one tick. A
        // stateless tail never re-reads it → `in_turn` sticks → `working` stuck true.
        // The resync guard's full re-derive observes the terminal stop_reason and
        // degrades the completed turn to idle.
        let dir = unique_dir("resync-burst-working");
        let path = dir.join("burst.jsonl");

        // Seed tick: a user prompt opens a turn (working, not busy).
        fs::write(&path, format!("{}\n", user_prompt("answer me"))).unwrap();
        let last_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut carry = CarryState::default();
        let seed = derive_state_with_carry(&read_all_lines(&path), &cfg(), &mut carry);
        assert!(
            seed.working && !seed.busy,
            "seed: turn open, nothing pending"
        );

        // One coalesced burst > 64 KiB: the terminal end_turn that CLOSES the turn,
        // followed by huge filler so the end_turn scrolls out of the 64 KiB window.
        let mut burst = String::new();
        burst.push_str(&assistant_text_end_turn("m1"));
        burst.push('\n');
        let filler_line = r#"{"type":"summary","leafUuid":"x"}"#;
        let filler_count = (RE_DERIVE_TAIL_CAP as usize * 2) / (filler_line.len() + 1);
        for _ in 0..filler_count {
            burst.push_str(filler_line);
            burst.push('\n');
        }
        {
            let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(burst.as_bytes()).unwrap();
            f.flush().unwrap();
        }

        // Precondition: the closing end_turn really is outside the last 64 KiB.
        let stale_tail = read_tail_lines(&path, RE_DERIVE_TAIL_CAP);
        assert!(
            !stale_tail.iter().any(|l| l.contains("end_turn")),
            "precondition: the closing end_turn scrolled past the 64 KiB window"
        );
        // The bug: a stateless tail misses the closing line → `working` stuck true.
        {
            let mut stuck_carry = carry.clone();
            let stuck = derive_state_with_carry(&stale_tail, &cfg(), &mut stuck_carry);
            assert!(
                stuck.working,
                "guard: a stateless 64 KiB tail re-derive leaves working stuck"
            );
        }

        // The resync guard: delta > cap → full re-derive sees the closing end_turn.
        let (lines, _new_len) = watch_tick_lines(&path, last_len);
        let next = derive_state_with_carry(&lines, &cfg(), &mut carry);
        assert!(
            !next.working,
            "resync guard: the full re-derive observes the terminal end_turn — \
             working is no longer stuck"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- Tail incremental append parsing (FR-2/AC-1) -------------------------

    #[test]
    fn tail_reads_only_appended_complete_lines() {
        let dir = unique_dir("tail-append");
        let path = dir.join("s.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{}", assistant_tool_use("m1", "t1", Some("tool_use"))).unwrap();
        f.flush().unwrap();

        let mut tail = Tail::new();
        let first = tail.read_appended(&path).unwrap();
        assert_eq!(first.len(), 1);
        // No new data → empty.
        assert!(tail.read_appended(&path).unwrap().is_empty());

        // Append another complete line.
        writeln!(f, "{}", user_tool_result("t1")).unwrap();
        f.flush().unwrap();
        let second = tail.read_appended(&path).unwrap();
        assert_eq!(second.len(), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_buffers_partial_line_until_completed() {
        let dir = unique_dir("tail-partial");
        let path = dir.join("s.jsonl");
        // Write a complete line plus a partial (no trailing newline).
        let complete = assistant_tool_use("m1", "t1", Some("tool_use"));
        fs::write(&path, format!("{complete}\n{{\"type\":\"assist")).unwrap();

        let mut tail = Tail::new();
        let first = tail.read_appended(&path).unwrap();
        // Only the complete line is surfaced; the partial is buffered.
        assert_eq!(first.len(), 1);

        // Complete the partial line.
        let finish = user_tool_result("t1");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        // Overwrite the partial with a full valid line by appending the rest.
        write!(f, "ant\",\"x\":1}}\n{finish}\n").unwrap();
        f.flush().unwrap();

        let second = tail.read_appended(&path).unwrap();
        // Now the completed (previously partial) line + the new line: 2 lines.
        assert_eq!(second.len(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_resets_when_file_shrinks() {
        let dir = unique_dir("tail-shrink");
        let path = dir.join("s.jsonl");
        // Two lines, then read them.
        fs::write(
            &path,
            format!("{}\n{}\n", user_tool_result("t1"), user_tool_result("t2")),
        )
        .unwrap();
        let mut tail = Tail::new();
        assert_eq!(tail.read_appended(&path).unwrap().len(), 2);
        // Truncate/rotate to a strictly shorter file → re-read from the top.
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(tail.read_appended(&path).unwrap().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_buffers_chunk_split_mid_multibyte_char() {
        // A chunk that ends in the middle of a multibyte UTF-8 char must not fail
        // the read (F18, FR-4/AC-3): the incomplete trailing bytes are buffered and
        // completed on the next read. Use the 4-byte emoji "🦀" (F0 9F A6 80).
        let dir = unique_dir("tail-utf8");
        let path = dir.join("s.jsonl");
        let line = r#"{"type":"user","sessionId":"🦀","message":{"role":"user","content":"hi"}}"#;
        let bytes = format!("{line}\n").into_bytes();
        // Split the file write so the first chunk ends one byte into the 4-byte
        // emoji: find the emoji's first byte and cut at +1.
        let crab_start = bytes
            .windows(4)
            .position(|w| w == [0xF0, 0x9F, 0xA6, 0x80])
            .expect("emoji present");
        let cut = crab_start + 1; // mid-multibyte sequence.

        // Write the first (UTF-8-incomplete) chunk.
        fs::write(&path, &bytes[..cut]).unwrap();
        let mut tail = Tail::new();
        // No complete line yet, and crucially NO Err(InvalidData).
        let first = tail.read_appended(&path).expect("no decode error on split");
        assert!(first.is_empty(), "no complete line yet");

        // Append the rest of the multibyte char and the newline.
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&bytes[cut..]).unwrap();
        f.flush().unwrap();

        let second = tail.read_appended(&path).expect("completes cleanly");
        assert_eq!(second.len(), 1, "the completed line is surfaced");
        // The line is well-formed JSON with the emoji intact.
        let parsed = parse_transcript_line(&second[0]).unwrap().unwrap();
        assert_eq!(parsed.r#type.as_deref(), Some("user"));
        assert!(
            second[0].contains('🦀'),
            "multibyte char reassembled intact"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- subagent counting (FR-2/AC-6) ---------------------------------------

    fn write_sidechain(path: &Path, sidechain: bool) {
        let line = format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"message":{{"id":"a","model":"claude-opus-4-8"}}}}"#
        );
        fs::write(path, format!("{line}\n")).unwrap();
    }

    #[test]
    fn counts_recent_sidechain_agents_flat_and_nested() {
        let dir = unique_dir("subs");
        let subs = dir.join("subagents");
        let wf = subs.join("workflows").join("wf1");
        fs::create_dir_all(&wf).unwrap();

        // Flat agent file (sidechain, recent) — counts.
        write_sidechain(&subs.join("agent-aaaa.jsonl"), true);
        // Nested workflow agent (sidechain, recent) — counts.
        write_sidechain(&wf.join("agent-bbbb.jsonl"), true);
        // Non-sidechain agent file — not counted.
        write_sidechain(&subs.join("agent-cccc.jsonl"), false);
        // Wrong filename — not counted.
        write_sidechain(&subs.join("notes.jsonl"), true);

        let n = count_subagents(&subs, Duration::from_secs(300));
        assert_eq!(n, 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_subagents_age_out_of_recency_window() {
        let dir = unique_dir("subs-stale");
        let subs = dir.join("subagents");
        fs::create_dir_all(&subs).unwrap();
        let agent = subs.join("agent-aaaa.jsonl");
        write_sidechain(&agent, true);
        // Force an old mtime exactly 10 minutes ago (no timezone ambiguity:
        // File::set_modified takes a SystemTime directly).
        let old = SystemTime::now() - Duration::from_secs(600);
        fs::OpenOptions::new()
            .write(true)
            .open(&agent)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // A 30s window excludes it; a 1h window includes it.
        assert_eq!(count_subagents(&subs, Duration::from_secs(30)), 0);
        assert_eq!(count_subagents(&subs, Duration::from_secs(3600)), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_subagents_dir_is_zero() {
        let dir = unique_dir("subs-missing");
        let n = count_subagents(&dir.join("nope"), Duration::from_secs(30));
        assert_eq!(n, 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// An agent transcript carrying a `message.usage` block (total = input +
    /// cache_read + cache_creation + output).
    fn write_agent_with_usage(
        path: &Path,
        sidechain: bool,
        input: u64,
        cache_read: u64,
        output: u64,
    ) {
        let line = format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"message":{{"id":"a","model":"claude-opus-4-8","usage":{{"input_tokens":{input},"cache_read_input_tokens":{cache_read},"output_tokens":{output}}}}}}}"#
        );
        fs::write(path, format!("{line}\n")).unwrap();
    }

    fn write_journal(path: &Path, lines: &[&str]) {
        fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
    }

    #[test]
    fn workflow_journal_counts_live_agents_ignoring_mtime() {
        let dir = unique_dir("subs-wf");
        let wf = dir.join("subagents").join("workflows").join("wf1");
        fs::create_dir_all(&wf).unwrap();
        write_agent_with_usage(&wf.join("agent-a.jsonl"), true, 100, 1000, 50); // 1150
        write_agent_with_usage(&wf.join("agent-b.jsonl"), true, 10, 200, 5); // 215
                                                                             // Journal: agent a started + resolved (done); agent b started only (live).
        write_journal(
            &wf.join("journal.jsonl"),
            &[
                r#"{"type":"started","key":"k-a","agentId":"a"}"#,
                r#"{"type":"started","key":"k-b","agentId":"b"}"#,
                r#"{"type":"result","key":"k-a","agentId":"a","result":{}}"#,
            ],
        );
        // Make every agent file stale (10 min ago): journal liveness must ignore
        // mtime, so b still counts even though nothing wrote recently.
        let old = SystemTime::now() - Duration::from_secs(600);
        for name in ["agent-a.jsonl", "agent-b.jsonl"] {
            fs::OpenOptions::new()
                .write(true)
                .open(wf.join(name))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }

        let scan = scan_subagents(&dir.join("subagents"), Duration::from_secs(30));
        assert_eq!(scan.count, 1, "only the unresolved agent (b) is live");
        assert_eq!(
            scan.tokens,
            Some(215),
            "tokens come from the live agent's latest usage"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn finished_workflow_contributes_zero_even_when_fresh() {
        let dir = unique_dir("subs-wf-done");
        let wf = dir.join("subagents").join("workflows").join("wf1");
        fs::create_dir_all(&wf).unwrap();
        // Fresh file, but the journal shows the agent resolved → 0 live.
        write_agent_with_usage(&wf.join("agent-a.jsonl"), true, 100, 1000, 50);
        write_journal(
            &wf.join("journal.jsonl"),
            &[
                r#"{"type":"started","key":"k-a","agentId":"a"}"#,
                r#"{"type":"result","key":"k-a","agentId":"a","result":{}}"#,
            ],
        );
        let scan = scan_subagents(&dir.join("subagents"), Duration::from_secs(3600));
        assert_eq!(scan.count, 0);
        assert_eq!(scan.tokens, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn stale_workflow_orphan_started_decays_to_zero() {
        // A failed/interrupted agent leaves a `started` with no terminal event.
        // Once the run is over (journal AND agent file both stale) such an orphan
        // must NOT be counted — otherwise orphaned `started` lines leak the "N×"
        // count forever across a long session.
        let dir = unique_dir("subs-wf-orphan");
        let wf = dir.join("subagents").join("workflows").join("wf1");
        fs::create_dir_all(&wf).unwrap();
        write_agent_with_usage(&wf.join("agent-x.jsonl"), true, 10, 200, 5); // 215
        write_journal(
            &wf.join("journal.jsonl"),
            &[r#"{"type":"started","key":"k-x","agentId":"x"}"#], // no terminal → orphan
        );
        let stale = SystemTime::now() - Duration::from_secs(600);
        for name in ["agent-x.jsonl", "journal.jsonl"] {
            fs::OpenOptions::new()
                .write(true)
                .open(wf.join(name))
                .unwrap()
                .set_modified(stale)
                .unwrap();
        }
        // Journal + agent both stale → run is over → the orphan is dead.
        let scan = scan_subagents(&dir.join("subagents"), Duration::from_secs(30));
        assert_eq!(
            scan.count, 0,
            "orphaned started from a finished run must not count"
        );
        assert_eq!(scan.tokens, None);

        // Agent transcript freshly written again → a genuinely long-running agent,
        // still live even though the journal is stale.
        fs::OpenOptions::new()
            .write(true)
            .open(wf.join("agent-x.jsonl"))
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        let scan = scan_subagents(&dir.join("subagents"), Duration::from_secs(30));
        assert_eq!(
            scan.count, 1,
            "a fresh agent transcript keeps a long-running orphan live"
        );
        assert_eq!(scan.tokens, Some(215));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flat_subagents_sum_tokens() {
        let dir = unique_dir("subs-tok");
        let subs = dir.join("subagents");
        fs::create_dir_all(&subs).unwrap();
        write_agent_with_usage(&subs.join("agent-a.jsonl"), true, 100, 1000, 50); // 1150
        write_agent_with_usage(&subs.join("agent-b.jsonl"), true, 10, 200, 5); // 215
        let scan = scan_subagents(&subs, Duration::from_secs(300));
        assert_eq!(scan.count, 2);
        assert_eq!(scan.tokens, Some(1365));
        fs::remove_dir_all(&dir).unwrap();
    }

    // ---- watch_session integration over a real temp file ---------------------

    #[test]
    fn watch_session_seeds_and_updates_snapshot() {
        let dir = unique_dir("watch");
        let path = dir.join("s1.jsonl");
        fs::write(
            &path,
            format!("{}\n", assistant_tool_use("m1", "t1", Some("tool_use"))),
        )
        .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(async {
            let watcher = watch_session(path.clone(), cfg()).expect("watcher starts");
            // Initial snapshot reflects the seeded file.
            let initial = watcher.current();
            assert_eq!(initial.model.as_deref(), Some("claude-opus-4-8"));
            assert!(initial.busy);

            // Append a tool_result → busy should clear. Poll the receiver with a
            // timeout so the test never hangs if FS events are slow.
            let mut rx = watcher.subscribe();
            {
                let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
                writeln!(f, "{}", user_tool_result("t1")).unwrap();
                f.flush().unwrap();
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut became_idle = false;
            while std::time::Instant::now() < deadline {
                if !rx.borrow().busy {
                    became_idle = true;
                    break;
                }
                let _ = tokio::time::timeout(Duration::from_millis(600), rx.changed()).await;
            }
            assert!(
                became_idle,
                "watcher should observe the tool_result and go idle"
            );
        });

        fs::remove_dir_all(&dir).unwrap();
    }
}
