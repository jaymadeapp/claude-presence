//! The Discord IPC sink: a single aggregated presence driven over local IPC
//! (ADR-3, ADR-6).
//!
//! The `discord-rich-presence` crate is **synchronous**, so the sink runs on a
//! dedicated OS thread fed by the aggregator's `watch::Receiver<PresenceUpdate>`
//! (ADR-6). The thread hosts a small current-thread tokio runtime purely so it
//! can `await` the watch channels (presence updates + a shutdown signal) and the
//! debounce/keepalive timers without busy-polling; all blocking IPC stays off the
//! async collector runtime.
//!
//! Responsibilities (FR-6):
//! - connect/handshake with the configured `client_id`, letting the crate scan
//!   `[XDG_RUNTIME_DIR, TMPDIR, TMP, TEMP]` for the `discord-ipc-0..9` socket;
//!   retry with backoff when Discord is absent (AC-1);
//! - push `SET_ACTIVITY` from a [`PresenceModel`], mapping
//!   `timestamps.start`/party/assets/buttons (AC-2);
//! - clear the presence on shutdown and on empty-state (AC-2, FR-5/AC-6);
//! - debounce to at most one publish per `min_interval` — including across
//!   reconnects — and republish a keepalive every `keepalive_interval`
//!   (AC-3, AC-5);
//! - detect a send failure, tear down, and reconnect with backoff (AC-4).

use std::time::{Duration, Instant};

use discord_rich_presence::activity::{Activity, Assets, Button, Timestamps};
use discord_rich_presence::{DiscordIpc, DiscordIpcClient};
use tokio::sync::watch;

use crate::config::Config;
use crate::state::aggregator::PresenceUpdate;
use crate::state::model::PresenceModel;

/// Discord's per-`large_image`/`small_image` hover-text cap and the button/label
/// caps are enforced upstream by the aggregator; the sink only maps fields.
const MAX_BUTTONS: usize = 2;

/// Backoff schedule used when Discord is absent at startup or after a send
/// failure (FR-6/AC-1, AC-4): exponential from `BACKOFF_MIN` to `BACKOFF_MAX`.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Run the Discord sink on a dedicated OS thread.
///
/// This is the public entry point wired by task 2.3. It is **non-blocking**: it
/// spawns the worker thread and returns its [`std::thread::JoinHandle`]. The
/// thread runs until `shutdown` flips to `true` (the caller signals a clean
/// shutdown by `send`ing `true` on the shutdown channel), at which point the sink
/// clears the presence and exits.
///
/// # Arguments
/// - `rx`: the aggregator's debounced presence stream
///   ([`crate::state::aggregator::aggregate_channel`]).
/// - `cfg`: daemon config; `client_id`, `min_interval`, `keepalive_interval` are
///   consumed here.
/// - `shutdown`: a `watch` channel; flipping it to `true` triggers a clean
///   teardown (clear presence, close IPC, exit the thread).
///
/// # Example wiring (task 2.3)
/// ```ignore
/// let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
/// let handle = discord::sink::run_sink(presence_rx, cfg, shutdown_rx);
/// // ... on SIGTERM/SIGINT:
/// let _ = shutdown_tx.send(true);
/// let _ = handle.join();
/// ```
pub fn run_sink(
    rx: watch::Receiver<PresenceUpdate>,
    cfg: Config,
    shutdown: watch::Receiver<bool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("discord-sink".to_string())
        .spawn(move || {
            // A current-thread runtime lets the sync sink await the watch
            // channels and timers without spinning, while all blocking IPC stays
            // on this dedicated OS thread (ADR-6).
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    tracing::error!(%err, "discord sink: failed to build runtime; sink disabled");
                    return;
                }
            };
            runtime.block_on(sink_loop(rx, cfg, shutdown));
        })
        .unwrap_or_else(|err| {
            tracing::error!(%err, "discord sink: failed to spawn thread; sink disabled");
            // Spawn a no-op thread so the signature always yields a JoinHandle.
            std::thread::spawn(|| {})
        })
}

