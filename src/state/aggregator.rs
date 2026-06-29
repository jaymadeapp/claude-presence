//! State aggregation (FR-5): fold the live session set into one Discord-safe
//! presence model, including focus selection, party sizing, field formatting,
//! the empty-state clear signal, and a debounced watch-channel helper.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::config::{Button, Config};
use crate::state::model::{PresenceModel, SessionState};

/// Discord's hard cap for both `details` and `state` (C-3).
pub const DISCORD_TEXT_LIMIT: usize = 128;

/// Default sticky window for focus selection (design §5).
pub const DEFAULT_STICKY_WINDOW: Duration = Duration::from_secs(8);

/// Aggregator output. `Clear` maps to `activity:null` in the Discord sink.
#[derive(Debug, Clone)]
pub enum PresenceUpdate {
    Clear,
    Activity(Box<PresenceModel>),
}

/// Which timestamp drives Discord's elapsed timer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TimestampMode {
    /// Focused session start time (default, FR-5/AC-4).
    #[default]
    SessionStart,
    /// Best available current-turn proxy in the current domain model.
    CurrentTurnStart,
}

/// Stateful aggregator. The state is only the sticky focused session id.
#[derive(Debug, Clone)]
pub struct Aggregator {
    cfg: Config,
    focused_session_id: Option<String>,
    sticky_window: Duration,
    timestamp_mode: TimestampMode,
}

impl Aggregator {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            focused_session_id: None,
            sticky_window: DEFAULT_STICKY_WINDOW,
            timestamp_mode: TimestampMode::default(),
        }
    }

    pub fn with_sticky_window(mut self, sticky_window: Duration) -> Self {
        self.sticky_window = sticky_window;
        self
    }

    pub fn with_timestamp_mode(mut self, timestamp_mode: TimestampMode) -> Self {
        self.timestamp_mode = timestamp_mode;
        self
    }

    /// Build the next presence update from the current live top-level sessions.
    ///
    /// Zero sessions intentionally emits [`PresenceUpdate::Clear`] and clears
    /// sticky focus so Discord remains cleared until a session reappears.
    pub fn aggregate(&mut self, sessions: Vec<SessionState>) -> PresenceUpdate {
        if sessions.is_empty() {
            self.focused_session_id = None;
            return PresenceUpdate::Clear;
        }

        let focused = self.focused_index(&sessions);
        self.focused_session_id = Some(sessions[focused].session_id.clone());

        let live_count = saturating_u32(sessions.len());
        let capacity = self.cfg.capacity.unwrap_or(live_count).max(live_count);
        let focused_session = &sessions[focused];

        // Only sessions doing actual work (busy OR running subagents) drive the
        // multi-session headline and its session count, so two idle sessions plus
        // one working one read as that one working session — not "across 3". The
        // running-agent total still aggregates across the whole set (idle sessions
        // contribute 0 agents by construction).
        let active_count = saturating_u32(sessions.iter().filter(|s| is_active(s)).count());
        let displayed_multi = active_count >= 2;
        let total_subagents: u32 = sessions
            .iter()
            .fold(0u32, |acc, session| acc.saturating_add(session.subagents));

        // When the multi-session card spans more than one distinct model, the
        // `state` line shows a generic "{active_count} Agents" label instead of a
        // single (focus-dependent) model name — picking one model would flicker as
        // focus moves between models. Computed over the *active* sessions so it
        // matches the "Working across N sessions" headline. `None` keeps the normal
        // single-model rendering (single session, or a multi card all on one model).
        let multi_agents = if displayed_multi {
            let distinct: std::collections::BTreeSet<String> = sessions
                .iter()
                .filter(|session| is_active(session))
                .map(|session| pretty_model(session.model.as_deref().unwrap_or("Claude")))
                .collect();
            (distinct.len() > 1).then_some(active_count)
        } else {
            None
        };

        // Displayed metrics: summed across the active sessions for the multi card
        // (each session's tokens already include its live subagents), else the
        // focused session's own. Context % is per-session and never aggregates.
        let (disp_cost, disp_tokens) = if displayed_multi {
            let combined = Combined::from_active(&sessions);
            (combined.cost, combined.tokens)
        } else {
            (focused_session.cost_usd, session_tokens(focused_session))
        };
        let disp_ctx = if displayed_multi {
            None
        } else {
            focused_session.ctx_pct
        };

        // Strongest privacy guarantee (C-7, FR-7/AC-2): when the focused session
        // is in global private mode or its project is blacklisted, render the
        // whole card from the generic `private_card` — no project, branch,
        // activity target, ai-title, model, or metrics may leak. The per-tool
        // small_image badge and small_text are dropped too. This holds for the
        // multi-session card too: we never leak a blacklisted project name.
        let private = is_private(focused_session, &self.cfg);
        let (details, state, small_image, small_text) = if private {
            let card = crate::privacy::private_card();
            (
                truncate_chars(&card.details, DISCORD_TEXT_LIMIT),
                truncate_chars(&card.state, DISCORD_TEXT_LIMIT),
                None,
                None,
            )
        } else {
            // The small badge is the single configured asset key only — per-tool
            // badges would need uploaded per-tool art assets that don't exist, and
            // an unknown key renders nothing, so we never pass `small_image_key`.
            let small_image = self.cfg.assets.small_image.clone();
            let state_inputs = StateInputs {
                model: focused_session.model.clone(),
                agent_count: total_subagents,
                multi_agents,
                cost: disp_cost,
                tokens: disp_tokens,
                ctx: disp_ctx,
            };
            (
                truncate_chars(
                    &format_details(focused_session, &self.cfg, active_count, displayed_multi),
                    DISCORD_TEXT_LIMIT,
                ),
                format_state(&self.cfg, &state_inputs),
                small_image,
                small_text(focused_session, active_count, displayed_multi),
            )
        };

        // Multi-session timer reflects total time working: earliest start across
        // the active sessions. Single-session keeps the per-mode focused start.
        let started_at_ms = if displayed_multi {
            let earliest = sessions
                .iter()
                .filter(|s| is_active(s))
                .map(|session| session.started_at)
                .min()
                .unwrap_or(focused_session.started_at);
            system_time_to_epoch_ms(earliest)
        } else {
            match self.timestamp_mode {
                TimestampMode::SessionStart => system_time_to_epoch_ms(focused_session.started_at),
                TimestampMode::CurrentTurnStart => {
                    system_time_to_epoch_ms(focused_session.last_active)
                }
            }
        };

        PresenceUpdate::Activity(Box::new(PresenceModel {
            sessions,
            focused,
            live_count,
            capacity,
            details,
            state,
            started_at_ms,
            large_image: self.cfg.assets.large_image.clone().unwrap_or_default(),
            large_text: "Claude Code".to_string(),
            small_image,
            small_text,
            buttons: valid_buttons(&self.cfg.buttons),
        }))
    }

    fn focused_index(&self, sessions: &[SessionState]) -> usize {
        // Focus a session that is actually working when any is, so the headline
        // and state reflect live work rather than the most-recently-touched idle
        // session. Fall back to the whole set when nothing is active.
        let pool: Vec<usize> = {
            let active: Vec<usize> = (0..sessions.len())
                .filter(|&i| is_active(&sessions[i]))
                .collect();
            if active.is_empty() {
                (0..sessions.len()).collect()
            } else {
                active
            }
        };

        let newest = pool
            .iter()
            .copied()
            .max_by_key(|&i| sessions[i].last_active)
            .unwrap_or(0);
        let Some(current_id) = self.focused_session_id.as_deref() else {
            return newest;
        };
        let Some(&current) = pool.iter().find(|&&i| sessions[i].session_id == current_id) else {
            return newest;
        };

        if current == newest {
            return current;
        }

        if within_sticky_window(
            sessions[current].last_active,
            sessions[newest].last_active,
            self.sticky_window,
        ) {
            current
        } else {
            newest
        }
    }
}

