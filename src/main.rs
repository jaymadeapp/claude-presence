//! `claude-presence` binary entry point.
//!
//! A thin clap dispatcher over the [`claude_presence`] library. Module wiring
//! lives in `src/lib.rs`; this file owns only the CLI surface and routes each
//! subcommand to its library implementation. `run` is the real daemon
//! ([`claude_presence::run`]); the rest remain stubs filled by later tasks.

use clap::{Parser, Subcommand};

use claude_presence::config::Config;
use claude_presence::error::{Error, Result};
use claude_presence::install::{hooks, launchd, statusline};
use claude_presence::platform::macos;
use claude_presence::{acquire_single_instance_lock, claude::sessions};

/// Aggregate live Claude Code activity into a single Discord Rich Presence.
#[derive(Debug, Parser)]
#[command(name = "claude-presence", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground.
    Run,
    /// Install the launchd agent + chained hooks + statusline wrapper (reversible).
    Install {
        /// Hide which project you're working in (privacy.fields.project=false)
        #[arg(long)]
        hide_project: bool,
        /// Show the project name (overrides the interactive default)
        #[arg(long, conflicts_with = "hide_project")]
        show_project: bool,
        /// Hide the running command in the small-icon tooltip (privacy.fields.command=false)
        #[arg(long)]
        hide_command: bool,
        /// Show the running command (overrides the interactive default)
        #[arg(long, conflicts_with = "hide_command")]
        show_command: bool,
        /// Global private mode: hide everything (privacy.redact=true)
        #[arg(long)]
        private: bool,
        /// Don't prompt; use flag values / privacy-preserving defaults (for brew/CI)
        #[arg(long, short = 'y', visible_alias = "non-interactive")]
        yes: bool,
    },
    /// Fully revert everything `install` set up.
    Uninstall,
    /// Re-enable the daemon (load the launchd agent).
    #[command(visible_alias = "on")]
    Enable,
    /// Disable the daemon and clear the Discord presence (unload the launchd agent).
    #[command(visible_alias = "off")]
    Disable,
    /// Show detected sessions and the Discord connection state.
    Status,
    /// Diagnose Discord socket, sessions, settings wiring, and instance conflicts.
    Doctor,
    /// Internal: pipe a hook/statusline event (stdin JSON) to the daemon socket.
    ///
    /// Used by the chained shell scripts; not part of the user-facing surface.
    #[command(hide = true)]
    Forward {
        /// Event kind being forwarded.
        #[arg(long, value_enum)]
        kind: ForwardKind,
    },
}

/// The kind of event a `forward` invocation carries.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum ForwardKind {
    Hook,
    Statusline,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run => claude_presence::run().await,
        Command::Install {
            hide_project,
            show_project,
            hide_command,
            show_command,
            private,
            yes,
        } => install(InstallOpts {
            hide_project,
            show_project,
            hide_command,
            show_command,
            private,
            yes,
        }),
        Command::Uninstall => uninstall(),
        Command::Enable => enable(),
        Command::Disable => disable(),
        Command::Status => status(),
        Command::Doctor => doctor(),
        Command::Forward { kind } => forward(kind),
    }
}

/// Resolved install-time privacy choices from the CLI flags.
struct InstallOpts {
    hide_project: bool,
    show_project: bool,
    hide_command: bool,
    show_command: bool,
    private: bool,
    yes: bool,
}

