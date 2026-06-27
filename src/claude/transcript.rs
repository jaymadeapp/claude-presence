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
    let parsed = parse_and_dedupe(lines);

    let mut state = DerivedState::default();
    let mut pending_tool_uses: HashSet<String> = HashSet::new();
    // Turn state: a genuine user prompt opens a turn (the model now owes output —
    // this is what keeps a long *thinking* pause active even when nothing is
    // written for minutes); a terminal assistant message closes it. A pending
    // tool_use or a streaming partial keeps it open.
    let mut in_turn = false;

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
        // Tolerantly parse through the adapter; a malformed/truncated line is
        // skipped, never fatal (FR-2/AC-1).
        let line = match parse_transcript_line(raw) {
            Ok(Some(line)) => line,
            Ok(None) => continue,
            Err(err) => {
                debug!(%err, "skipping malformed transcript line");
                continue;
            }
        };
        // The same string parses as raw JSON for tool_result correlation. If the
        // adapter accepted it, this cannot fail, but degrade to Null defensively.
        let value: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
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
        for agent_id in live_workflow_agent_ids(&journal) {
            *count += 1;
            if let Some(t) = last_request_tokens(&dir.join(format!("agent-{agent_id}.jsonl"))) {
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
/// Holds a byte offset and a carry buffer for a trailing partial line. Each
/// [`Tail::read_appended`] reads from the offset to EOF, returns every **complete**
/// (newline-terminated) line appended since the last call, and leaves any final
/// line without a trailing `\n` un-consumed so it is completed on the next read
/// (FR-2/AC-1: tolerate a truncated final JSON line).
#[derive(Debug, Default)]
pub struct Tail {
    /// Byte offset up to which complete lines have been consumed.
    offset: u64,
    /// Bytes of a trailing line read without a terminating newline yet.
    carry: String,
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
            carry: String::new(),
        }
    }

    /// Read and return the complete lines appended since the last call.
    ///
    /// A file shorter than the recorded offset (truncated/rotated) resets the
    /// tail to the start so nothing is missed. Returns an empty vec when there is
    /// nothing new. The trailing partial line (no `\n`) is buffered, not returned.
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
        let mut chunk = String::new();
        let read = file.take(len - self.offset).read_to_string(&mut chunk)?;
        self.offset += read as u64;

        // Prepend any buffered partial line from the previous read.
        let mut buf = std::mem::take(&mut self.carry);
        buf.push_str(&chunk);

        let mut lines = Vec::new();
        // Split, keeping a trailing newline-less remainder in `carry`.
        let ends_with_newline = buf.ends_with('\n');
        let mut iter = buf.split('\n').peekable();
        while let Some(segment) = iter.next() {
            let is_last = iter.peek().is_none();
            if is_last && !ends_with_newline {
                // Incomplete trailing line: buffer it for the next read.
                self.carry = segment.to_string();
                break;
            }
            if is_last && ends_with_newline {
                // The split yields a trailing "" after the final newline; drop it.
                break;
            }
            lines.push(segment.to_string());
        }
        Ok(lines)
    }
}

// ---------------------------------------------------------------------------
// notify-based watcher → watch channel (the aggregator-facing API)
// ---------------------------------------------------------------------------

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
/// append (FR-2). The whole file is re-derived each event from the lines seen so
/// far — transcripts are small relative to FS-event cadence and a full re-derive
/// is the simplest correct way to honour the partial-dedupe and busy/idle
/// semantics without carrying cross-event parser state.
///
/// `transcript` is the `<sessionId>.jsonl` path; its sibling
/// `<dir>/<sessionId>/subagents` directory (if present) is counted for
/// FR-2/AC-6. The returned [`SessionWatcher`] owns the `notify` watcher and a
/// small event-loop thread; drop it to stop watching.
pub fn watch_session(transcript: PathBuf, cfg: Config) -> notify::Result<SessionWatcher> {
    let recency = Duration::from_secs(cfg.subagent_recency_secs);
    let subagents_dir = subagents_dir_for(&transcript);

    // Seed the initial snapshot from the whole existing file so a session that
    // started before us is immediately reflected.
    let mut state = derive_state(&read_all_lines(&transcript), &cfg);
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

            // Re-read the whole file and re-derive. (Append-only + small files.)
            let mut next = derive_state(&read_all_lines(&transcript), &cfg);
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
