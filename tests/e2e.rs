//! End-to-end aggregation smoke test (task 5.2, FR-5/AC-2..AC-6, NFR-2, C-3).
//!
//! A black-box exercise of the aggregation contract through the **public crate
//! API** (`claude_presence::state::aggregator::…`) — no `#[path]` include, no
//! real `~/.claude` data, no live Discord. Inline fixture [`SessionState`]s are
//! fed through [`Aggregator::aggregate`] and the resulting [`PresenceModel`] is
//! asserted against the Discord contract:
//!
//! - non-empty path → `Activity` with `details`/`state` ≤128 chars (C-3,
//!   FR-5/AC-3), `live_count`/`capacity` tracking the session set (FR-5/AC-2),
//!   and a milliseconds-magnitude `started_at_ms` (FR-5/AC-4);
//! - empty input → `Clear` (FR-5/AC-6, NFR-2 degrade-don't-fail);
//! - a pathologically long project/plan still fits ≤128 (C-3);
//! - with multiple sessions, the most-recently-active is focused, the timer
//!   reflects the earliest start, and subagents are *not* counted in the live
//!   count (FR-5/AC-1, AC-2).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use claude_presence::config::{Assets, Config, FieldToggles, PrivacySettings};
use claude_presence::privacy::{PRIVATE_DETAILS, PRIVATE_STATE};
use claude_presence::state::aggregator::{Aggregator, PresenceUpdate, DISCORD_TEXT_LIMIT};
use claude_presence::state::model::{Activity, SessionState};

/// A millisecond `SystemTime` `ms` after the Unix epoch.
fn time(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// `char` count, mirroring the aggregator's own ≤128 measurement (C-3).
fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// Build a realistic fixture session. `started_ms`/`last_active_ms` are epoch
/// milliseconds so the resulting `started_at_ms` is a true ms-magnitude value.
fn fixture(id: &str, project: &str, started_ms: u64, last_active_ms: u64) -> SessionState {
    SessionState {
        session_id: id.to_string(),
        pid: 1000,
        project: project.to_string(),
        cwd: std::path::PathBuf::from(format!("/Users/me/Projects/{project}")),
        branch: Some("main".to_string()),
        model: Some("claude-opus-4-8".to_string()),
        started_at: time(started_ms),
        last_active: time(last_active_ms),
        busy: true,
        working: false,
        activity: Some(Activity {
            verb: "Editing".to_string(),
            target: Some("main.rs".to_string()),
            small_image_key: Some("edit".to_string()),
        }),
        title: None,
        cost_usd: Some(1.23),
        ctx_pct: Some(12.5),
        tokens_total: Some(420_000),
        subagents: 0,
        subagent_tokens: None,
    }
}

/// A non-default config exercising the full card (all fields on, fixed capacity,
/// a plan label, asset keys).
fn full_config(capacity: u32) -> Config {
    Config {
        plan_label: "Max 20x".to_string(),
        capacity: Some(capacity),
        fields: FieldToggles {
            timestamp: true,
            cost: true,
            tokens: true,
            context_pct: true,
            branch: true,
        },
        assets: Assets {
            large_image: Some("cc-logo".to_string()),
            small_image: Some("claude".to_string()),
        },
        ..Config::default()
    }
}

/// Non-empty path: ≥2 fixtures → `Activity` whose `details`/`state` fit ≤128,
/// whose `live_count`/`capacity` track the session count (with `capacity ≥
/// live_count`), and whose `started_at_ms` is a real milliseconds value equal to
/// the *earliest* session start (the multi-session timer reflects total time
/// working) (FR-5/AC-2..AC-4, C-3).
#[test]
fn aggregates_sessions_into_correct_presence_model() {
    let started_a = 1_781_989_000_000;
    let started_b = 1_781_989_500_000;
    let sessions = vec![
        fixture("a", "alpha", started_a, 10_000),
        // "b" is the most-recently-active (last_active 20_000 > 10_000), so it
        // becomes the focused/headline session (FR-5/AC-1).
        fixture("b", "beta", started_b, 20_000),
    ];

    let mut aggregator = Aggregator::new(full_config(8));
    let PresenceUpdate::Activity(model) = aggregator.aggregate(sessions) else {
        panic!("two live sessions must yield an Activity, not Clear");
    };

    // C-3 / FR-5/AC-3: both card text fields hard-capped at 128 chars.
    assert!(
        char_count(&model.details) <= DISCORD_TEXT_LIMIT,
        "details exceeds 128 chars: {:?}",
        model.details
    );
    assert!(
        char_count(&model.state) <= DISCORD_TEXT_LIMIT,
        "state exceeds 128 chars: {:?}",
        model.state
    );

    // Multi-session headline aggregates across sessions; no project name leaks.
    assert_eq!(model.details, "Working across 2 sessions");

    // FR-5/AC-2: live_count = #sessions, capacity >= live_count.
    assert_eq!(
        model.live_count, 2,
        "live_count must equal the session count"
    );
    assert_eq!(
        model.capacity, 8,
        "capacity must reflect the configured max"
    );
    assert!(
        model.capacity >= model.live_count,
        "capacity must never be below live_count"
    );

    // FR-5/AC-1: the most-recently-active session ("b") is focused.
    assert_eq!(
        model.sessions[model.focused].session_id, "b",
        "focused session must be the most-recently-active"
    );

    // FR-5/AC-4: started_at_ms is a millisecond-magnitude value (13 digits for
    // current dates → > 1e12). For the multi-session card it is the EARLIEST
    // start ("a"), so the timer reflects total time working, NOT seconds.
    assert!(
        model.started_at_ms > 1_000_000_000_000,
        "started_at_ms must be epoch milliseconds (>1e12), got {}",
        model.started_at_ms
    );
    assert_eq!(
        model.started_at_ms, started_a as i64,
        "multi-session started_at_ms must equal the earliest session start in ms"
    );
}

/// Empty-state clear path: zero sessions → `Clear` so the Discord presence is
/// dropped and held cleared (FR-5/AC-6; NFR-2 degrade rather than fail).
#[test]
fn empty_session_set_clears_presence() {
    let mut aggregator = Aggregator::new(Config::default());
    assert!(
        matches!(aggregator.aggregate(Vec::new()), PresenceUpdate::Clear),
        "zero live sessions must signal Clear"
    );

    // Stays cleared on a subsequent empty tick (held cleared, FR-5/AC-6).
    assert!(matches!(
        aggregator.aggregate(Vec::new()),
        PresenceUpdate::Clear
    ));
}

/// Truncation: a pathologically long project and plan label still produce a
/// card whose `details` and `state` are ≤128 chars (C-3, FR-5/AC-3).
#[test]
fn pathologically_long_fields_stay_within_128() {
    let mut session = fixture("x", &"project-".repeat(50), 1_781_989_000_000, 10_000);
    session.branch = Some("feature/".to_string() + &"x".repeat(200));

    let cfg = Config {
        // A long *unknown* plan label survives abbreviation, forcing the ladder
        // into its metric-drop + final hard-cap rungs.
        plan_label: "Enterprise-Unlimited ".repeat(20).trim().to_string(),
        ..full_config(1)
    };

    let mut aggregator = Aggregator::new(cfg);
    let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session]) else {
        panic!("expected an Activity for one live session");
    };

    assert!(
        char_count(&model.details) <= DISCORD_TEXT_LIMIT,
        "details must be capped at 128 even with a huge project/branch: len {}",
        char_count(&model.details)
    );
    assert!(
        char_count(&model.state) <= DISCORD_TEXT_LIMIT,
        "state must be capped at 128 even with a huge plan label: len {}",
        char_count(&model.state)
    );
}

