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

use std::ffi::OsStr;
use std::path::{Component, Path};

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
/// Matching is prefix-based on path **components** (via [`path_matches_blacklist`]),
/// so `/a/b` blacklists `/a/b/c` but not `/a/bc`. On macOS each component is matched
/// case-insensitively (the APFS/HFS+ default), so a differently-cased path under a
/// blacklisted root still collapses to the generic label. The live `cwd` is matched
/// **without any filesystem syscall** (no `fs::canonicalize` on the hot path); any
/// `~`/symlink normalization is applied once at config-load to the blacklist
/// **entries** only — never to the cwd. The result is always a **superset** of the
/// old lexical `Path::starts_with` match.
pub fn is_blacklisted<P: AsRef<Path>>(path: &Path, blacklist: &[P]) -> bool {
    blacklist
        .iter()
        .any(|entry| path_matches_blacklist(path, entry.as_ref()))
}

/// Whether `cwd` is equal to, or nested under, the blacklist `entry`, by direct
/// **component-prefix** matching: `entry` must be a component-prefix of `cwd`.
///
/// Each [`OsStr`] component is compared for case-folded equality on macOS (exact
/// elsewhere), so the component-boundary rule (`/a/b` ∌ `/a/bc`) is preserved by
/// per-component equality — not by `str`/`Path::starts_with` on a joined string. No
/// `fs::canonicalize` is called on `cwd` (no per-match syscall, no symlink-resolution
/// regression); the `entry` is expected to be already `~`-expanded at config-load.
pub fn path_matches_blacklist(cwd: &Path, entry: &Path) -> bool {
    let mut entry_components = normal_components(entry);
    let mut cwd_components = normal_components(cwd);
    loop {
        match entry_components.next() {
            // `entry` is exhausted: it is a component-prefix of `cwd` → match.
            None => return true,
            Some(want) => match cwd_components.next() {
                // `cwd` is shorter than `entry` → not nested under it.
                None => return false,
                Some(have) if components_eq(want, have) => continue,
                Some(_) => return false,
            },
        }
    }
}

/// Iterator over the meaningful (`Normal`/`RootDir`/`Prefix`) components of a path,
/// matching how `Path::starts_with` walks components (so the new matcher is a
/// superset of the old lexical match).
fn normal_components(path: &Path) -> impl Iterator<Item = Component<'_>> {
    path.components()
        .filter(|c| !matches!(c, Component::CurDir))
}

/// Compare two path components for equality, case-folded on macOS (the
/// case-insensitive default of APFS/HFS+) and exact elsewhere.
fn components_eq(a: Component<'_>, b: Component<'_>) -> bool {
    let (a, b) = (a.as_os_str(), b.as_os_str());
    if cfg!(target_os = "macos") {
        os_str_eq_ignore_ascii_case(a, b)
    } else {
        a == b
    }
}