/// Compose the three reversible installers into one `install` (FR-8/AC-2).
///
/// Order matters: write the statusline wrapper, then chain the hooks, then
/// `bootstrap` the launchd agent **last** so the daemon only starts once all of
/// its wiring (the socket the chained scripts forward into) exists. Each sub-
/// installer is idempotent, so re-running `install` is safe.
///
/// On any step failing, the steps already applied are rolled back best-effort
/// (in reverse order) so a partial install never leaves the user half-wired
/// (NFR-6); anything that cannot be reverted is logged, not swallowed silently.
fn install(opts: InstallOpts) -> Result<()> {
    println!("Installing claude-presence…");

    // Resolve and persist the user's privacy choices BEFORE bootstrapping launchd:
    // the config has no hot reload, so the daemon must find these on first start.
    // This writes user data; it is intentionally NOT rolled back on a later failure.
    resolve_and_save_privacy(&opts)?;

    statusline::install()?;
    println!("  [ok] statusline wrapper chained");

    if let Err(err) = hooks::install() {
        eprintln!("  [fail] hooks: {err}");
        rollback_statusline();
        return Err(err);
    }
    println!("  [ok] lifecycle hooks chained");

    if let Err(err) = launchd::install() {
        eprintln!("  [fail] launchd agent: {err}");
        rollback_hooks();
        rollback_statusline();
        return Err(err);
    }
    println!("  [ok] launchd agent bootstrapped (daemon started)");

    println!("Install complete. Run `claude-presence doctor` to verify.");
    Ok(())
}

/// Resolve the install-time privacy choices (flags → interactive prompt →
/// privacy-preserving defaults) and persist them into the config file.
///
/// Precedence per axis: an explicit `--show-*`/`--hide-*` flag wins; otherwise,
/// when interactive (stdin is a TTY and `--yes` was not passed) we prompt with a
/// default of HIDE; otherwise (non-interactive, no flag) we default to HIDE.
/// `--private` additionally enables global redaction. The config is loaded,
/// updated, and written via [`Config::save`] (user data — never rolled back).
fn resolve_and_save_privacy(opts: &InstallOpts) -> Result<()> {
    use std::io::IsTerminal;

    let interactive = std::io::stdin().is_terminal() && !opts.yes;
    let mut cfg = Config::load();

    if opts.private {
        cfg.privacy.redact = true;
    }

    // Project axis: explicit flag → prompt (default hide) → default hide.
    cfg.privacy.fields.project = if opts.show_project {
        true
    } else if opts.hide_project {
        false
    } else if interactive {
        !prompt_yes_no("Hide which project you're working in?")
    } else {
        false
    };

    // Command axis: same resolution.
    cfg.privacy.fields.command = if opts.show_command {
        true
    } else if opts.hide_command {
        false
    } else if interactive {
        !prompt_yes_no("Hide the command currently running?")
    } else {
        false
    };

    if interactive {
        println!(
            "  Model, metrics and the elapsed timer are always shown. Use --private to hide everything."
        );
    }

    cfg.save()?;
    Ok(())
}

/// Ask a yes/no question on stdout, reading one line from stdin. The default is
/// YES (shown as `[Y/n]`): an empty line or a `y`/`Y` answer is treated as yes,
/// anything else as no. A read error falls back to the default (yes).
fn prompt_yes_no(question: &str) -> bool {
    use std::io::Write;

    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let answer = line.trim();
            answer.is_empty() || answer.eq_ignore_ascii_case("y")
        }
        Err(_) => true,
    }
}

/// Re-enable the daemon by loading the launchd agent (the `on` alias, FR-8).
///
/// Requires a prior `install` (the plist must exist); a missing plist is a clear
/// error pointing the user at `claude-presence install` rather than a silent no-op.
fn enable() -> Result<()> {
    let path = launchd::plist_path()?;
    if !path.exists() {
        eprintln!("claude-presence: not installed — run `claude-presence install` first.");
        return Err(Error::Other(
            "launchd agent plist missing; run `claude-presence install`".into(),
        ));
    }
    launchd::bootstrap(&path)?;
    println!("claude-presence: enabled (presence will appear when a session is active).");
    Ok(())
}

/// Disable the daemon by unloading the launchd agent (the `off` alias, FR-8).
///
/// `bootout` sends SIGTERM; the daemon's graceful shutdown already clears the
/// Discord presence (see `src/lib.rs` shutdown path → sink `clear_activity`), so
/// no extra IPC is needed here. `bootout` is idempotent / tolerant of a
/// not-loaded job, like uninstall.
fn disable() -> Result<()> {
    launchd::bootout(launchd::label())?;
    println!("claude-presence: disabled — Discord presence cleared.");
    Ok(())
}