/// The driver loop: keep a live IPC connection and publish presence updates with
/// debounce + keepalive, reconnecting with backoff on any failure.
async fn sink_loop(
    mut rx: watch::Receiver<PresenceUpdate>,
    cfg: Config,
    mut shutdown: watch::Receiver<bool>,
) {
    let min_interval = duration_from_seconds(cfg.min_interval, Duration::from_millis(2500));
    let keepalive = duration_from_seconds(cfg.keepalive_interval, Duration::from_secs(15));

    let mut client = DiscordIpcClient::new(cfg.client_id.to_string());

    // Persist the last publish time across reconnects so a flapping connection
    // can't emit back-to-back `SET_ACTIVITY` calls below `min_interval` and blow
    // Discord's 5-updates/20s budget (FR-6/AC-5). A per-`serve` clock would reset
    // on every reconnect; this one outlives them.
    let mut last_publish_at: Option<Instant> = None;

    'outer: loop {
        if *shutdown.borrow() {
            break;
        }

        // (Re)connect with backoff. Retry indefinitely while Discord is absent
        // rather than exiting (FR-6/AC-1), but bail out promptly on shutdown.
        if !connect_with_backoff(&mut client, &mut shutdown).await {
            break;
        }

        // Drive presence from the connection until a send fails (→ reconnect) or
        // shutdown is requested (→ clear + exit).
        match serve(
            &mut client,
            &mut rx,
            &mut shutdown,
            min_interval,
            keepalive,
            &mut last_publish_at,
        )
        .await
        {
            ServeOutcome::Shutdown => {
                // Best-effort clear on clean shutdown (FR-6/AC-2).
                if let Err(err) = client.clear_activity() {
                    tracing::debug!(%err, "discord sink: clear on shutdown failed");
                }
                let _ = client.close();
                break 'outer;
            }
            ServeOutcome::Disconnected => {
                tracing::warn!("discord sink: connection lost; reconnecting");
                let _ = client.close();
                // Loop back to reconnect with backoff (FR-6/AC-4).
            }
        }
    }

    tracing::info!("discord sink: stopped");
}

/// Outcome of [`serve`]: why the inner loop returned.
enum ServeOutcome {
    /// A clean shutdown was requested.
    Shutdown,
    /// A send/IPC error occurred; the caller should reconnect.
    Disconnected,
}

/// Connect (with handshake) to Discord, retrying with exponential backoff while
/// the daemon keeps running. Returns `false` only if shutdown was requested
/// before a connection was established (so the caller exits cleanly).
async fn connect_with_backoff(
    client: &mut DiscordIpcClient,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let mut backoff = BACKOFF_MIN;
    loop {
        if *shutdown.borrow() {
            return false;
        }

        // Attempt to connect directly: the crate's `connect` scans
        // `[XDG_RUNTIME_DIR, TMPDIR, TMP, TEMP]` for the `discord-ipc-0..9`
        // socket, which is wider than a `$TMPDIR`-only pre-probe. Relying on its
        // `Err` arm for retry keeps the "retry, don't exit" semantics while
        // staying correct when the socket lives outside `$TMPDIR`.
        match client.connect() {
            Ok(()) => {
                tracing::info!("discord sink: connected");
                return true;
            }
            Err(err) => {
                tracing::debug!(%err, "discord sink: connect failed; will retry");
            }
        }

        // Backoff, but wake immediately on shutdown.
        if wait_or_shutdown(backoff, shutdown).await {
            return false;
        }
        backoff = next_backoff(backoff);
    }
}

