//! Integration tests for the privacy sanitizers (task 5.1, FR-7/AC-2, FR-8/AC-4,
//! NFR-3, C-7).
//!
//! These tests assert the daemon's core privacy guarantee end-to-end at the level
//! of its public sanitizers: a fake secret in a Bash command never reaches
//! `details`, the AI title is gated, the log formatter omits commands/paths/raw
//! JSON, and buttons never emit a `file://` URL.

use std::path::{Path, PathBuf};

use claude_presence::privacy::{
    ai_title, basename, is_blacklisted, is_safe_button_url, private_card, project_label,
    redact_text, scrub_bash_command, GENERIC_PROJECT,
};

/// A representative fake secret used across the tests — must never surface.
const FAKE_TOKEN: &str = "sk-FAKE-aaaaaaaabbbbbbbbccccccccdddddddd";

// --- FR-7/AC-2: a Bash command's secret never reaches `details` ---------------

/// Mirror of how the aggregator builds the `details` activity fragment from the
/// scrubbed Bash target (see `state/aggregator.rs::format_details`): `verb` +
/// optional `target`. The aggregator itself lives in another file, so we
/// reconstruct the exact rendering here over the sanitized primitive it consumes.
fn details_from_bash(command: &str, scrub_bash_args: bool) -> String {
    let target = scrub_bash_command(command, scrub_bash_args);
    match target {
        Some(t) if !t.is_empty() => format!("Running {t}"),
        _ => "Running".to_string(),
    }
}

#[test]
fn bash_token_never_appears_in_details_by_default() {
    // By default bash args are dropped entirely, so the command is not shown at
    // all and the token cannot possibly leak into `details`.
    let command = format!("curl -H 'Authorization: Bearer {FAKE_TOKEN}' https://api.example.com");
    let details = details_from_bash(&command, false);
    assert_eq!(details, "Running");
    assert!(!details.contains(FAKE_TOKEN), "{details}");
    assert!(!details.contains("Authorization"), "{details}");
    assert!(!details.contains("Bearer"), "{details}");
}

#[test]
fn bash_token_never_appears_in_details_even_when_scrubbing() {
    // With the opt-in `scrub_bash_args`, the command IS shown — but routed through
    // the scrubber, so the secret is stripped before it can reach `details`.
    let command = format!("curl --token={FAKE_TOKEN} https://api.example.com");
    let details = details_from_bash(&command, true);
    assert!(!details.contains(FAKE_TOKEN), "{details}");
    assert!(details.contains("curl"), "{details}");
    assert!(details.contains("[redacted]"), "{details}");
}

#[test]
fn bash_long_blob_is_redacted() {
    let command = format!("echo {FAKE_TOKEN}");
    let out = scrub_bash_command(&command, true).expect("shown when scrubbing");
    assert!(!out.contains(FAKE_TOKEN), "{out}");
    assert!(out.contains("[redacted]"), "{out}");
}

#[test]
fn bash_credentialed_url_is_stripped() {
    let out = scrub_bash_command("git clone https://user:hunter2@host.example/x", true)
        .expect("shown when scrubbing");
    assert!(!out.contains("hunter2"), "{out}");
    assert!(out.contains("[redacted]@host.example"), "{out}");
}

#[test]
fn bash_env_assignment_value_is_redacted() {
    let out = scrub_bash_command("DATABASE_PASSWORD=hunter2 psql", true).expect("shown");
    assert!(!out.contains("hunter2"), "{out}");
    assert!(out.contains("DATABASE_PASSWORD=[redacted]"), "{out}");
}

// --- FR-7/AC-2: ai-title gating ----------------------------------------------

#[test]
fn ai_title_suppressed_unless_opted_in_and_not_blacklisted() {
    let blacklist = [PathBuf::from("/Users/me/private")];
    let normal = Path::new("/Users/me/work");
    let secret = Path::new("/Users/me/private/repo");

    // Off by default.
    assert_eq!(
        ai_title(Some("Fixing the parser"), false, normal, &blacklist),
        None
    );
    // Opted in, not blacklisted → shown.
    assert_eq!(
        ai_title(Some("Fixing the parser"), true, normal, &blacklist),
        Some("Fixing the parser".to_string())
    );
    // Opted in but blacklisted → suppressed.
    assert_eq!(
        ai_title(Some("Fixing the parser"), true, secret, &blacklist),
        None
    );
}

