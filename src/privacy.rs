//! Sanitizers: path → basename, blacklist matching, and bash-arg scrubbing (task 0.2).
//!
//! Privacy is non-negotiable (C-7 / ADR-8): only structured, sanitized fields may
//! leave the process — never raw prompt text, file contents, or full paths. These
//! helpers operate on borrowed primitives (paths, strings, slices) rather than the
//! [`crate::config`] model, so the two modules stay independently compilable; the
//! daemon wires the `[privacy]` settings into these calls.
//!
//! The bash-arg scrubber here provides working basics (env-assignment, credentialed
//! URL, `key=secret` and `Authorization`-style stripping, plus truncation); the full
//! pattern set may be extended by later tasks.
//!
//! These sanitizers are consumed by the aggregator and activity mapper.

use std::path::Path;

/// Placeholder substituted for a redacted secret in scrubbed bash args.
const REDACTED: &str = "[redacted]";

/// Max length of a scrubbed bash command before truncation (keeps `details` short).
const MAX_BASH_LEN: usize = 64;

/// Max length of a sanitized log fragment before truncation (longer than the card
/// limit since logs are local-only, but still bounded so blobs can't sprawl).
const MAX_LOG_LEN: usize = 256;

/// Generic label shown for a blacklisted (or fully redacted) project.
pub const GENERIC_PROJECT: &str = "a project";

/// Generic `details` line shown by the global private-mode card.
pub const PRIVATE_DETAILS: &str = "Working";
/// Generic `state` line shown by the global private-mode card.
pub const PRIVATE_STATE: &str = "Claude Code";

/// Reduce a path to its final component (basename), e.g. `/Users/x/private` →
/// `private`. Full paths must never reach Discord or the logs.
///
/// Falls back to the whole input (lossily) when there is no final component
/// (e.g. `/`), so the result is always non-empty for a non-empty path.
pub fn basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Whether `path` is under (or equal to) any entry in `blacklist`.
///
/// Matching is prefix-based on path components, so `/a/b` blacklists `/a/b/c`
/// but not `/a/bc`.
pub fn is_blacklisted<P: AsRef<Path>>(path: &Path, blacklist: &[P]) -> bool {
    blacklist
        .iter()
        .any(|entry| path.starts_with(entry.as_ref()))
}

/// Resolve the project label for a cwd, honoring the redaction switch and the
/// blacklist. Blacklisted (or fully redacted) projects collapse to a generic
/// label so nothing identifying leaks.
pub fn project_label<P: AsRef<Path>>(cwd: &Path, redact: bool, blacklist: &[P]) -> String {
    if redact || is_blacklisted(cwd, blacklist) {
        return GENERIC_PROJECT.to_string();
    }
    basename(cwd)
}

/// A generic, fully redacted card representation for the global "private mode".
///
/// When the daemon (or a session) is running in private mode, nothing identifying
/// may leave the process: no project, no activity target, no model, no metrics.
/// This struct carries only static, non-sensitive strings, so it is safe to push
/// to Discord verbatim. It is intentionally self-contained (it does not depend on
/// the [`crate::state::model`] types) so [`crate::privacy`] stays independently
/// compilable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateCard {
    /// Generic `details` line (never a project, path, or activity target).
    pub details: String,
    /// Generic `state` line (never a model, plan, or metrics).
    pub state: String,
}

impl Default for PrivateCard {
    fn default() -> Self {
        Self {
            details: PRIVATE_DETAILS.to_string(),
            state: PRIVATE_STATE.to_string(),
        }
    }
}

/// Build the global private-mode card: a generic, non-identifying presence.
///
/// Use this when redaction must hide everything (e.g. a global private switch or
/// a fully blacklisted focused session) — it guarantees no project, path,
/// activity, model, or metric leaks into the card.
pub fn private_card() -> PrivateCard {
    PrivateCard::default()
}

/// Gate the AI-generated session title (FR-7/AC-2).
///
/// The title only surfaces when the user has explicitly opted in
/// (`show_ai_title`) AND the project is not blacklisted. Even then it is routed
/// through [`redact_text`] so an embedded secret can never leak, and an
/// all-whitespace or now-empty title is suppressed. Returns `None` whenever the
/// title must be hidden.
pub fn ai_title<P: AsRef<Path>>(
    title: Option<&str>,
    show_ai_title: bool,
    cwd: &Path,
    blacklist: &[P],
) -> Option<String> {
    if !show_ai_title || is_blacklisted(cwd, blacklist) {
        return None;
    }
    let title = title?.trim();
    if title.is_empty() {
        return None;
    }
    let cleaned = redact_text(title);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// Whether a button URL is safe to emit on the PUBLIC card (FR-7/AC-2).
///
/// Only `https://` URLs pass. Anything else — notably `file://` (a local path
/// leak), `http://`, or a scheme-less string — is rejected. A `file://` URL must
/// NEVER reach Discord.
pub fn is_safe_button_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("https://") && url.len() > "https://".len()
}