/// ASCII-case-insensitive equality over two [`OsStr`]s via their lossy UTF-8 view.
/// Used for macOS component folding; ASCII folding matches the common-case behavior
/// of the system volume and never narrows the lexical superset (an exact match always
/// folds equal).
fn os_str_eq_ignore_ascii_case(a: &OsStr, b: &OsStr) -> bool {
    if a == b {
        return true;
    }
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
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
/// through [`redact_text`], so an embedded secret can never leak and a full
/// filesystem path is reduced to its basename. An all-whitespace or now-empty
/// title is suppressed. Returns `None` whenever the title must be hidden.
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
/// (FR-1/AC-6, NFR-3): strip secrets, collapse credentialed URLs, reduce paths to
/// their basename, and never let raw `tool_input` / paths / JSON bodies through
/// verbatim. This is the sanitizer of record for user-derived log text.
///
/// This applies the same per-token scrubbing as [`scrub_bash_command`] via
/// [`scrub_token`] across the whole string — known-secret formats (GitHub/OpenAI/
/// Slack/AWS/Google keys, JWTs) and long base64/hex blobs, `key=secret`/
/// `key:secret` pairs, env-assignments, credentialed URLs + sensitive query params,
/// and finally a filesystem-path token reduced to its basename — then truncates the
/// result. Call sites that log any user-derived text MUST run it through this first.
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
///
/// Stages run in a fixed order so the path-basename stage can never mangle a
/// credentialed URL or a `KEY=/secret/path`:
/// 1. known-secret format / long base64-hex blob → `[redacted]`,
/// 2. `key=value` / `key:value` for a sensitive key (non-URL tokens) → value
///    `[redacted]`,
/// 3. credentialed-URL credential strip + sensitive query-param redaction,
/// 4. **last**: an unambiguous filesystem path → its basename.
fn scrub_token(token: &str) -> String {
    if looks_like_secret_blob(token) || looks_like_known_secret(token) {
        return REDACTED.to_string();
    }

    // A URL (`scheme://…`) is owned by the dedicated URL stage below: skip the
    // `key=value`/`key:value` branches so a query param's `=` (e.g.
    // `?access_token=…`) is not greedily redacted at the wrong boundary.
    if !token.contains("://") {
        // `KEY=value` (env-assignment or `--flag=secret`): redact the value when the
        // key looks sensitive, otherwise keep it.
        if let Some((key, value)) = token.split_once('=') {
            if !value.is_empty() && (is_sensitive_key(key) || is_env_assignment(key)) {
                return format!("{key}={REDACTED}");
            }
        }

        // `key:value` (e.g. `Authorization:Bearer`) for a sensitive key — redact the
        // value but keep the key so the shape is still legible.
        if let Some((key, value)) = token.split_once(':') {
            if !value.is_empty() && is_sensitive_key(key) {
                return format!("{key}:{REDACTED}");
            }
        }
    }

    // A URL (`scheme://…`): strip any `user:pass@` credentials and redact the value
    // of any sensitive query parameter. A URL is never reduced to a basename.
    if token.contains("://") {
        return scrub_url(token);
    }

    // LAST stage: an unambiguous filesystem path is reduced to its basename,
    // mirroring `claude::activity::path_target`. Runs only after the secret/URL
    // stages so a credentialed URL is credential-stripped (not basename'd) and a
    // `KEY=/secret/path` is redacted by the `key=value` branch first.
    if is_unambiguous_path(token) {
        return basename(Path::new(token));
    }

    token.to_string()
}

/// Whether `token` is unambiguously a filesystem path that should be reduced to its
/// basename: a leading `/` or `~`, or it contains `/` and is not a `scheme://…` URL,
/// not a `key=value`/`key:value` pair, and not a known MIME type / scheme.
fn is_unambiguous_path(token: &str) -> bool {
    if token.starts_with('/') || token.starts_with('~') {
        return true;
    }
    if !token.contains('/') {
        return false;
    }
    // A non-rooted token must not be a URL, a `key=value`/`key:value` pair, or a MIME
    // type to be path-shaped — so `application/json` passes through.
    if token.contains("://") || token.contains('=') || token.contains(':') || is_mime_type(token) {
        return false;
    }
    // A relative token (no leading `/`/`~`) is ambiguous: a bare `>=2 slashes` rule
    // mangles regex/sed/word tokens (`s/foo/bar/g`, `and/or/maybe`, `TODO/FIXME/done`).
    // Treat it as a path ONLY when its last segment carries a file extension
    // (`src/main.rs`, `a/b/c.rs`), so non-path multi-slash tokens pass through
    // unmangled.
    last_segment_has_extension(token)
}

/// Whether the final `/`-segment of a relative token carries a file extension (a `.`
/// with non-empty name and extension parts, not a leading/trailing dot).
fn last_segment_has_extension(token: &str) -> bool {
    let last = token.rsplit('/').next().unwrap_or(token);
    match last.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && !ext.is_empty(),
        None => false,
    }
}

/// Whether `token` looks like a `type/subtype` MIME type (e.g. `application/json`),
/// which must NOT be basename'd. Conservative: a single `/`, both sides non-empty
/// and made of MIME-token characters, and a recognized top-level type.
fn is_mime_type(token: &str) -> bool {
    let Some((top, sub)) = token.split_once('/') else {
        return false;
    };
    if top.is_empty() || sub.is_empty() || sub.contains('/') {
        return false;
    }
    const MIME_TOPS: [&str; 9] = [
        "application",
        "audio",
        "font",
        "image",
        "message",
        "model",
        "multipart",
        "text",
        "video",
    ];
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_');
    MIME_TOPS.contains(&top.to_ascii_lowercase().as_str()) && sub.chars().all(is_token_char)
}