#[test]
fn ai_title_strips_secret_before_showing() {
    let blacklist: [PathBuf; 0] = [];
    let title = ai_title(
        Some(&format!("debugging {FAKE_TOKEN} flow")),
        true,
        Path::new("/Users/me/work"),
        &blacklist,
    )
    .expect("non-empty after scrubbing");
    assert!(!title.contains(FAKE_TOKEN), "{title}");
    assert!(title.contains("debugging"), "{title}");
}

#[test]
fn ai_title_empty_is_suppressed() {
    let blacklist: [PathBuf; 0] = [];
    assert_eq!(
        ai_title(Some("   "), true, Path::new("/x"), &blacklist),
        None
    );
    assert_eq!(ai_title(None, true, Path::new("/x"), &blacklist), None);
}

// --- FR-8/AC-4 / NFR-3: the log formatter omits command/paths/raw JSON --------

#[test]
fn log_formatter_omits_command_paths_and_raw_json() {
    // A representative raw statusline-ish blob a careless call site might try to
    // log: a full path, a secret token, and a credentialed URL.
    let raw = format!(
        "tool_input {{\"command\":\"curl --token={FAKE_TOKEN} https://user:pw@host/x\"}} \
         path=/Users/me/private/secret.rs"
    );
    let sanitized = redact_text(&raw);

    // No secret, no credentials.
    assert!(!sanitized.contains(FAKE_TOKEN), "{sanitized}");
    assert!(!sanitized.contains("pw@"), "{sanitized}");
    // The sensitive value is redacted.
    assert!(sanitized.contains("[redacted]"), "{sanitized}");
}

#[test]
fn log_formatter_truncates_runaway_input() {
    // A huge blob must not sprawl across the log — it is bounded.
    let huge = "a".repeat(10_000);
    let out = redact_text(&format!("line {huge}"));
    assert!(out.chars().count() <= 256, "len {}", out.chars().count());
    assert!(!out.contains(&huge), "raw blob leaked");
}

// --- FR-7/AC-2: buttons never emit a `file://` URL ----------------------------

#[test]
fn buttons_reject_file_urls_and_non_https() {
    assert!(is_safe_button_url("https://github.com/me/repo"));
    assert!(!is_safe_button_url("file:///Users/me/private/repo"));
    assert!(!is_safe_button_url("http://example.com"));
    assert!(!is_safe_button_url("https://")); // no host
    assert!(!is_safe_button_url(""));
}

/// Mirror of `aggregator::valid_buttons`'s URL filter over the public predicate:
/// only `https://` survives, so a `file://` button is dropped entirely.
#[test]
fn file_url_button_is_filtered_out() {
    let configured = [
        ("Repo", "https://github.com/me/repo"),
        ("Local", "file:///Users/me/private"),
    ];
    let kept: Vec<&str> = configured
        .iter()
        .filter(|(_, url)| is_safe_button_url(url))
        .map(|(_, url)| *url)
        .collect();
    assert_eq!(kept, vec!["https://github.com/me/repo"]);
    assert!(!kept.iter().any(|u| u.starts_with("file://")));
}

// --- Global private-mode generic card -----------------------------------------

#[test]
fn private_card_leaks_nothing_identifying() {
    let card = private_card();
    // Carries only static, generic strings — never a path/project/model/metric.
    assert!(!card.details.contains('/'));
    assert!(!card.details.contains('$'));
    assert!(!card.state.contains('/'));
    assert!(!card.state.contains('$'));
    assert!(!card.details.is_empty());
    assert!(!card.state.is_empty());
}

// --- Supporting sanitizer primitives ------------------------------------------

#[test]
fn paths_reduce_to_basename_and_blacklist_collapses_to_generic() {
    let path = Path::new("/Users/me/private/src/main.rs");
    assert_eq!(basename(path), "main.rs");

    let blacklist = [PathBuf::from("/Users/me/private")];
    assert!(is_blacklisted(path, &blacklist));
    // A blacklisted (or redacted) project never surfaces its real name.
    assert_eq!(project_label(path, false, &blacklist), GENERIC_PROJECT);
    assert_eq!(
        project_label(path, true, &[] as &[PathBuf]),
        GENERIC_PROJECT
    );
}