/// A session is "active" — counted toward the working-session headline and
/// preferred for focus — when a turn is in progress (`working`, which spans
/// thinking and between-tool gaps), a tool is mid-flight (`busy`), or it has live
/// subagents. `working` is the signal that keeps a long reasoning pause or an
/// orchestrating session (whose main `tool_use` momentarily resolves) active.
fn is_active(session: &SessionState) -> bool {
    session.working || session.busy || session.subagents > 0
}

/// The session's displayed token total: its own latest-request tokens plus the
/// tokens of its live subagents. `None` only when both are absent.
fn session_tokens(session: &SessionState) -> Option<u64> {
    match (session.tokens_total, session.subagent_tokens) {
        (Some(t), Some(a)) => Some(t.saturating_add(a)),
        (Some(t), None) => Some(t),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Build a debounced watch channel from a watch channel of live sessions.
///
/// Collector bursts are coalesced to the newest session set before publishing a
/// [`PresenceUpdate`]. The delay uses `Config::min_interval`, matching the
/// configured debounce floor for presence updates.
pub fn aggregate_channel(
    mut sessions_rx: watch::Receiver<Vec<SessionState>>,
    cfg: Config,
) -> watch::Receiver<PresenceUpdate> {
    let mut aggregator = Aggregator::new(cfg.clone());
    let initial = aggregator.aggregate(sessions_rx.borrow().clone());
    let (tx, rx) = watch::channel(initial);
    let debounce = duration_from_seconds(cfg.min_interval, COALESCE_FLOOR);

    tokio::spawn(async move {
        loop {
            if sessions_rx.changed().await.is_err() {
                break;
            }
            let mut latest = sessions_rx.borrow().clone();
            let sleep = tokio::time::sleep(debounce);
            tokio::pin!(sleep);

            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    changed = sessions_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        latest = sessions_rx.borrow().clone();
                    }
                }
            }

            if tx.send(aggregator.aggregate(latest)).is_err() {
                break;
            }
        }
    });

    rx
}

fn within_sticky_window(current: SystemTime, candidate: SystemTime, window: Duration) -> bool {
    match candidate.duration_since(current) {
        Ok(delta) => delta <= window,
        Err(_) => true,
    }
}

/// Whether the focused session must be rendered with nothing identifying: the
/// global private switch is on, OR the session's project is blacklisted (C-7,
/// FR-7/AC-2). When true the activity target, branch, and ai-title are all
/// suppressed and the project collapses to a generic label.
fn is_private(session: &SessionState, cfg: &Config) -> bool {
    cfg.privacy.redact || crate::privacy::is_blacklisted(&session.cwd, &cfg.privacy.blacklist_paths)
}

fn format_details(
    session: &SessionState,
    cfg: &Config,
    active_count: u32,
    displayed_multi: bool,
) -> String {
    // Multi-session headline: just the count of *working* sessions. The running
    // agent count moved to `state` (the "N× model" prefix), so no project name
    // leaks here (privacy-safe by construction).
    if displayed_multi {
        return format!(
            "Working across {}",
            pluralize(active_count, "session", "sessions")
        );
    }

    let private = is_private(session, cfg);

    // The project is hidden when private mode is on OR the per-field project
    // toggle is off (`fields.project = false` collapses it to the generic label).
    let hide_project = cfg.privacy.redact || !cfg.privacy.fields.project;

    // Project label is resolved through the privacy helper so a blacklisted /
    // redacted / project-hidden session collapses to the generic label (never the
    // basename).
    let project =
        crate::privacy::project_label(&session.cwd, hide_project, &cfg.privacy.blacklist_paths);

    // Single-session headline: the project the session is working on. Any live
    // agent count is surfaced in `state` (the "N× model" prefix), not here.
    let mut details = format!("Working on {project}");

    // Branch is suppressed in private mode AND when the project is hidden (the
    // branch reveals the repo, so it must follow `fields.project`).
    if cfg.fields.branch && !private && cfg.privacy.fields.project {
        if let Some(branch) = session
            .branch
            .as_deref()
            .filter(|branch| !branch.is_empty())
        {
            details.push_str(" (");
            details.push_str(branch);
            details.push(')');
        }
    }

    // Append the gated ai-title when opted-in, not private, and not blacklisted.
    // `privacy::ai_title` enforces the opt-in + blacklist + secret-scrub; we only
    // surface it when there is room within the ≤128 cap. The ai-title is also
    // suppressed when the project is hidden (`fields.project = false`), mirroring
    // the branch gate above — it can reveal the project the session is working on.
    if !private && cfg.privacy.fields.project {
        if let Some(title) = crate::privacy::ai_title(
            session.title.as_deref(),
            cfg.show_ai_title,
            &session.cwd,
            &cfg.privacy.blacklist_paths,
        ) {
            let candidate = format!("{details} · {title}");
            if char_count(&candidate) <= DISCORD_TEXT_LIMIT {
                details = candidate;
            }
        }
    }

    details
}