/// Serve presence updates over an established connection until shutdown or a send
/// failure. Implements debounce (`min_interval`), keepalive republish
/// (`keepalive_interval`), empty-state clear, and coalescing to the newest model.
async fn serve(
    client: &mut DiscordIpcClient,
    rx: &mut watch::Receiver<PresenceUpdate>,
    shutdown: &mut watch::Receiver<bool>,
    min_interval: Duration,
    keepalive: Duration,
    last_publish_at: &mut Option<Instant>,
) -> ServeOutcome {
    // Publish the current value on (re)connect so a fresh socket reflects live
    // state at once; this also seeds the keepalive baseline. Gate it by
    // `min_interval` against the *persisted* last publish so a flapping
    // connection can't burst past Discord's rate limit (FR-6/AC-5).
    if let Some(wait) = debounce_remaining(*last_publish_at, min_interval) {
        if wait_or_shutdown(wait, shutdown).await {
            return ServeOutcome::Shutdown;
        }
    }
    let current = rx.borrow().clone();
    if publish(client, &current).is_err() {
        return ServeOutcome::Disconnected;
    }
    let mut last_published: Option<PresenceUpdate> = Some(current);
    *last_publish_at = Some(Instant::now());

    loop {
        if *shutdown.borrow() {
            return ServeOutcome::Shutdown;
        }

        // Time until the next keepalive republish is due.
        let keepalive_due = (*last_publish_at)
            .map(|at| keepalive.saturating_sub(at.elapsed()))
            .unwrap_or(Duration::ZERO);
        let keepalive_sleep = tokio::time::sleep(keepalive_due);
        tokio::pin!(keepalive_sleep);

        tokio::select! {
            // Shutdown requested → clear + exit.
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return ServeOutcome::Shutdown;
                }
            }

            // New presence model. Coalesce + debounce before publishing.
            changed = rx.changed() => {
                if changed.is_err() {
                    // Aggregator gone → nothing more to serve; treat as shutdown.
                    return ServeOutcome::Shutdown;
                }
                // Debounce/coalesce: respect min_interval since the last publish,
                // collapsing any bursts to the newest value (FR-6/AC-3, AC-5).
                if let Some(wait) = debounce_remaining(*last_publish_at, min_interval) {
                    if wait_or_shutdown(wait, shutdown).await {
                        return ServeOutcome::Shutdown;
                    }
                }
                let latest = rx.borrow().clone(); // newest after the debounce wait
                if update_changed(last_published.as_ref(), &latest) {
                    match publish(client, &latest) {
                        Ok(()) => {
                            last_published = Some(latest);
                            *last_publish_at = Some(Instant::now());
                        }
                        Err(()) => return ServeOutcome::Disconnected,
                    }
                }
            }

            // Keepalive: republish the last model so the presence doesn't expire
            // (FR-6/AC-3).
            _ = &mut keepalive_sleep => {
                if let Some(model) = last_published.clone() {
                    match publish(client, &model) {
                        Ok(()) => *last_publish_at = Some(Instant::now()),
                        Err(()) => return ServeOutcome::Disconnected,
                    }
                } else {
                    *last_publish_at = Some(Instant::now());
                }
            }
        }
    }
}

/// Apply a [`PresenceUpdate`] to the live connection. Maps `Clear` to
/// `clear_activity` (empty-state, FR-5/AC-6) and `Activity` to `set_activity`.
///
/// Returns `Err(())` on any IPC failure so the caller reconnects; the underlying
/// `discord_rich_presence` error is logged, never propagated as a crate error
/// (the sink degrades, it never panics).
fn publish(client: &mut DiscordIpcClient, update: &PresenceUpdate) -> Result<(), ()> {
    match update {
        PresenceUpdate::Clear => client.clear_activity().map_err(|err| {
            tracing::debug!(%err, "discord sink: clear_activity failed");
        }),
        PresenceUpdate::Activity(model) => {
            let activity = build_activity(model);
            client.set_activity(activity).map_err(|err| {
                tracing::debug!(%err, "discord sink: set_activity failed");
            })
        }
    }
}