/// Sanitize an arbitrary, possibly-sensitive string before it reaches a log line
/// (FR-8/AC-4, NFR-3): strip secrets, collapse credentialed URLs, and never let
/// raw `tool_input` / paths / JSON bodies through verbatim.
///
/// This applies the same per-token secret scrubbing as [`scrub_bash_command`]
/// (env-assignments, `key=secret`, `Authorization`, credentialed URLs, long
/// base64/hex blobs) across the whole string and truncates the result. Call sites
/// that log any user-derived text MUST run it through this first.
pub fn redact_text(text: &str) -> String {
    let scrubbed = text
        .split_whitespace()
        .map(scrub_token)
        .collect::<Vec<_>>()
        .join(" ");
    truncate(&scrubbed, MAX_LOG_LEN)
}

/// Scrub a single bash command for public display.
///
/// `scrub_bash_args` gates whether the command is shown at all: when `false`
/// (the default) the command is dropped entirely and `None` is returned. When
/// `true`, the command is stripped of obvious secrets — `WORD=value`
/// env-assignments, `--flag=secret` / `key: secret` pairs for sensitive keys,
/// credentialed URLs, and long base64/hex blobs — then truncated.
///
/// This is the working-basics scaffolding; the full pattern set may be extended
/// by later tasks. It never returns raw, unscrubbed argument text.
pub fn scrub_bash_command(command: &str, scrub_bash_args: bool) -> Option<String> {
    if !scrub_bash_args {
        return None;
    }

    let scrubbed = command
        .split_whitespace()
        .map(scrub_token)
        .collect::<Vec<_>>()
        .join(" ");

    Some(truncate(&scrubbed, MAX_BASH_LEN))
}

/// Scrub one whitespace-delimited token, replacing any secret-bearing value.
fn scrub_token(token: &str) -> String {
    if looks_like_secret_blob(token) {
        return REDACTED.to_string();
    }

    // `KEY=value` (env-assignment or `--flag=secret`): redact the value when the
    // key looks sensitive, otherwise keep it.
    if let Some((key, value)) = token.split_once('=') {
        if !value.is_empty() && (is_sensitive_key(key) || is_env_assignment(key)) {
            return format!("{key}={REDACTED}");
        }
    }

    // `key:value` (e.g. `Authorization:Bearer`) for a sensitive key — redact the
    // value but keep the key so the shape is still legible. Guard against `://`
    // so a credentialed/plain URL is not mistaken for a `key:value` pair (handled
    // below).
    if !token.contains("://") {
        if let Some((key, value)) = token.split_once(':') {
            if !value.is_empty() && is_sensitive_key(key) {
                return format!("{key}:{REDACTED}");
            }
        }
    }

    // Credentialed URL: `scheme://user:pass@host` → strip the credentials.
    if let Some(stripped) = strip_url_credentials(token) {
        return stripped;
    }

    token.to_string()
}

/// Whether a `KEY=value` key names a sensitive field (token/secret/password/etc.).
fn is_sensitive_key(key: &str) -> bool {
    let key = key.trim_start_matches('-').to_ascii_lowercase();
    const SENSITIVE: [&str; 7] = [
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "api_key",
        "apikey",
    ];
    SENSITIVE.iter().any(|needle| key.contains(needle))
}

/// Whether `key` is a bare uppercase env-var name (`FOO`, `AWS_SECRET`), which we
/// treat as a `WORD=value` env-assignment and redact defensively.
fn is_env_assignment(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key.chars().any(|c| c.is_ascii_uppercase())
}

/// Strip `user:pass@` credentials from a URL-like token; `None` if there are none.
fn strip_url_credentials(token: &str) -> Option<String> {
    let (scheme, rest) = token.split_once("://")?;
    let (creds, host) = rest.split_once('@')?;
    if creds.is_empty() || !creds.contains(':') {
        return None;
    }
    Some(format!("{scheme}://{REDACTED}@{host}"))
}