/// Best-effort revert of the statusline wrapper during install rollback.
fn rollback_statusline() {
    if let Err(err) = statusline::uninstall() {
        tracing::warn!(target: "install", %err, "rollback: could not revert statusline wrapper");
        eprintln!("  [warn] rollback: statusline wrapper not fully reverted: {err}");
    }
}

/// Best-effort revert of the chained hooks during install rollback.
fn rollback_hooks() {
    if let Err(err) = hooks::uninstall() {
        tracing::warn!(target: "install", %err, "rollback: could not revert hooks");
        eprintln!("  [warn] rollback: hooks not fully reverted: {err}");
    }
}

/// Reverse every chained change `install` made (FR-8/AC-3, NFR-6).
///
/// Order is the exact reverse of [`install`]: `launchd::uninstall()` **first** so
/// `launchctl bootout` tears the job out of launchd *before* anything else and
/// before the process could be relaunched (FR-8/AC-3), then the hooks, then the
/// statusline wrapper. Each sub-uninstall already does restore-or-warn on drift;
/// we surface a clear per-step PASS/WARN summary. A failure of one step does not
/// abort the rest — every artifact gets a removal attempt — but the first error
/// is propagated as the exit status.
fn uninstall() -> Result<()> {
    println!("Uninstalling claude-presence…");
    let mut first_err: Option<Error> = None;

    // (1) launchd FIRST — bootout before the process exits / can relaunch.
    match launchd::uninstall() {
        Ok(()) => println!("  [ok] launchd agent booted out and plist removed"),
        Err(err) => {
            eprintln!("  [warn] launchd agent not fully removed: {err}");
            first_err.get_or_insert(err);
        }
    }

    // (2) hooks — remove only our exact entries (preserves the user's hooks).
    match hooks::uninstall() {
        Ok(()) => println!("  [ok] lifecycle hooks unchained (user hooks preserved)"),
        Err(err) => {
            eprintln!("  [warn] hooks not fully unchained: {err}");
            first_err.get_or_insert(err);
        }
    }

    // (3) statusline — restore the user's original on no drift, else warn.
    match statusline::uninstall() {
        Ok(()) => println!("  [ok] statusline wrapper removed (original restored if unchanged)"),
        Err(err) => {
            eprintln!("  [warn] statusline wrapper not fully removed: {err}");
            first_err.get_or_insert(err);
        }
    }

    match first_err {
        None => {
            println!("Uninstall complete.");
            Ok(())
        }
        Some(err) => {
            eprintln!("Uninstall finished with warnings; see the lines above.");
            Err(err)
        }
    }
}

/// Show detected live sessions and the Discord/daemon connection state.
///
/// Human-readable stdout (FR-8/AC-2). Degrades gracefully: zero sessions and an
/// absent Discord both render as plain status lines, never an error. The daemon
/// is reported "running" when the single-instance lock is already held by someone
/// else (acquiring it ourselves means nothing else holds it).
fn status() -> Result<()> {
    println!("claude-presence status");

    match sessions::discover() {
        Ok(live) if live.is_empty() => println!("  Sessions: none detected"),
        Ok(live) => {
            println!("  Sessions: {} live", live.len());
            for s in &live {
                let branch = s.branch.as_deref().unwrap_or("-");
                println!(
                    "    - {} (pid {}, branch {})",
                    s.project_name, s.pid, branch
                );
            }
        }
        Err(err) => println!("  Sessions: discovery failed ({err})"),
    }

    println!("  Discord: {}", discord_socket_status());
    println!("  Daemon:  {}", daemon_run_status());
    Ok(())
}

/// Whether a Discord IPC socket (`$TMPDIR/discord-ipc-0..9`) is present.
fn discord_socket_status() -> &'static str {
    if discord_socket_present() {
        "IPC socket present"
    } else {
        "not running / not detected"
    }
}