/// Build a `discord-rich-presence` [`Activity`] from a [`PresenceModel`].
///
/// Mapping (design §4.3): `details`/`state` verbatim (already ≤128, sanitized by
/// the aggregator); `timestamps.start` = `started_at_ms` **in milliseconds**
/// (FR-5/AC-4 — do NOT divide); assets only when their keys are non-empty (images
/// are optional); buttons only when present (https-only is enforced by the
/// aggregator), capped at Discord's 2-button max.
///
/// `party.size` is intentionally NOT set: Discord renders it as a "(N of M)"
/// suffix on the narrow profile card, clipping `state`. The session count now
/// lives in `details`/`small_text` instead. `live_count`/`capacity` remain on the
/// model for other consumers; they are just not mapped to the Discord activity.
///
/// Borrows from `model`, so the returned `Activity` shares its lifetime.
fn build_activity(model: &PresenceModel) -> Activity<'_> {
    let mut activity = Activity::new();

    if !model.details.is_empty() {
        activity = activity.details(model.details.as_str());
    }
    if !model.state.is_empty() {
        activity = activity.state(model.state.as_str());
    }

    // `started_at_ms` is already epoch MILLISECONDS — pass through unchanged.
    activity = activity.timestamps(Timestamps::new().start(model.started_at_ms));

    if let Some(assets) = build_assets(model) {
        activity = activity.assets(assets);
    }

    let buttons = build_buttons(model);
    if !buttons.is_empty() {
        activity = activity.buttons(buttons);
    }

    activity
}

/// Build the `Assets` block, or `None` if there is nothing to show.
///
/// `large_image`/`small_image` are optional: omit a key when empty so the MVP
/// shows a valid card before any art asset exists (CLAUDE.md "Project specifics").
fn build_assets(model: &PresenceModel) -> Option<Assets<'_>> {
    let mut assets = Assets::new();
    let mut any = false;

    if !model.large_image.is_empty() {
        assets = assets.large_image(model.large_image.as_str());
        any = true;
    }
    // `large_text` is the hover tooltip for the large image. Set it even with no
    // `large_image` asset of our own: Discord falls back to the app icon as the
    // large image, and `large_text` still labels it on hover (e.g. "Claude Code").
    if !model.large_text.is_empty() {
        assets = assets.large_text(model.large_text.as_str());
        any = true;
    }

    if let Some(small_image) = model.small_image.as_deref().filter(|s| !s.is_empty()) {
        assets = assets.small_image(small_image);
        any = true;
        if let Some(small_text) = model.small_text.as_deref().filter(|s| !s.is_empty()) {
            assets = assets.small_text(small_text);
        }
    }

    any.then_some(assets)
}

/// Map the model's `(label, url)` pairs to crate `Button`s, capped at the
/// 2-button Discord maximum. URL validity (https-only) is guaranteed upstream.
fn build_buttons(model: &PresenceModel) -> Vec<Button<'_>> {
    model
        .buttons
        .iter()
        .take(MAX_BUTTONS)
        .map(|(label, url)| Button::new(label.as_str(), url.as_str()))
        .collect()
}

/// Whether `next` differs from `prev`, so we only publish on change (FR-6/AC-3).
fn update_changed(prev: Option<&PresenceUpdate>, next: &PresenceUpdate) -> bool {
    match prev {
        None => true,
        Some(prev) => !updates_eq(prev, next),
    }
}

/// Structural equality of two presence updates for the change-detection above.
///
/// `PresenceUpdate`/`PresenceModel` don't derive `PartialEq`, so compare the
/// fields that actually map onto the Discord card.
fn updates_eq(a: &PresenceUpdate, b: &PresenceUpdate) -> bool {
    match (a, b) {
        (PresenceUpdate::Clear, PresenceUpdate::Clear) => true,
        (PresenceUpdate::Activity(a), PresenceUpdate::Activity(b)) => models_eq(a, b),
        _ => false,
    }
}

/// Field-wise equality of the card-relevant `PresenceModel` fields.
fn models_eq(a: &PresenceModel, b: &PresenceModel) -> bool {
    a.details == b.details
        && a.state == b.state
        && a.started_at_ms == b.started_at_ms
        && a.live_count == b.live_count
        && a.capacity == b.capacity
        && a.large_image == b.large_image
        && a.large_text == b.large_text
        && a.small_image == b.small_image
        && a.small_text == b.small_text
        && a.buttons == b.buttons
}