/// Whether a `KEY=value` key names a sensitive field (token/secret/password/etc.).
///
/// Normalizes `-`→`_` (after lowercasing and stripping leading `-`) so hyphenated
/// header keys (`x-api-key`, `api-key`) match the `api_key` needle.
fn is_sensitive_key(key: &str) -> bool {
    let key = key
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('-', "_");
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

/// Sensitive URL query-parameter names whose value is replaced with `[redacted]`.
/// Matched after `-`→`_` normalization via [`is_sensitive_key`], plus this explicit
/// short-name set (`key`/`sig`/`signature`/`access_token`) the dossier F30 names.
fn is_sensitive_query_key(key: &str) -> bool {
    const SENSITIVE_QUERY: [&str; 4] = ["key", "sig", "signature", "access_token"];
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    SENSITIVE_QUERY.contains(&normalized.as_str()) || is_sensitive_key(key)
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

/// Scrub a `scheme://…` URL token: strip any `user[:pass]@` userinfo (a bare
/// token-as-userinfo with no colon is a credential too), redact the value of any
/// sensitive query parameter, AND redact any query/fragment value that *looks like* a
/// secret regardless of its parameter name. The path is left untouched (a URL is never
/// reduced to a basename).
fn scrub_url(token: &str) -> String {
    let Some((scheme, rest)) = token.split_once("://") else {
        return token.to_string();
    };

    // Peel the `#fragment` off FIRST (before the `?` split) so an OAuth-style
    // `#access_token=…` fragment is scrubbed and not mistaken for part of the query.
    let (rest, fragment) = match rest.split_once('#') {
        Some((head, frag)) => (head, Some(frag)),
        None => (rest, None),
    };

    // Split the authority+path from the query string (everything after the first `?`).
    let (authority_path, query) = match rest.split_once('?') {
        Some((head, q)) => (head, Some(q)),
        None => (rest, None),
    };

    // Strip `user[:pass]@` userinfo from the authority whenever it is non-empty — a
    // bare token-as-userinfo (`ghp_…@host`, no colon) is just as much a credential as
    // `user:pass@host`, so redact the whole userinfo unconditionally.
    let authority_path = match authority_path.split_once('@') {
        Some((creds, host)) if !creds.is_empty() => format!("{REDACTED}@{host}"),
        _ => authority_path.to_string(),
    };

    let mut out = format!("{scheme}://{authority_path}");
    if let Some(query) = query {
        out.push('?');
        out.push_str(&scrub_url_params(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(&scrub_url_params(fragment));
    }
    out
}

/// Scrub the `k=v[&k=v…]` pairs of a URL query string or fragment: redact the value
/// when the key is sensitive OR the value itself looks like a secret (so a bare token
/// is caught regardless of its parameter name), preserving the `&` shape.
fn scrub_url_params(params: &str) -> String {
    params
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value))
                if !value.is_empty()
                    && (is_sensitive_query_key(key)
                        || looks_like_secret_blob(value)
                        || looks_like_known_secret(value)) =>
            {
                format!("{key}={REDACTED}")
            }
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Heuristic: long base64/hex-ish blob that is likely a key or hash. The charset
/// includes `_` and `.` so URL-safe base64 and dotted blobs (e.g. JWT segments)
/// are caught.
///
/// A path-shaped token containing `/` is NOT treated as a blob unless it also carries
/// the `+`/`=` markers of standard/padded base64 — so a long absolute path like
/// `/Users/x/Projects/private/auth.rs` falls through to the basename stage instead of
/// being redacted (real base64/hex blobs and JWTs are still caught: JWTs by
/// `looks_like_known_secret`, padded/standard base64 by the `+`/`=` markers here).
fn looks_like_secret_blob(token: &str) -> bool {
    if token.len() < 32 {
        return false;
    }
    let charset_ok = token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_' | '.'));
    if !charset_ok {
        return false;
    }
    // A `/` makes the token path-shaped; only classify it as a blob when standard/
    // padded base64 markers (`+`/`=`) are also present.
    if token.contains('/') && !(token.contains('+') || token.contains('=')) {
        return false;
    }
    true
}

/// Length-independent detection of common real-world secret formats by their known
/// prefix (anchored at token start) or JWT shape, so a short-but-real key is caught
/// even when `looks_like_secret_blob` would miss it (FR-1/AC-3).
fn looks_like_known_secret(token: &str) -> bool {
    // Known provider prefixes, anchored at the token start to bound false positives.
    const KNOWN_PREFIXES: [&str; 16] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk-proj-",
        "sk-",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "AKIA",
        "ASIA",
        "AIza",
    ];
    if KNOWN_PREFIXES
        .iter()
        .any(|prefix| token.starts_with(prefix))
    {
        return true;
    }

    looks_like_jwt(token)
}

/// JWT shape: three `.`-separated base64url segments, the first starting `eyJ`.
fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    header.starts_with("eyJ")
        && [header, payload, signature]
            .iter()
            .all(|seg| !seg.is_empty() && seg.chars().all(is_base64url_char))
}

/// Whether `c` is a base64url character (`A-Za-z0-9-_`) or padding (`=`).
fn is_base64url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=')
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
        let raw =
            "running AWS_SECRET=abc123 git clone https://user:pw@host/x /Users/x/secret/auth.rs";
        let out = redact_text(raw);
        assert!(!out.contains("abc123"), "{out}");
        assert!(!out.contains("pw@"), "{out}");
        assert!(out.contains("[redacted]"), "{out}");
        // A full filesystem path is reduced to its basename (no `/`-prefixed
        // component leaks into the log).
        assert!(out.contains("auth.rs"), "{out}");
        assert!(!out.contains("/Users/x/secret"), "{out}");
        // The credentialed URL keeps its path (it is credential-stripped, not
        // basename'd).
        assert!(out.contains("https://[redacted]@host/x"), "{out}");
    }

    #[test]
    fn redact_text_reduces_full_path_to_basename() {
        let out = redact_text("Refactoring /Users/x/secret/auth.rs");
        // No `/`-prefixed path component remains.
        assert!(!out.contains('/'), "{out}");
        assert!(out.contains("auth.rs"), "{out}");
        assert!(out.contains("Refactoring"), "{out}");
    }

    #[test]
    fn redact_text_strips_tilde_and_relative_paths() {
        let out = redact_text("edit ~/Projects/private/key.pem and src/main.rs");
        assert!(out.contains("key.pem"), "{out}");
        assert!(out.contains("main.rs"), "{out}");
        assert!(!out.contains("Projects"), "{out}");
        assert!(!out.contains("private"), "{out}");
        assert!(!out.contains("src/main.rs"), "{out}");
    }

    #[test]
    fn credentialed_url_is_credential_stripped_not_basenamed() {
        // The URL keeps its path; only the credentials are stripped.
        let out = scrub_token("https://user:pw@host/x/secret.rs");
        assert_eq!(out, "https://[redacted]@host/x/secret.rs");
    }

    #[test]
    fn sensitive_url_query_params_are_redacted() {
        let out = scrub_token("https://api/v1?access_token=sk-SECRET&sig=abc&page=2");
        assert!(out.contains("access_token=[redacted]"), "{out}");
        assert!(out.contains("sig=[redacted]"), "{out}");
        assert!(!out.contains("sk-SECRET"), "{out}");
        assert!(!out.contains("=abc"), "{out}");
        // A non-sensitive param is preserved.
        assert!(out.contains("page=2"), "{out}");
    }

    #[test]
    fn mime_type_and_option_token_pass_through() {
        // A MIME type and an `a/b`-style option/regex token are not basename'd.
        assert_eq!(scrub_token("application/json"), "application/json");
        assert_eq!(scrub_token("text/html"), "text/html");
        assert_eq!(scrub_token("a/b"), "a/b");
        // A plain word with no `/` is untouched.
        assert_eq!(scrub_token("reading"), "reading");
        // A public (no-credential) https URL is not basename'd.
        assert_eq!(
            scrub_token("https://github.com/foo/bar"),
            "https://github.com/foo/bar"
        );
    }

    #[test]
    fn url_strips_token_as_userinfo_with_no_colon() {
        // A bare token-as-userinfo (no `:`) is a credential too and must be stripped.
        let out = scrub_token("https://ghp_SECRETTOKEN123@github.com/x");
        assert!(!out.contains("ghp_SECRETTOKEN123"), "{out}");
        assert!(out.contains("[redacted]@github.com/x"), "{out}");
    }

    #[test]
    fn url_redacts_oauth_fragment_token() {
        // An OAuth-style `#access_token=…` fragment must be redacted (by name) and a
        // bare-secret fragment value redacted regardless of its key name.
        let out = scrub_token("https://app/cb#access_token=SECRETvalue123&state=ok");
        assert!(!out.contains("SECRETvalue123"), "{out}");
        assert!(out.contains("access_token=[redacted]"), "{out}");
        assert!(out.contains("state=ok"), "{out}");
    }

    #[test]
    fn url_redacts_bare_secret_value_by_shape_not_name() {
        // A query/fragment value that *looks like* a secret is redacted even when its
        // parameter name is not in the sensitive set.
        let out = scrub_token("https://api/v1?ref=ghp_0123456789ABCDEFabcdef&page=2");
        assert!(!out.contains("ghp_0123456789ABCDEFabcdef"), "{out}");
        assert!(out.contains("ref=[redacted]"), "{out}");
        assert!(out.contains("page=2"), "{out}");
    }

    #[test]
    fn non_path_multislash_tokens_pass_through_unmangled() {
        // Regex/sed/word tokens with multiple slashes must NOT be basename'd.
        assert_eq!(scrub_token("s/foo/bar/g"), "s/foo/bar/g");
        assert_eq!(scrub_token("and/or/maybe"), "and/or/maybe");
        assert_eq!(scrub_token("TODO/FIXME/done"), "TODO/FIXME/done");
        // A relative token whose last segment has an extension still reduces.
        assert_eq!(scrub_token("src/main.rs"), "main.rs");
        assert_eq!(scrub_token("a/b/c.rs"), "c.rs");
    }

    #[test]
    fn long_absolute_path_reduces_to_basename_not_redacted() {
        // A long absolute path (no colon, no base64 markers) must basename, not redact.
        let out = scrub_token("/Users/jakub/Projects/private/auth.rs");
        assert_eq!(out, "auth.rs");
    }

    #[test]
    fn known_secret_formats_are_redacted() {
        // GitHub PAT.
        assert_eq!(scrub_token("ghp_0123456789ABCDEFabcdef"), REDACTED);
        assert_eq!(scrub_token("github_pat_11ABCDEFG0abc"), REDACTED);
        // OpenAI keys.
        assert_eq!(scrub_token("sk-proj-abcDEF123456"), REDACTED);
        assert_eq!(scrub_token("sk-abcDEF123456"), REDACTED);
        // Slack.
        assert_eq!(scrub_token("xoxb-123-456-abcDEF"), REDACTED);
        // AWS access-key id (20 chars).
        assert_eq!(scrub_token("AKIAIOSFODNN7EXAMPLE"), REDACTED);
        // Google API key.
        assert_eq!(scrub_token("AIzaSyA-1234567890abcDEF"), REDACTED);
        // JWT shape (three base64url segments, first starting `eyJ`).
        assert_eq!(
            scrub_token("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc-DEF_123"),
            REDACTED
        );
    }

    #[test]
    fn ordinary_tokens_are_not_false_positives() {
        // Tokens that resemble — but are not — a known secret must survive.
        assert_eq!(scrub_token("skipping"), "skipping");
        assert_eq!(scrub_token("asia-pacific"), "asia-pacific");
        // A normal three-dot token without the `eyJ` header is not a JWT.
        assert_eq!(scrub_token("a.b.c"), "a.b.c");
    }

    #[test]
    fn hyphenated_sensitive_header_keys_are_redacted() {
        // `-`→`_` normalization means hyphenated header keys still redact.
        let out = scrub_token("x-api-key=verysecretvalue");
        assert_eq!(out, "x-api-key=[redacted]");
    }

    #[test]
    fn blacklist_matches_case_variants_and_preserves_boundary() {
        let blacklist = [PathBuf::from("/Users/X/Private")];
        // A differently-cased path under the blacklisted root collapses to generic
        // (macOS case-insensitive); elsewhere the exact case still matches.
        if cfg!(target_os = "macos") {
            assert!(is_blacklisted(
                Path::new("/users/x/private/secret.rs"),
                &blacklist
            ));
        }
        // The exact-case path always matches (superset of today's lexical match).
        assert!(is_blacklisted(
            Path::new("/Users/X/Private/secret.rs"),
            &blacklist
        ));
        // Component boundary preserved: `/a/b` ∌ `/a/bc`.
        let bl2 = [PathBuf::from("/a/b")];
        assert!(is_blacklisted(Path::new("/a/b"), &bl2));
        assert!(is_blacklisted(Path::new("/a/b/c"), &bl2));
        assert!(!is_blacklisted(Path::new("/a/bc"), &bl2));
    }

    #[test]
    fn path_matches_blacklist_unit() {
        // Equal or nested → match; sibling/shorter → no match.
        assert!(path_matches_blacklist(
            Path::new("/a/b/c"),
            Path::new("/a/b")
        ));
        assert!(path_matches_blacklist(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!path_matches_blacklist(
            Path::new("/a/bc"),
            Path::new("/a/b")
        ));
        assert!(!path_matches_blacklist(Path::new("/a"), Path::new("/a/b")));
        // A `.` component in the cwd is ignored (matches `Path::starts_with`).
        assert!(path_matches_blacklist(
            Path::new("/a/./b/c"),
            Path::new("/a/b")
        ));
    }

    #[test]
    fn redact_text_truncates_long_input() {
        let long = "word ".repeat(200);
        assert!(redact_text(&long).chars().count() <= MAX_LOG_LEN);
    }
}