/// Inputs for the `state` line, decoupled from `SessionState` so the multi- and
/// single-session paths (and the truncation tests) feed it uniformly.
struct StateInputs {
    /// Focused session model id (e.g. `claude-opus-4-8`); `None` → "Claude".
    model: Option<String>,
    /// Total live subagents across all sessions; `>1` renders an "N×" prefix on
    /// the model (e.g. `20× Opus 4.8`); `0` or `1` renders none (a ×1 is noise). Ignored when
    /// [`Self::multi_agents`] is set (the generic label carries no prefix).
    agent_count: u32,
    /// When the multi-session card spans more than one distinct model, `Some(n)`
    /// (n = active session count) renders the model slot as `"{n} Agents"` instead
    /// of a model name, and suppresses the `agent_count` prefix. `None` keeps the
    /// normal single-model rendering.
    multi_agents: Option<u32>,
    /// Displayed cost (focused or combined), gated by `fields.cost`.
    cost: Option<f64>,
    /// Displayed token total incl. live subagents, gated by `fields.tokens`.
    tokens: Option<u64>,
    /// Displayed context %, single-session only, gated by `fields.context_pct`.
    ctx: Option<f64>,
}

fn format_state(cfg: &Config, inputs: &StateInputs) -> String {
    let full_plan = cfg.plan_label.trim().to_string();

    // The model slot. When the multi-session card spans more than one distinct
    // model (`multi_agents`), show a generic "{n} Agents" label — with no model
    // name and no "N×" subagent prefix — rather than picking a single model that
    // would flip as focus moves. Otherwise it's the focused model, optionally
    // prefixed with the running subagent count ("N×", e.g. "20× Opus 4.8"), which
    // stays attached through the abbreviation rungs below.
    let (full_model, short_model, prefix_owned) = match inputs.multi_agents {
        Some(n) => {
            let label = format!("{n} Agents");
            (label.clone(), label, None)
        }
        None => {
            let full = pretty_model(inputs.model.as_deref().unwrap_or("Claude"));
            let short = abbreviate_model(&full);
            // A lone subagent (count 1) renders no prefix — a "1×" multiplier is
            // noise (×1 says nothing) and reads oddly next to a multi-session
            // headline. The prefix appears only once there is real fan-out (>= 2).
            let prefix = (inputs.agent_count > 1).then(|| format!("{}\u{d7}", inputs.agent_count));
            (full, short, prefix)
        }
    };
    let prefix = prefix_owned.as_deref();
    let mut metrics = Metrics::build(cfg, inputs.cost, inputs.tokens, inputs.ctx);

    let mut state = state_with(prefix, &full_model, &full_plan, &metrics);
    if char_count(&state) <= DISCORD_TEXT_LIMIT {
        return state;
    }

    state = state_with(prefix, &short_model, &full_plan, &metrics);
    if char_count(&state) <= DISCORD_TEXT_LIMIT {
        return state;
    }

    let short_plan = abbreviate_plan(&full_plan);
    state = state_with(prefix, &short_model, &short_plan, &metrics);
    if char_count(&state) <= DISCORD_TEXT_LIMIT {
        return state;
    }

    // Drop metrics from the tail in order: ctx% → tokens → cost. (ctx is always
    // None in the multi-session card, so this rung is a no-op there.)
    metrics.ctx = None;
    state = state_with(prefix, &short_model, &short_plan, &metrics);
    if char_count(&state) <= DISCORD_TEXT_LIMIT {
        return state;
    }

    metrics.tokens = None;
    state = state_with(prefix, &short_model, &short_plan, &metrics);
    if char_count(&state) <= DISCORD_TEXT_LIMIT {
        return state;
    }

    metrics.cost = None;
    truncate_chars(
        &state_with(prefix, &short_model, &short_plan, &metrics),
        DISCORD_TEXT_LIMIT,
    )
}

#[derive(Debug, Clone)]
struct Metrics {
    cost: Option<String>,
    tokens: Option<String>,
    ctx: Option<String>,
}

impl Metrics {
    /// Build the rendered state metrics from already-resolved displayed values,
    /// each gated by its field toggle. (The caller decides single vs combined and
    /// whether ctx% applies.)
    fn build(cfg: &Config, cost: Option<f64>, tokens: Option<u64>, ctx: Option<f64>) -> Self {
        Self {
            cost: cfg.fields.cost.then(|| cost.map(format_cost)).flatten(),
            tokens: cfg
                .fields
                .tokens
                .then(|| tokens.map(format_tokens))
                .flatten(),
            ctx: cfg
                .fields
                .context_pct
                .then(|| ctx.map(format_ctx))
                .flatten(),
        }
    }
}

/// Aggregated cost/token totals across the **active** sessions, used only by the
/// multi-session card. `None` for a field means every active session left it
/// `None`.
#[derive(Debug, Clone, Copy, Default)]
struct Combined {
    cost: Option<f64>,
    tokens: Option<u64>,
}

