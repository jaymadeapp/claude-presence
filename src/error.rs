//! Crate-wide error type (CLAUDE.md mandates `thiserror`).
//!
//! A single `Error` enum is used across the daemon so call sites can bubble
//! failures with `?` and a shared [`Result`] alias. Variants carry only
//! categorical context — never raw prompt/transcript text, full paths, or
//! statusline payloads (FR-8/AC-4, NFR-3); see `logging.rs` for the sink-side
//! guarantee and `privacy.rs` for the sanitizers call sites must use before
//! including any user-derived string in an error message.

use thiserror::Error;

/// Errors raised anywhere in the daemon.
///
/// Variants that wrap a foreign error via `#[from]` deliberately surface only
/// that error's own `Display`; callers MUST NOT format raw user data (commands,
/// prompts, paths, statusline JSON) into the `String`-carrying variants.
///
/// Several variants are constructed only by later tasks (config, ingest,
/// schema, discord, lifecycle), so the enum carries a `dead_code` allow until
/// those call sites land — mirroring the skeleton convention in `config.rs`.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum Error {
    /// An I/O operation failed (filesystem, sockets, process probes).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization of a transcript line, sessions registry,
    /// statusline, or hook payload failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML deserialization of the config file failed.
    #[error("toml parse error: {0}")]
    TomlDe(#[from] toml::de::Error),

    /// TOML serialization (e.g. writing a default config) failed.
    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),

    /// Configuration was missing, invalid, or internally inconsistent.
    ///
    /// The message MUST describe the problem categorically (which field / why),
    /// never echo secret config values.
    #[error("config error: {0}")]
    Config(String),

    /// Talking to Discord over local IPC failed (handshake, send, or socket
    /// probe). Wraps the `discord-rich-presence` error type.
    #[error("discord ipc error: {0}")]
    Discord(#[from] discord_rich_presence::error::Error),

    /// The daemon ingest socket (statusline/hook forwarder) failed: bind,
    /// accept, peer-uid check, or a malformed frame.
    #[error("ingest error: {0}")]
    Ingest(String),

    /// Another live instance already holds the single-instance lock
    /// (two writers would breach Discord's 5/20s rate limit — FR-8/AC-1).
    #[error("another claude-presence instance is already running")]
    AlreadyRunning,

    /// A required well-known path (home, state dir, `~/.claude`, `$TMPDIR`)
    /// could not be resolved on this platform.
    #[error("could not resolve {0} directory")]
    PathResolution(&'static str),

    /// A `~/.claude` internal schema was unrecognized or changed shape; the
    /// caller should degrade to a reduced card rather than panic (ADR-5).
    #[error("unsupported claude schema: {0}")]
    Schema(String),

    /// A catch-all for not-yet-categorized failures.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