/// Focus + live-count shape with subagents: the focused session is the
/// most-recently active, `live_count` counts every top-level session, and
/// subagents are NOT folded into the live count (FR-5/AC-1, AC-2).
#[test]
fn focus_picks_newest_and_subagents_are_not_in_live_count() {
    let mut older = fixture("older", "alpha", 1_781_989_000_000, 5_000);
    // Many subagents on a non-focused session must not inflate live_count.
    older.subagents = 9;

    let newer = fixture("newer", "beta", 1_781_989_100_000, 50_000);

    // Capacity left unset → defaults to live_count (FR-5/AC-2).
    let mut aggregator = Aggregator::new(Config::default());
    let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![older, newer]) else {
        panic!("expected an Activity for two live sessions");
    };

    assert_eq!(
        model.sessions[model.focused].session_id, "newer",
        "the most-recently-active session must be focused"
    );
    assert_eq!(
        model.live_count, 2,
        "live_count counts top-level sessions only, never subagents"
    );
    assert_eq!(
        model.capacity, 2,
        "unset capacity defaults to live_count (FR-5/AC-2)"
    );
    // Multi-session headline counts the working sessions; the 9 running agents
    // become the "N×" model prefix in state, not part of the headline.
    assert_eq!(model.details, "Working across 2 sessions");
    assert!(
        model.state.contains("9\u{d7}"),
        "state was {:?}",
        model.state
    );
}