/// Heuristic: long base64/hex-ish blob that is likely a key or hash.
fn looks_like_secret_blob(token: &str) -> bool {
    token.len() >= 32
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == '-')
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn basename_is_last_component() {
        assert_eq!(basename(Path::new("/Users/x/private")), "private");
        assert_eq!(basename(Path::new("repo")), "repo");
    }

    #[test]
    fn blacklist_matches_by_component() {
        let blacklist = [PathBuf::from("/a/b")];
        assert!(is_blacklisted(Path::new("/a/b"), &blacklist));
        assert!(is_blacklisted(Path::new("/a/b/c"), &blacklist));
        assert!(!is_blacklisted(Path::new("/a/bc"), &blacklist));
        assert!(!is_blacklisted(Path::new("/x"), &blacklist));
    }

    #[test]
    fn project_label_honors_redact_and_blacklist() {
        let blacklist = [PathBuf::from("/secret")];
        let cwd = Path::new("/Users/x/private");
        // No redaction, not blacklisted → basename.
        assert_eq!(project_label(cwd, false, &blacklist), "private");
        // Redacting → generic.
        assert_eq!(project_label(cwd, true, &blacklist), GENERIC_PROJECT);
        // Blacklisted → generic even without the global switch.
        assert_eq!(
            project_label(Path::new("/secret/repo"), false, &blacklist),
            GENERIC_PROJECT
        );
    }

    #[test]
    fn bash_args_dropped_by_default() {
        assert_eq!(scrub_bash_command("rm -rf /", false), None);
    }

    #[test]
    fn bash_scrub_redacts_secrets_but_keeps_shape() {
        let out = scrub_bash_command("AWS_SECRET=abc123 curl host", true).unwrap();
        assert!(out.contains("AWS_SECRET=[redacted]"), "{out}");
        assert!(out.contains("curl"), "{out}");
        assert!(!out.contains("abc123"), "{out}");
    }

    #[test]
    fn bash_scrub_strips_url_credentials() {
        let out = scrub_bash_command("git clone https://user:pw@example.com/x", true).unwrap();
        assert!(out.contains("https://[redacted]@example.com/x"), "{out}");
        assert!(!out.contains("pw@"), "{out}");
    }

    #[test]
    fn bash_scrub_redacts_long_blob() {
        let blob = "A".repeat(40);
        let out = scrub_bash_command(&format!("echo {blob}"), true).unwrap();
        assert!(!out.contains(&blob), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
    }

    #[test]
    fn bash_scrub_truncates() {
        let long = "echo ".to_string() + &"word ".repeat(40);
        let out = scrub_bash_command(&long, true).unwrap();
        assert!(out.chars().count() <= MAX_BASH_LEN, "{out}");
    }

    #[test]
    fn bash_scrub_redacts_colon_separated_sensitive_value() {
        let out = scrub_bash_command("curl Authorization:Bearer-abc host", true).unwrap();
        assert!(out.contains("Authorization:[redacted]"), "{out}");
        assert!(!out.contains("Bearer-abc"), "{out}");
        assert!(out.contains("curl"), "{out}");
    }

    #[test]
    fn private_card_is_generic() {
        let card = private_card();
        assert_eq!(card.details, PRIVATE_DETAILS);
        assert_eq!(card.state, PRIVATE_STATE);
        // The generic card must not carry anything project- or path-shaped.
        assert!(!card.details.contains('/'));
        assert!(!card.state.contains('/'));
    }

    #[test]
    fn ai_title_gated_off_by_default() {
        let blacklist: [PathBuf; 0] = [];
        let cwd = Path::new("/Users/x/repo");
        // show_ai_title = false → suppressed regardless of the title.
        assert_eq!(
            ai_title(Some("Refactor auth"), false, cwd, &blacklist),
            None
        );
    }

    #[test]
    fn ai_title_shown_only_when_opted_in_and_not_blacklisted() {
        let blacklist = [PathBuf::from("/secret")];
        assert_eq!(
            ai_title(
                Some("Refactor auth"),
                true,
                Path::new("/Users/x/repo"),
                &blacklist
            ),
            Some("Refactor auth".to_string())
        );
        // Blacklisted project → suppressed even with the opt-in.
        assert_eq!(
            ai_title(
                Some("Refactor auth"),
                true,
                Path::new("/secret/repo"),
                &blacklist
            ),
            None
        );
    }

    #[test]
    fn ai_title_strips_embedded_secret() {
        let blacklist: [PathBuf; 0] = [];
        let title = ai_title(
            Some("token=sk-FAKE1234567890 work"),
            true,
            Path::new("/Users/x/repo"),
            &blacklist,
        )
        .unwrap();
        assert!(!title.contains("sk-FAKE"), "{title}");
        assert!(title.contains("work"), "{title}");
    }

    #[test]
    fn is_safe_button_url_only_https() {
        assert!(is_safe_button_url("https://example.com/repo"));
        assert!(!is_safe_button_url("file:///Users/me/private"));
        assert!(!is_safe_button_url("http://example.com"));
        assert!(!is_safe_button_url("https://")); // scheme only, no host
        assert!(!is_safe_button_url("ftp://example.com"));
        assert!(!is_safe_button_url(""));
    }

    #[test]
    fn redact_text_strips_secrets_and_paths_for_logs() {
        let raw = "running AWS_SECRET=abc123 git clone https://user:pw@host/x";
        let out = redact_text(raw);
        assert!(!out.contains("abc123"), "{out}");
        assert!(!out.contains("pw@"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
    }

    #[test]
    fn redact_text_truncates_long_input() {
        let long = "word ".repeat(200);
        assert!(redact_text(&long).chars().count() <= MAX_LOG_LEN);
    }
}