/// Probe `$TMPDIR/discord-ipc-0..9` for an existing socket file.
fn discord_socket_present() -> bool {
    (0u8..10).any(|n| macos::discord_ipc_socket(n).exists())
}

/// Whether the daemon appears to be running, inferred from the single-instance
/// lock: if we cannot acquire it (`AlreadyRunning`), a daemon holds it; if we can
/// acquire it, nothing else is running (we release it immediately).
fn daemon_run_status() -> &'static str {
    match acquire_single_instance_lock() {
        Err(Error::AlreadyRunning) => "running (single-instance lock held)",
        Ok(_lock) => "not running (lock is free)",
        Err(_) => "unknown (could not probe lock)",
    }
}

/// The PASS/WARN/FAIL verdict of a single `doctor` check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Pass,
    Warn,
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Pass => "PASS",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }
}

/// One diagnostic line: a verdict, a headline, and an actionable hint.
struct Check {
    level: Level,
    label: String,
    hint: String,
}

impl Check {
    fn print(&self) {
        println!("[{}] {} — {}", self.level.tag(), self.label, self.hint);
    }
}

/// Diagnose the install: Discord socket, settings wiring, config validity,
/// single-instance conflicts, and the buttons-on-own-profile caveat (FR-8/AC-2).
///
/// Every check prints a `[PASS]/[WARN]/[FAIL]` line with a remediation hint. The
/// command never panics and exits cleanly even when Discord is absent and no
/// sessions exist (those are reported as `WARN`, not errors).
fn doctor() -> Result<()> {
    println!("claude-presence doctor");

    let checks: Vec<Check> = vec![
        // (1) Discord IPC socket present in $TMPDIR.
        check_discord(discord_socket_present()),
        // (2) Settings wiring: statusLine wrapper + hook entries.
        check_statusline(statusline::is_wired()),
        check_hooks(hooks::wired_count()),
        // (2b) launchd agent plist present (its bootstrap wiring).
        check_launchd(launchd::plist_path().map(|p| p.exists())),
        // (3) Config validity (always returns defaults; report effective values).
        check_config(&Config::load()),
        // (4) Single-instance conflict.
        check_instance(acquire_single_instance_lock()),
        // (5) Detected sessions (informational; absent is a WARN, not a failure).
        check_sessions(sessions::discover()),
        // (6) Buttons-on-own-profile caveat (design §4.3, FR-7/AC-2).
        check_buttons(&Config::load()),
    ];

    for check in &checks {
        check.print();
    }

    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    println!(
        "\n{} check(s): {} FAIL, {} WARN.",
        checks.len(),
        fails,
        warns
    );
    Ok(())
}

/// PASS when a Discord IPC socket is present, WARN (with launch hint) otherwise.
fn check_discord(present: bool) -> Check {
    if present {
        Check {
            level: Level::Pass,
            label: "Discord IPC socket".into(),
            hint: "found in $TMPDIR".into(),
        }
    } else {
        Check {
            level: Level::Warn,
            label: "Discord IPC socket".into(),
            hint: "Discord not running / not detected — start the Discord desktop app".into(),
        }
    }
}

/// PASS when our statusLine wrapper is wired, WARN otherwise; FAIL on a read error.
fn check_statusline(wired: Result<bool>) -> Check {
    match wired {
        Ok(true) => Check {
            level: Level::Pass,
            label: "statusLine wiring".into(),
            hint: "settings.json points at our wrapper".into(),
        },
        Ok(false) => Check {
            level: Level::Warn,
            label: "statusLine wiring".into(),
            hint: "wrapper not installed — run `claude-presence install`".into(),
        },
        Err(err) => Check {
            level: Level::Fail,
            label: "statusLine wiring".into(),
            hint: format!("could not read settings.json: {err}"),
        },
    }
}