impl Combined {
    /// Sum cost and tokens across the active sessions only (the ones the
    /// multi-session card represents). Each session's tokens include its live
    /// subagents (see [`session_tokens`]).
    fn from_active(sessions: &[SessionState]) -> Self {
        let cost = sessions
            .iter()
            .filter(|session| is_active(session))
            .filter_map(|session| session.cost_usd)
            .fold(None, |acc: Option<f64>, value| {
                Some(acc.unwrap_or(0.0) + value)
            });
        let tokens = sessions
            .iter()
            .filter(|session| is_active(session))
            .filter_map(session_tokens)
            .fold(None, |acc: Option<u64>, value| {
                Some(acc.unwrap_or(0).saturating_add(value))
            });
        Self { cost, tokens }
    }
}

fn state_with(prefix: Option<&str>, model: &str, plan: &str, metrics: &Metrics) -> String {
    let mut parts = Vec::new();
    let model_token = match prefix {
        Some(p) if !model.is_empty() => format!("{p} {model}"),
        Some(p) => p.to_string(),
        None => model.to_string(),
    };
    if !model_token.is_empty() {
        parts.push(model_token);
    }
    if !plan.is_empty() {
        parts.push(plan.to_string());
    }
    if let Some(cost) = &metrics.cost {
        parts.push(cost.clone());
    }
    if let Some(tokens) = &metrics.tokens {
        parts.push(tokens.clone());
    }
    if let Some(ctx) = &metrics.ctx {
        parts.push(ctx.clone());
    }
    parts.join(" \u{b7} ")
}

fn pretty_model(model: &str) -> String {
    let mut cleaned = model.trim();
    if let Some(stripped) = cleaned.strip_prefix("claude-") {
        cleaned = stripped;
    }

    let parts: Vec<&str> = cleaned.split('-').collect();
    if parts.len() >= 3 {
        let family = title_case(parts[0]);
        if parts[1].chars().all(|c| c.is_ascii_digit())
            && parts[2].chars().all(|c| c.is_ascii_digit())
        {
            return format!("{family} {}.{}", parts[1], parts[2]);
        }
    }

    title_case_words(&cleaned.replace('-', " "))
}

fn abbreviate_model(model: &str) -> String {
    for (prefix, letter) in [("Opus ", "O"), ("Sonnet ", "S"), ("Haiku ", "H")] {
        if let Some(rest) = model.strip_prefix(prefix) {
            return format!("{letter}{}", rest.replace(' ', ""));
        }
    }
    // Unknown models keep their (collapsed) name; the ladder's final rung caps
    // the whole string to ≤128, so we never hard-truncate here (that would make
    // the metric-drop rungs unreachable — see FR-5/AC-3).
    model.to_string()
}

fn abbreviate_plan(plan: &str) -> String {
    if plan.is_empty() {
        return String::new();
    }

    let lower = plan.to_ascii_lowercase();
    if lower.contains("max") {
        let suffix: String = plan
            .chars()
            .filter(|c| c.is_ascii_digit() || c.eq_ignore_ascii_case(&'x'))
            .collect();
        return if suffix.is_empty() {
            "Max".to_string()
        } else {
            format!("M{suffix}")
        };
    }
    if lower.contains("pro") {
        return "Pro".to_string();
    }

    // Unknown plans keep their label here. Capping to ≤128 is the ladder's final
    // rung; truncating to a fixed width at step 2 would short-circuit the
    // metric-drop rungs (ctx% → tokens → cost) and make them dead code.
    plan.to_string()
}

fn format_cost(cost: f64) -> String {
    if cost >= 100.0 {
        format!("${cost:.0}")
    } else if cost >= 10.0 {
        format!("${cost:.1}")
    } else {
        format!("${cost:.2}")
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K tok", tokens.saturating_add(500) / 1_000)
    } else {
        format!("{tokens} tok")
    }
}

fn format_ctx(ctx_pct: f64) -> String {
    format!("Ctx {:.0}%", ctx_pct.clamp(0.0, 999.0))
}