/// Privacy wiring through the public aggregator (C-7, FR-7/AC-2):
/// (a) default (redact = false, no blacklist) shows the real basename + branch;
/// (b) a blacklisted focused session collapses to the generic private card with
///     no real basename and no branch;
/// (c) global redact = true also collapses to the generic private card.
#[test]
fn privacy_wiring_blacklist_and_redact_collapse_the_card() {
    let started = 1_781_989_000_000;

    // (a) Default config: the card is informative — real basename + branch.
    let mut default_agg = Aggregator::new(Config::default());
    let PresenceUpdate::Activity(default_model) =
        default_agg.aggregate(vec![fixture("a", "private", started, 10_000)])
    else {
        panic!("expected activity");
    };
    assert!(
        default_model.details.contains("private"),
        "default card must show the real basename: {:?}",
        default_model.details
    );
    assert!(
        default_model.details.contains("(main)"),
        "default card must show the branch: {:?}",
        default_model.details
    );

    // (b) Blacklisted focused session → generic private card, nothing identifying.
    let blacklist_cfg = Config {
        privacy: PrivacySettings {
            redact: false,
            blacklist_paths: vec![std::path::PathBuf::from("/Users/me/Projects/private")],
            scrub_bash_args: false,
            fields: Default::default(),
        },
        ..Config::default()
    };
    let mut bl_agg = Aggregator::new(blacklist_cfg);
    let PresenceUpdate::Activity(bl_model) =
        bl_agg.aggregate(vec![fixture("a", "private", started, 10_000)])
    else {
        panic!("expected activity");
    };
    assert_eq!(bl_model.details, PRIVATE_DETAILS);
    assert_eq!(bl_model.state, PRIVATE_STATE);
    assert!(
        !bl_model.details.contains("private") && !bl_model.details.contains("main"),
        "blacklisted card must not leak basename or branch: {:?}",
        bl_model.details
    );

    // (c) Global redact = true → generic private card regardless of blacklist.
    let redact_cfg = Config {
        privacy: PrivacySettings {
            redact: true,
            ..PrivacySettings::default()
        },
        ..Config::default()
    };
    let mut rd_agg = Aggregator::new(redact_cfg);
    let PresenceUpdate::Activity(rd_model) =
        rd_agg.aggregate(vec![fixture("a", "private", started, 10_000)])
    else {
        panic!("expected activity");
    };
    assert_eq!(rd_model.details, PRIVATE_DETAILS);
    assert_eq!(rd_model.state, PRIVATE_STATE);

    // (d) Multi-session with a blacklisted focused session still never leaks the
    // project name — the whole card collapses to the generic private card.
    let mut bl_multi = Aggregator::new(Config {
        privacy: PrivacySettings {
            redact: false,
            blacklist_paths: vec![std::path::PathBuf::from("/Users/me/Projects/private")],
            scrub_bash_args: false,
            fields: Default::default(),
        },
        ..Config::default()
    });
    let PresenceUpdate::Activity(bl_multi_model) = bl_multi.aggregate(vec![
        fixture("a", "alpha", started, 10_000),
        // "private" is most-recently-active → focused → triggers the private card.
        fixture("b", "private", started, 20_000),
    ]) else {
        panic!("expected activity");
    };
    assert_eq!(bl_multi_model.details, PRIVATE_DETAILS);
    assert_eq!(bl_multi_model.state, PRIVATE_STATE);
    assert!(
        !bl_multi_model.details.contains("private"),
        "multi-session private card must not leak the blacklisted project: {:?}",
        bl_multi_model.details
    );
}

/// Single-session rendering of the adaptive card (the common case): the headline
/// reads "Working on {project} ({branch})", the metrics line is middot-separated
/// with no party suffix, and the live tool activity is preserved in small_text.
#[test]
fn single_session_card_uses_adaptive_strings() {
    let cfg = Config {
        plan_label: "Max 20x".to_string(),
        ..full_config(1)
    };
    let mut session = fixture("solo", "claude-presence", 1_781_989_000_000, 10_000);
    session.branch = Some("main".to_string());

    let mut aggregator = Aggregator::new(cfg);
    let PresenceUpdate::Activity(model) = aggregator.aggregate(vec![session]) else {
        panic!("expected an Activity for one live session");
    };

    assert_eq!(model.details, "Working on claude-presence (main)");
    assert!(model.state.starts_with("Opus 4.8"), "{}", model.state);
    assert!(model.state.contains('\u{b7}'), "{}", model.state);
    assert!(!model.state.contains('|'), "{}", model.state);
    // small_text keeps the live activity ("Editing main.rs") on hover.
    assert_eq!(model.small_text.as_deref(), Some("Editing main.rs"));
    // Single-session timer = the session's own start (not multi-session earliest).
    assert_eq!(model.started_at_ms, 1_781_989_000_000);
}