/// PASS when every lifecycle event carries our hook entry, WARN when partial /
/// none, FAIL on a read error.
fn check_hooks(count: Result<(usize, usize)>) -> Check {
    match count {
        Ok((present, total)) if present == total => Check {
            level: Level::Pass,
            label: "hooks wiring".into(),
            hint: format!("{present}/{total} lifecycle events chained"),
        },
        Ok((present, total)) => Check {
            level: Level::Warn,
            label: "hooks wiring".into(),
            hint: format!("only {present}/{total} events chained — run `claude-presence install`"),
        },
        Err(err) => Check {
            level: Level::Fail,
            label: "hooks wiring".into(),
            hint: format!("could not read settings.json: {err}"),
        },
    }
}

/// PASS when the launchd plist exists, WARN otherwise; FAIL if the path is
/// unresolvable.
fn check_launchd(exists: Result<bool>) -> Check {
    match exists {
        Ok(true) => Check {
            level: Level::Pass,
            label: "launchd agent".into(),
            hint: "plist present in ~/Library/LaunchAgents".into(),
        },
        Ok(false) => Check {
            level: Level::Warn,
            label: "launchd agent".into(),
            hint: "plist missing — run `claude-presence install`".into(),
        },
        Err(err) => Check {
            level: Level::Fail,
            label: "launchd agent".into(),
            hint: format!("could not resolve plist path: {err}"),
        },
    }
}

/// PASS: Config::load() always returns a valid config; report the effective
/// client_id and capacity.
fn check_config(cfg: &Config) -> Check {
    let capacity = cfg
        .capacity
        .map(|c| c.to_string())
        .unwrap_or_else(|| "auto (live count)".into());
    Check {
        level: Level::Pass,
        label: "config".into(),
        hint: format!("client_id={}, capacity={}", cfg.client_id, capacity),
    }
}

/// Single-instance: report a running daemon when the lock is already held
/// (WARN — that is the expected state once installed), PASS when free, FAIL on an
/// unexpected lock error.
fn check_instance(lock: Result<claude_presence::InstanceLock>) -> Check {
    match lock {
        Ok(_lock) => Check {
            level: Level::Pass,
            label: "single-instance".into(),
            hint: "no other daemon holds the lock".into(),
        },
        Err(Error::AlreadyRunning) => Check {
            level: Level::Warn,
            label: "single-instance".into(),
            hint: "another daemon instance is running (lock held) — expected if installed".into(),
        },
        Err(err) => Check {
            level: Level::Fail,
            label: "single-instance".into(),
            hint: format!("could not probe the lock: {err}"),
        },
    }
}

/// Detected sessions: PASS with a count when any exist, WARN when none, FAIL on a
/// discovery error.
fn check_sessions(live: Result<Vec<sessions::LiveSession>>) -> Check {
    match live {
        Ok(s) if s.is_empty() => Check {
            level: Level::Warn,
            label: "sessions".into(),
            hint: "none detected — start a Claude Code session to see the card".into(),
        },
        Ok(s) => Check {
            level: Level::Pass,
            label: "sessions".into(),
            hint: format!("{} live session(s) detected", s.len()),
        },
        Err(err) => Check {
            level: Level::Fail,
            label: "sessions".into(),
            hint: format!("discovery failed: {err}"),
        },
    }
}

/// Buttons-on-own-profile caveat (design §4.3 / FR-7/AC-2): always informational.
fn check_buttons(cfg: &Config) -> Check {
    if cfg.buttons.is_empty() {
        Check {
            level: Level::Pass,
            label: "buttons".into(),
            hint: "off by default; note they may not render on your OWN Discord profile over local IPC".into(),
        }
    } else {
        Check {
            level: Level::Warn,
            label: "buttons".into(),
            hint: format!(
                "{} configured — they may NOT render on your OWN profile over local IPC (visible to others)",
                cfg.buttons.len()
            ),
        }
    }
}