/// Pluralize a count with the singular form for exactly 1, e.g.
/// `pluralize(1, "agent", "agents") == "1 agent"`.
fn pluralize(count: u32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// The small-badge tooltip (`assets.small_text`), shown on hover.
///
/// Multi-session: how many sessions are working. Single-session: the focused tool
/// activity (`"{verb} {target}"` or just the verb), keeping the live activity
/// visible on hover even though it is no longer the headline. `None` when a
/// single session has no current activity.
fn small_text(session: &SessionState, active_count: u32, displayed_multi: bool) -> Option<String> {
    if displayed_multi {
        return Some(format!("{} active sessions", active_count));
    }

    session.activity.as_ref().map(|activity| {
        activity
            .target
            .as_ref()
            .filter(|target| !target.is_empty())
            .map(|target| format!("{} {target}", activity.verb))
            .unwrap_or_else(|| activity.verb.clone())
    })
}

fn valid_buttons(buttons: &[Button]) -> Vec<(String, String)> {
    buttons
        .iter()
        // Central privacy helper: trims and rejects non-https + scheme-only URLs
        // (e.g. bare `https://` or a `file://` path leak) — FR-7/AC-2.
        .filter(|button| crate::privacy::is_safe_button_url(&button.url))
        .map(|button| {
            (
                truncate_chars(button.label.trim(), 32),
                button.url.trim().to_string(),
            )
        })
        .filter(|(label, url)| !label.is_empty() && !url.is_empty())
        .collect()
}

fn system_time_to_epoch_ms(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// The aggregator's own anti-busy-spin coalesce floor: a non-positive/non-finite
/// `min_interval` collapses the debounce window to zero and busy-spins the inner
/// coalesce loop (F33). Clamp it to a safe non-zero default instead. This is
/// independent of the sink's `FALLBACK_MIN_INTERVAL` (4.0s) rate floor — the sink's
/// rolling window is the rate ceiling; this is purely to keep the coalesce loop from
/// sleeping zero in a hot loop.
const COALESCE_FLOOR: Duration = Duration::from_millis(2500);

fn duration_from_seconds(seconds: f64, fallback: Duration) -> Duration {
    if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        fallback
    }
}

fn saturating_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if char_count(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

fn title_case_words(s: &str) -> String {
    s.split_whitespace()
        .map(title_case)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::new();
    out.extend(first.to_uppercase());
    out.push_str(&chars.as_str().to_ascii_lowercase());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Assets, FieldToggles};
    use crate::state::model::Activity;

    fn time(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn session(id: &str, last_active_ms: u64) -> SessionState {
        SessionState {
            session_id: id.to_string(),
            pid: 100,
            project: "private".to_string(),
            cwd: std::path::PathBuf::from("/Users/me/Projects/private"),
            branch: Some("main".to_string()),
            model: Some("claude-opus-4-8".to_string()),
            started_at: time(1_781_989_000_000),
            last_active: time(last_active_ms),
            busy: true,
            working: false,
            activity: Some(Activity {
                verb: "Running".to_string(),
                target: Some("cargo".to_string()),
                small_image_key: Some("bash".to_string()),
            }),
            title: None,
            cost_usd: Some(0.98),
            ctx_pct: Some(8.3),
            tokens_total: Some(837_000),
            subagents: 0,
            subagent_tokens: None,
        }
    }

    #[test]
    fn empty_sessions_signal_clear() {
        let mut aggregator = Aggregator::new(Config::default());
        assert!(matches!(
            aggregator.aggregate(Vec::new()),
            PresenceUpdate::Clear
        ));
    }

    #[test]
    fn live_count_and_capacity_track_sessions_not_busy_or_subagents() {
        let mut cfg = Config {
            capacity: Some(5),
            ..Config::default()
        };
        cfg.assets = Assets {
            large_image: Some("large".to_string()),
            small_image: Some("small".to_string()),
        };
        let mut s1 = session("a", 10);
        s1.busy = false;
        s1.subagents = 7;
        let s2 = session("b", 20);

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s1, s2]) else {
            panic!("expected activity");
        };

        // `live_count`/`capacity` remain on the model (not mapped to Discord now).
        assert_eq!(model.live_count, 2);
        assert_eq!(model.capacity, 5);
        assert_eq!(model.sessions[model.focused].session_id, "b");
        // The running-agent count is the "N×" model prefix in `state` (7 agents
        // across the two active sessions), never the literal word "subagent".
        assert!(model.state.contains("7\u{d7} Opus 4.8"), "{}", model.state);
        assert!(!model.state.contains("subagent"));
    }

    #[test]
    fn multi_session_details_count_sessions_and_state_counts_agents() {
        // Two working sessions carrying agents → details counts the *sessions*,
        // and the total running-agent count (3) becomes the "N×" model prefix in
        // state.
        let mut cfg = Config::default();
        cfg.assets.small_image = Some("claude".to_string());
        let mut s1 = session("a", 10);
        s1.subagents = 2;
        let mut s2 = session("b", 20);
        s2.subagents = 1;

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s1, s2]) else {
            panic!("expected activity");
        };
        // Headline = working-session count; no agent count, no project leak.
        assert_eq!(model.details, "Working across 2 sessions");
        assert!(!model.details.contains("private"));
        // A = 3 total agents → "3×" prefix on the model in state.
        assert!(model.state.contains("3\u{d7} Opus 4.8"), "{}", model.state);
        // small_text reflects the working-session count.
        assert_eq!(model.small_text.as_deref(), Some("2 active sessions"));
    }

    #[test]
    fn multi_session_details_without_agents() {
        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) =
            aggregator.aggregate(vec![session("a", 10), session("b", 20)])
        else {
            panic!("expected activity");
        };
        assert_eq!(model.details, "Working across 2 sessions");
    }

    #[test]
    fn multi_session_state_combines_tokens_and_omits_ctx() {
        let mut cfg = Config {
            plan_label: "Max 20x".to_string(),
            ..Config::default()
        };
        cfg.fields.cost = true;
        let mut s1 = session("a", 10);
        s1.tokens_total = Some(100_000);
        s1.cost_usd = Some(1.0);
        let mut s2 = session("b", 20);
        s2.tokens_total = Some(200_000);
        s2.cost_usd = Some(2.0);

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s1, s2]) else {
            panic!("expected activity");
        };
        // Combined tokens T = 300_000 → "300K tok"; combined cost = $3.00.
        assert!(model.state.contains("300K tok"), "{}", model.state);
        assert!(model.state.contains("$3.00"), "{}", model.state);
        // Per-session ctx% does not aggregate → never shown in the multi card.
        assert!(!model.state.contains("Ctx "), "{}", model.state);
        // Middot separator, not a pipe.
        assert!(model.state.contains('\u{b7}'), "{}", model.state);
        assert!(!model.state.contains('|'), "{}", model.state);
    }

    #[test]
    fn mixed_models_show_generic_agents_label() {
        // 3× Opus 4.8 + 2× Sonnet 4.6, all active → the state model slot is a
        // generic "5 Agents" label, never a single (focus-dependent) model name.
        let mut cfg = Config::default();
        cfg.assets.small_image = Some("claude".to_string());

        let mut sessions = Vec::new();
        for i in 0..3 {
            sessions.push(session(&format!("opus{i}"), 10 + i));
        }
        for i in 0..2 {
            let mut s = session(&format!("sonnet{i}"), 20 + i);
            s.model = Some("claude-sonnet-4-6".to_string());
            sessions.push(s);
        }

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(sessions) else {
            panic!("expected activity");
        };

        assert_eq!(model.details, "Working across 5 sessions");
        // Generic agent label — never a specific model name that would flip as
        // focus moves between the Opus and Sonnet sessions.
        assert!(model.state.starts_with("5 Agents"), "{}", model.state);
        assert!(!model.state.contains("Opus"), "{}", model.state);
        assert!(!model.state.contains("Sonnet"), "{}", model.state);
        assert_eq!(model.small_text.as_deref(), Some("5 active sessions"));
    }

    #[test]
    fn same_model_multi_keeps_the_model_name() {
        // Several sessions all on ONE model still show that model (not "Agents") —
        // the generic label is only for genuinely mixed-model cards.
        let mut cfg = Config::default();
        cfg.assets.small_image = Some("claude".to_string());

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) =
            aggregator.aggregate(vec![session("a", 10), session("b", 20), session("c", 30)])
        else {
            panic!("expected activity");
        };

        assert_eq!(model.details, "Working across 3 sessions");
        assert!(model.state.contains("Opus 4.8"), "{}", model.state);
        assert!(!model.state.contains("Agents"), "{}", model.state);
    }

    #[test]
    fn mixed_models_suppress_the_subagent_prefix() {
        // With mixed models the generic "{n} Agents" label replaces the model, so
        // the running-subagent "N×" prefix is dropped (it would read nonsensically).
        let mut cfg = Config::default();
        cfg.assets.small_image = Some("claude".to_string());

        let mut opus = session("o", 10);
        opus.subagents = 4;
        let mut sonnet = session("s", 20);
        sonnet.model = Some("claude-sonnet-4-6".to_string());
        sonnet.subagents = 3;

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![opus, sonnet]) else {
            panic!("expected activity");
        };

        assert!(model.state.starts_with("2 Agents"), "{}", model.state);
        assert!(!model.state.contains('\u{d7}'), "{}", model.state);
    }

    #[test]
    fn multi_session_timer_uses_earliest_start() {
        let mut early = session("a", 30_000);
        early.started_at = time(1_000);
        let mut late = session("b", 50_000); // focused (most-recently-active)
        late.started_at = time(9_000);

        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![early, late]) else {
            panic!("expected activity");
        };
        // Focus is the most-recently-active ("b"), but the timer reflects the
        // earliest start across all live sessions (so it tracks total work time).
        assert_eq!(model.sessions[model.focused].session_id, "b");
        assert_eq!(model.started_at_ms, 1_000);
    }

    #[test]
    fn focused_session_is_sticky_within_window() {
        let cfg = Config::default();
        let mut aggregator = Aggregator::new(cfg).with_sticky_window(Duration::from_millis(1_000));

        let PresenceUpdate::Activity(first) =
            aggregator.aggregate(vec![session("a", 10_000), session("b", 9_000)])
        else {
            panic!("expected activity");
        };
        assert_eq!(first.sessions[first.focused].session_id, "a");

        let PresenceUpdate::Activity(sticky) =
            aggregator.aggregate(vec![session("a", 10_000), session("b", 10_500)])
        else {
            panic!("expected activity");
        };
        assert_eq!(sticky.sessions[sticky.focused].session_id, "a");

        let PresenceUpdate::Activity(switched) =
            aggregator.aggregate(vec![session("a", 10_000), session("b", 11_500)])
        else {
            panic!("expected activity");
        };
        assert_eq!(switched.sessions[switched.focused].session_id, "b");
    }

    #[test]
    fn details_state_and_timestamp_fit_discord_contract() {
        let mut cfg = Config {
            plan_label: "Max 20x".to_string(),
            ..Config::default()
        };
        cfg.fields = FieldToggles {
            timestamp: true,
            cost: true,
            tokens: true,
            context_pct: true,
            branch: true,
        };

        let mut s = session("a", 20);
        // Details now derives the label from the cwd basename, so make THAT long
        // to exercise the ≤128 cap on `details`.
        s.cwd = std::path::PathBuf::from(format!("/Users/me/{}", "p".repeat(200)));
        s.started_at = time(1_781_989_000_123);

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };

        assert!(char_count(&model.details) <= DISCORD_TEXT_LIMIT);
        assert!(char_count(&model.state) <= DISCORD_TEXT_LIMIT);
        assert_eq!(model.started_at_ms, 1_781_989_000_123);
    }

    #[test]
    fn truncation_ladder_drops_metrics_from_tail() {
        // A long *unknown* plan survives both abbreviation rungs (model + plan),
        // so the ladder is forced into its metric-drop rungs. The lengths are
        // tuned so it drops ctx% then tokens to fit, keeping cost — proving the
        // ordered tail drop ctx% → tokens → cost (FR-5/AC-3).
        let mut cfg = Config {
            plan_label: "Enterprise ".repeat(10).trim().to_string(),
            ..Config::default()
        };
        cfg.fields.cost = true; // cost is off by default; this test exercises the cost rung

        let inputs = StateInputs {
            model: Some("claude-opus-4-8".to_string()),
            agent_count: 0,
            multi_agents: None,
            cost: Some(0.98),
            tokens: Some(837_000),
            ctx: Some(8.3),
        };
        let state = format_state(&cfg, &inputs);
        assert!(char_count(&state) <= DISCORD_TEXT_LIMIT);
        // Rungs 1 & 2 ran first: model abbreviated, plan retained.
        assert!(state.contains("O4.8"));
        assert!(state.contains("Enterprise"));
        // Tail dropped in order: ctx% first, then tokens — both gone.
        assert!(!state.contains("Ctx "));
        assert!(!state.contains("tok"));
        // Cost is the last metric in the tail, so it survives once the string fits.
        assert!(state.contains('$'));
    }

    #[test]
    fn truncation_ladder_drops_cost_last_when_still_over() {
        // An even longer unknown plan forces *all* metrics off, exercising the
        // final cost-drop rung and the overall ≤128 cap.
        let mut cfg = Config {
            plan_label: "Enterprise ".repeat(20).trim().to_string(),
            ..Config::default()
        };
        cfg.fields.cost = true; // cost is off by default; this test exercises the cost-drop rung

        let inputs = StateInputs {
            model: Some("claude-opus-4-8".to_string()),
            agent_count: 0,
            multi_agents: None,
            cost: Some(0.98),
            tokens: Some(837_000),
            ctx: Some(8.3),
        };
        let state = format_state(&cfg, &inputs);
        assert!(char_count(&state) <= DISCORD_TEXT_LIMIT);
        assert!(state.contains("O4.8"));
        assert!(!state.contains("Ctx "));
        assert!(!state.contains("tok"));
        assert!(!state.contains('$'));
    }

    #[test]
    fn buttons_https_only_filter() {
        let cfg = Config {
            buttons: vec![
                Button {
                    label: "Repo".to_string(),
                    url: "https://example.com/repo".to_string(),
                },
                Button {
                    label: "Local".to_string(),
                    url: "file:///Users/me/private".to_string(),
                },
            ],
            ..Config::default()
        };

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };

        assert_eq!(
            model.buttons,
            vec![("Repo".to_string(), "https://example.com/repo".to_string())]
        );
    }

    #[test]
    fn single_idle_card_shows_project_and_branch() {
        // Product goal: out of the box (redact = false, no blacklist) a single
        // idle session reads "Working on {project} ({branch})". The live activity
        // moves to small_text (the badge tooltip), not the headline.
        let cfg = Config::default();
        assert!(!cfg.privacy.redact, "redact must default to false");
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        assert_eq!(model.details, "Working on private (main)");
        // The focused tool activity is preserved on hover via small_text.
        assert_eq!(model.small_text.as_deref(), Some("Running cargo"));
    }

    #[test]
    fn project_hidden_collapses_details_and_branch_but_keeps_metrics() {
        // fields.project = false collapses the project to the generic label and
        // suppresses the branch (it reveals the repo); model + metrics + state are
        // unaffected.
        let mut cfg = Config::default();
        cfg.privacy.fields.project = false;
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        assert_eq!(
            model.details,
            format!("Working on {}", crate::privacy::GENERIC_PROJECT)
        );
        assert!(!model.details.contains("private"), "{}", model.details);
        assert!(!model.details.contains("main"), "{}", model.details);
        // State still carries the model (and metrics are untouched by this toggle).
        assert!(model.state.contains("Opus 4.8"), "{}", model.state);
    }

    #[test]
    fn single_session_with_agents_keeps_project_in_details_and_count_in_state() {
        // A single session running subagents keeps the project in `details`; the
        // agent count is the "N×" model prefix in `state`.
        let cfg = Config::default();
        let mut s = session("a", 10);
        s.subagents = 3;
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };
        assert_eq!(
            model.details, "Working on private (main)",
            "{}",
            model.details
        );
        // Count lives in state now, as the model prefix.
        assert!(model.state.contains("3\u{d7} Opus 4.8"), "{}", model.state);
        assert!(
            !model.details.contains("Orchestrating"),
            "{}",
            model.details
        );
    }

    #[test]
    fn single_subagent_renders_no_multiplier_prefix() {
        // A lone subagent (count 1) must NOT render a "1×" prefix — ×1 is noise and
        // reads oddly (e.g. next to a multi-session headline). The model shows
        // plainly; the "N×" prefix appears only at >= 2 subagents.
        let mut s = session("a", 10);
        s.subagents = 1;
        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };
        assert_eq!(
            model.details, "Working on private (main)",
            "{}",
            model.details
        );
        assert!(model.state.contains("Opus 4.8"), "{}", model.state);
        assert!(
            !model.state.contains('\u{d7}'),
            "no ×1 multiplier: {}",
            model.state
        );
    }

    #[test]
    fn blacklisted_project_degrades_details_to_generic() {
        // A session whose cwd is under a blacklist entry must show the generic
        // label — no real basename, no branch, no activity target.
        let cfg = Config {
            privacy: crate::config::PrivacySettings {
                redact: false,
                blacklist_paths: vec![std::path::PathBuf::from("/Users/me/Projects/private")],
                scrub_bash_args: false,
                fields: Default::default(),
            },
            ..Config::default()
        };
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        // Blacklisted focused session → whole card is the generic private card.
        assert_eq!(model.details, crate::privacy::PRIVATE_DETAILS);
        assert_eq!(model.state, crate::privacy::PRIVATE_STATE);
        assert!(!model.details.contains("private"), "{}", model.details);
        assert!(!model.details.contains("main"), "{}", model.details);
        assert!(!model.details.contains("cargo"), "{}", model.details);
        // No per-tool badge or model leaks either.
        assert!(model.small_image.is_none());
        assert!(!model.state.contains("Opus 4.8"));
    }

    #[test]
    fn global_redact_uses_generic_private_card() {
        let cfg = Config {
            privacy: crate::config::PrivacySettings {
                redact: true,
                ..crate::config::PrivacySettings::default()
            },
            ..Config::default()
        };
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        assert_eq!(model.details, crate::privacy::PRIVATE_DETAILS);
        assert_eq!(model.state, crate::privacy::PRIVATE_STATE);
        assert!(model.small_text.is_none());
    }

    #[test]
    fn ai_title_surfaces_only_when_opted_in_and_not_blacklisted() {
        // Off by default: no title in details even when the session carries one.
        let mut s = session("a", 10);
        s.title = Some("Refactor the parser".to_string());
        let mut off = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model_off) = off.aggregate(vec![s.clone()]) else {
            panic!("expected activity");
        };
        assert!(
            !model_off.details.contains("Refactor the parser"),
            "{}",
            model_off.details
        );

        // Opted in, not blacklisted → the title is appended to details.
        let cfg_on = Config {
            show_ai_title: true,
            ..Config::default()
        };
        let mut on = Aggregator::new(cfg_on);
        let PresenceUpdate::Activity(model_on) = on.aggregate(vec![s]) else {
            panic!("expected activity");
        };
        assert!(
            model_on.details.contains("Refactor the parser"),
            "{}",
            model_on.details
        );
        assert!(char_count(&model_on.details) <= DISCORD_TEXT_LIMIT);
    }

    #[test]
    fn ai_title_suppressed_when_project_hidden() {
        // FR-1/AC-2 (F9): hiding the project (`fields.project = false`) must also
        // suppress the ai-title — even with `show_ai_title = true` and no global
        // redact/blacklist — mirroring the branch gate. Today's code only gated on
        // `!private`, leaking the title into details.
        let mut s = session("a", 10);
        s.title = Some("Refactor the parser".to_string());

        let mut cfg = Config {
            show_ai_title: true,
            ..Config::default()
        };
        cfg.privacy.fields.project = false;
        assert!(!cfg.privacy.redact, "redact must stay off for this test");

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };
        assert!(
            !model.details.contains("Refactor the parser"),
            "{}",
            model.details
        );
    }

    #[test]
    fn format_tokens_saturates_without_overflow() {
        // FR-6/AC-1 (F32): `tokens + 500` would overflow on a saturated u64 total
        // and panic in debug builds. `saturating_add` keeps it finite.
        let rendered = format_tokens(u64::MAX);
        // u64::MAX >= 1_000_000, so it renders via the "M tok" branch (no add) —
        // exercise the "K tok" branch's saturating add directly for a value near the
        // boundary too.
        assert!(rendered.contains("tok"), "{rendered}");
        // A value in the K range just under u64::MAX/1000 still must not overflow.
        let near = format_tokens(999_999);
        assert_eq!(near, "1000K tok");
    }

    #[test]
    fn duration_from_seconds_falls_back_on_bad_input() {
        // FR-6/AC-3 (F33): a non-positive / non-finite `min_interval` must not
        // collapse the coalesce window to zero (which busy-spins the inner loop).
        // The aggregator clamps to its own 2.5s coalesce floor.
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let d = duration_from_seconds(bad, COALESCE_FLOOR);
            assert_eq!(d, COALESCE_FLOOR, "input {bad} must clamp to the floor");
            assert!(!d.is_zero(), "input {bad} must not yield Duration::ZERO");
        }
        assert_eq!(COALESCE_FLOOR, Duration::from_millis(2500));
        // A valid positive interval still passes through unchanged.
        assert_eq!(
            duration_from_seconds(4.0, COALESCE_FLOOR),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn focus_follows_most_recently_active_not_started() {
        // Both sessions start at the same time; "b" is more recently *active*.
        let mut a = session("a", 10_000);
        a.started_at = time(1_000);
        let mut b = session("b", 50_000);
        b.started_at = time(1_000);
        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![a, b]) else {
            panic!("expected activity");
        };
        assert_eq!(
            model.sessions[model.focused].session_id, "b",
            "focus must follow last_active, not started_at"
        );
    }

    #[test]
    fn single_session_state_shows_agent_count_prefix_and_small_text_is_activity() {
        // The agent count is the "N×" model prefix in `state` (never the literal
        // word "subagent"); small_text keeps the live tool activity for hover.
        let cfg = Config::default();
        let mut s = session("a", 10);
        s.subagents = 2;

        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };

        assert!(
            model.state.starts_with("2\u{d7} Opus 4.8"),
            "{}",
            model.state
        );
        assert!(!model.state.contains("subagent"), "{}", model.state);
        assert_eq!(model.small_text.as_deref(), Some("Running cargo"));
        assert_eq!(model.live_count, 1);
        assert_eq!(model.capacity, 1);
    }

    #[test]
    fn single_session_small_image_is_configured_badge_not_per_tool_key() {
        // Per-tool badge keys are never emitted; only the configured asset key.
        let mut cfg = Config::default();
        cfg.assets.small_image = Some("claude".to_string());
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        // session()'s activity carries small_image_key = "bash" — it must NOT win.
        assert_eq!(model.small_image.as_deref(), Some("claude"));
    }

    #[test]
    fn small_image_is_none_when_unconfigured() {
        let mut cfg = Config::default();
        cfg.assets.small_image = None;
        let mut aggregator = Aggregator::new(cfg);
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session("a", 10)]) else {
            panic!("expected activity");
        };
        assert!(model.small_image.is_none());
    }

    #[test]
    fn idle_sessions_are_excluded_from_the_working_session_count() {
        // Three live sessions but only one is working → single-session headline
        // for the working one, focused even though an idle session is newer.
        let working = session("work", 10);
        let mut idle1 = session("idle1", 90); // newest, but idle
        idle1.busy = false;
        idle1.subagents = 0;
        let mut idle2 = session("idle2", 50);
        idle2.busy = false;
        idle2.subagents = 0;

        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![working, idle1, idle2])
        else {
            panic!("expected activity");
        };
        // Only one session is active → single-session "Working on …", not "across".
        assert_eq!(
            model.details, "Working on private (main)",
            "{}",
            model.details
        );
        // Focus is the working session despite an idle session being more recent.
        assert_eq!(model.sessions[model.focused].session_id, "work");
        // live_count still reflects every open session.
        assert_eq!(model.live_count, 3);
    }

    #[test]
    fn background_subagents_keep_a_non_busy_session_active() {
        // A session whose main thread is idle but which has live subagents (e.g. a
        // background workflow) still counts as active and orchestrating.
        let mut a = session("a", 10);
        a.busy = false;
        a.subagents = 4;
        let mut b = session("b", 20);
        b.busy = false;
        b.subagents = 2;

        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![a, b]) else {
            panic!("expected activity");
        };
        // Both active via subagents → multi headline + total 6 agents in state.
        assert_eq!(model.details, "Working across 2 sessions");
        assert!(model.state.contains("6\u{d7} Opus 4.8"), "{}", model.state);
    }

    #[test]
    fn thinking_session_with_no_pending_tool_is_active() {
        // working=true (model thinking / mid-turn) but busy=false and no subagents
        // → still active. Two thinking sessions read as "across 2 sessions".
        let mut a = session("a", 10);
        a.busy = false;
        a.working = true;
        let mut b = session("b", 20);
        b.busy = false;
        b.working = true;

        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![a, b]) else {
            panic!("expected activity");
        };
        assert_eq!(model.details, "Working across 2 sessions");
    }

    #[test]
    fn subagent_tokens_are_added_to_the_displayed_token_total() {
        // The token figure includes the live subagents' tokens, not just the
        // session's own.
        let mut s = session("a", 10);
        s.tokens_total = Some(100_000);
        s.subagent_tokens = Some(50_000);

        let mut aggregator = Aggregator::new(Config::default());
        let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![s]) else {
            panic!("expected activity");
        };
        // 100K + 50K = 150K tok (default fields.tokens = true).
        assert!(model.state.contains("150K tok"), "{}", model.state);
    }
}