/// Sleep for `delay`, returning early as `true` if shutdown is signalled first.
async fn wait_or_shutdown(delay: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if delay.is_zero() {
        return *shutdown.borrow();
    }
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    tokio::select! {
        _ = &mut sleep => *shutdown.borrow(),
        changed = shutdown.changed() => changed.is_err() || *shutdown.borrow(),
    }
}

/// How long to wait before the next publish is allowed, given the *persisted*
/// last-publish time and `min_interval` (FR-6/AC-5). `None` means no wait is
/// needed — either nothing has been published yet, or `min_interval` has already
/// elapsed. Persisting `last_publish_at` across reconnects lets this bound the
/// on-(re)connect publish too, so a flapping connection can't exceed Discord's
/// rate limit.
fn debounce_remaining(
    last_publish_at: Option<Instant>,
    min_interval: Duration,
) -> Option<Duration> {
    let at = last_publish_at?;
    let elapsed = at.elapsed();
    if elapsed < min_interval {
        Some(min_interval - elapsed)
    } else {
        None
    }
}

/// Exponential backoff step, capped at [`BACKOFF_MAX`].
fn next_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(BACKOFF_MAX)
}

/// Parse a seconds value into a `Duration`, falling back to `fallback` for
/// non-finite or non-positive inputs (a zero interval would defeat the rate
/// limit / keepalive).
fn duration_from_seconds(seconds: f64, fallback: Duration) -> Duration {
    if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::model::PresenceModel;

    fn model() -> PresenceModel {
        PresenceModel {
            details: "Working on private (main)".to_string(),
            state: "Opus 4.8 \u{b7} Max 20x".to_string(),
            started_at_ms: 1_781_989_000_123,
            live_count: 2,
            capacity: 5,
            large_image: "cc-logo".to_string(),
            large_text: "Claude Code".to_string(),
            small_image: Some("bash".to_string()),
            small_text: Some("Running cargo".to_string()),
            buttons: vec![("Repo".to_string(), "https://example.com".to_string())],
            ..PresenceModel::default()
        }
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let mut d = BACKOFF_MIN;
        assert_eq!(d, Duration::from_secs(1));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(2));
        d = next_backoff(d);
        assert_eq!(d, Duration::from_secs(4));
        // Walk up to and past the cap; it must saturate at BACKOFF_MAX.
        for _ in 0..10 {
            d = next_backoff(d);
        }
        assert_eq!(d, BACKOFF_MAX);
        assert_eq!(next_backoff(BACKOFF_MAX), BACKOFF_MAX);
    }

    #[test]
    fn timestamp_start_is_passed_as_milliseconds() {
        // The activity serializes timestamps.start verbatim from started_at_ms;
        // assert via the JSON since the crate's fields are private.
        let m = model();
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert_eq!(json["timestamps"]["start"], 1_781_989_000_123i64);
    }

    #[test]
    fn party_is_not_set_on_the_activity() {
        // `party.size` clips `state` on Discord's narrow profile card, so it is
        // intentionally omitted — the session count lives in details/small_text.
        let m = model();
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert!(
            json.get("party").is_none(),
            "party must not be mapped to the Discord activity: {json:?}"
        );
    }

    #[test]
    fn details_state_buttons_are_mapped() {
        let m = model();
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert_eq!(json["details"], "Working on private (main)");
        assert_eq!(json["state"], "Opus 4.8 \u{b7} Max 20x");
        assert_eq!(json["buttons"][0]["label"], "Repo");
        assert_eq!(json["buttons"][0]["url"], "https://example.com");
    }

    #[test]
    fn large_text_tooltip_emitted_without_image_keys() {
        // With no image keys but large_text set, assets still carry large_text so
        // the app-icon fallback large image gets a "Claude Code" hover tooltip.
        let mut m = model();
        m.large_image = String::new();
        m.small_image = None;
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert_eq!(json["assets"]["large_text"], "Claude Code");
        assert!(json["assets"].get("large_image").is_none());
        assert!(json["assets"].get("small_image").is_none());
    }

    #[test]
    fn assets_omitted_when_fully_empty() {
        let mut m = model();
        m.large_image = String::new();
        m.small_image = None;
        m.large_text = String::new();
        m.small_text = None;
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert!(
            json.get("assets").is_none(),
            "assets must be omitted when nothing to show"
        );
    }

    #[test]
    fn assets_present_when_keys_set() {
        let m = model();
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert_eq!(json["assets"]["large_image"], "cc-logo");
        assert_eq!(json["assets"]["large_text"], "Claude Code");
        assert_eq!(json["assets"]["small_image"], "bash");
        assert_eq!(json["assets"]["small_text"], "Running cargo");
    }

    #[test]
    fn buttons_capped_at_two() {
        let mut m = model();
        m.buttons = vec![
            ("A".to_string(), "https://a.example".to_string()),
            ("B".to_string(), "https://b.example".to_string()),
            ("C".to_string(), "https://c.example".to_string()),
        ];
        let buttons = build_buttons(&m);
        assert_eq!(buttons.len(), MAX_BUTTONS);
    }

    #[test]
    fn empty_buttons_field_is_absent() {
        let mut m = model();
        m.buttons.clear();
        let json = serde_json::to_value(build_activity(&m)).expect("serialize");
        assert!(
            json.get("buttons").is_none(),
            "no buttons field when none configured"
        );
    }

    #[test]
    fn change_detection_seeds_then_dedupes() {
        let a = PresenceUpdate::Activity(Box::new(model()));
        // First publish (no previous) always counts as a change.
        assert!(update_changed(None, &a));
        // Identical model → no change (debounce only republishes on change).
        assert!(!update_changed(Some(&a), &a));
        // Clear vs Activity differ.
        assert!(update_changed(Some(&PresenceUpdate::Clear), &a));
        // Clear vs Clear is unchanged.
        assert!(!update_changed(
            Some(&PresenceUpdate::Clear),
            &PresenceUpdate::Clear
        ));
    }

    #[test]
    fn change_detection_tracks_card_fields() {
        let base = PresenceUpdate::Activity(Box::new(model()));

        let mut m2 = model();
        m2.state = "Sonnet 4.6 | Pro".to_string();
        let changed_state = PresenceUpdate::Activity(Box::new(m2));
        assert!(update_changed(Some(&base), &changed_state));

        let mut m3 = model();
        m3.live_count = 3;
        let changed_party = PresenceUpdate::Activity(Box::new(m3));
        assert!(update_changed(Some(&base), &changed_party));
    }

    #[test]
    fn duration_from_seconds_falls_back_on_bad_input() {
        let fb = Duration::from_secs(15);
        assert_eq!(duration_from_seconds(2.5, fb), Duration::from_secs_f64(2.5));
        assert_eq!(duration_from_seconds(0.0, fb), fb);
        assert_eq!(duration_from_seconds(-1.0, fb), fb);
        assert_eq!(duration_from_seconds(f64::NAN, fb), fb);
        assert_eq!(duration_from_seconds(f64::INFINITY, fb), fb);
    }

    #[test]
    fn debounce_remaining_accounts_for_persisted_last_publish() {
        let min_interval = Duration::from_secs(5);

        // No prior publish → no wait (first publish is free).
        assert_eq!(debounce_remaining(None, min_interval), None);

        // A publish that just happened → must wait nearly the full interval.
        // (Bound it loosely to stay robust against scheduling jitter.)
        let just_now = Instant::now();
        let wait = debounce_remaining(Some(just_now), min_interval)
            .expect("a fresh publish must force a wait");
        assert!(
            wait > Duration::from_secs(4) && wait <= min_interval,
            "expected ~5s remaining, got {wait:?}"
        );

        // A publish older than min_interval → no wait, even across a reconnect.
        let long_ago = Instant::now()
            .checked_sub(min_interval + Duration::from_secs(1))
            .expect("instant arithmetic");
        assert_eq!(debounce_remaining(Some(long_ago), min_interval), None);
    }
}