/// Pipe the hook/statusline JSON arriving on stdin to the daemon socket.
///
/// The `kind` is informational only — the forwarded JSON already carries its own
/// `kind`/`event` discriminant, so the body is identical for both: read stdin,
/// connect, write, exit. Delivery is best-effort and **never fails the caller**
/// (FR-4/AC-3): a down/absent daemon socket is swallowed so the chained hook or
/// statusline command can never fail the originating tool call.
fn forward(_kind: ForwardKind) -> Result<()> {
    claude_presence::ingest::socket::forward_stdin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_pass_when_present_warn_when_absent() {
        assert_eq!(check_discord(true).level, Level::Pass);
        assert_eq!(check_discord(false).level, Level::Warn);
        // The absent hint must be actionable (mentions Discord).
        assert!(check_discord(false).hint.to_lowercase().contains("discord"));
    }

    #[test]
    fn statusline_levels_map_from_result() {
        assert_eq!(check_statusline(Ok(true)).level, Level::Pass);
        assert_eq!(check_statusline(Ok(false)).level, Level::Warn);
        assert_eq!(
            check_statusline(Err(Error::Other("boom".into()))).level,
            Level::Fail
        );
    }

    #[test]
    fn hooks_pass_only_when_all_events_wired() {
        assert_eq!(check_hooks(Ok((6, 6))).level, Level::Pass);
        assert_eq!(check_hooks(Ok((3, 6))).level, Level::Warn);
        assert_eq!(check_hooks(Ok((0, 6))).level, Level::Warn);
        assert_eq!(
            check_hooks(Err(Error::Other("boom".into()))).level,
            Level::Fail
        );
        // The partial hint reports the ratio so the user sees what's missing.
        assert!(check_hooks(Ok((3, 6))).hint.contains("3/6"));
    }

    #[test]
    fn launchd_levels_map_from_existence() {
        assert_eq!(check_launchd(Ok(true)).level, Level::Pass);
        assert_eq!(check_launchd(Ok(false)).level, Level::Warn);
        assert_eq!(
            check_launchd(Err(Error::PathResolution("home"))).level,
            Level::Fail
        );
    }

    #[test]
    fn config_check_reports_effective_values() {
        let check = check_config(&Config::default());
        assert_eq!(check.level, Level::Pass);
        // The default client_id and the auto-capacity wording must be surfaced.
        assert!(check
            .hint
            .contains(&Config::default().client_id.to_string()));
        assert!(check.hint.contains("auto"));

        let cfg = Config {
            capacity: Some(7),
            ..Config::default()
        };
        assert!(check_config(&cfg).hint.contains("capacity=7"));
    }

    #[test]
    fn instance_check_warns_when_already_running() {
        assert_eq!(
            check_instance(Err(Error::AlreadyRunning)).level,
            Level::Warn
        );
        assert_eq!(
            check_instance(Err(Error::Other("x".into()))).level,
            Level::Fail
        );
        // A held lock means nothing else is running → PASS.
        // (We can't fabricate an InstanceLock here without I/O, so the
        // AlreadyRunning / error arms above cover the decision branches.)
    }

    #[test]
    fn sessions_check_warns_when_none() {
        assert_eq!(check_sessions(Ok(Vec::new())).level, Level::Warn);
        assert_eq!(
            check_sessions(Err(Error::Other("x".into()))).level,
            Level::Fail
        );
        let live = vec![sessions::LiveSession {
            pid: 1,
            session_id: "s".into(),
            cwd: std::path::PathBuf::from("/p"),
            project_name: "p".into(),
            transcript: None,
            branch: None,
            started_at: None,
            version: None,
        }];
        assert_eq!(check_sessions(Ok(live)).level, Level::Pass);
    }

    #[test]
    fn buttons_check_notes_own_profile_caveat() {
        // No buttons configured -> Pass (the default ships one button, so clear it
        // explicitly to exercise the empty branch).
        let off_cfg = Config {
            buttons: Vec::new(),
            ..Config::default()
        };
        let off = check_buttons(&off_cfg);
        assert_eq!(off.level, Level::Pass);
        assert!(off.hint.to_lowercase().contains("own"));

        // The shipped default has one button -> Warn.
        let on = check_buttons(&Config::default());
        assert_eq!(on.level, Level::Warn);
        assert!(on.hint.to_lowercase().contains("own"));
    }
}
