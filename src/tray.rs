//! Optional macOS menu-bar tray control (`feature = "tray"`, FR-9/AC-1).
//!
//! This module is **entirely opt-in**. The daemon's normal home is a headless
//! `launchd` user agent (design §3.1), which has no UI session and never builds
//! a tray. The tray is therefore a *foreground* convenience: when a user runs
//! `claude-presence` interactively and the binary is compiled with
//! `--features tray`, [`run`](crate::run) (a later integration) may call
//! [`run_tray`] to surface a menu-bar item exposing
//!
//! - the current presence **summary** (a disabled, text-only line, FR-9/AC-1);
//! - an **on/off** toggle (enable/disable pushing the presence to Discord);
//! - a **pause** toggle (temporarily hold the presence without disabling);
//! - **quit** (ask the daemon to shut down cleanly).
//!
//! ## Threading / integration contract
//!
//! [`tao`]'s event loop and [`tray_icon`] both require the **main thread** on
//! macOS, and the tray icon must be created from inside the event loop's
//! `StartCause::Init` callback (verified against `tray-icon` 0.24 docs). The
//! event loop's [`run`](tao::event_loop::EventLoop::run) never returns
//! (`-> !`), so [`run_tray`] takes over the calling thread for the process
//! lifetime. The daemon's collectors/sink already live on a `tokio` runtime on
//! other threads; the integration in [`crate::run`] would spawn that runtime,
//! then call [`run_tray`] on the main thread, wiring the two together through
//! the channels described on [`TrayConfig`]:
//!
//! - the daemon **publishes** the latest [`TraySummary`] on a
//!   [`watch::Receiver`] that the tray polls and renders;
//! - the tray **emits** [`TrayCommand`]s (on/off, pause, quit) back to the
//!   daemon over an [`mpsc::UnboundedSender`].
//!
//! Nothing here reaches Discord or the logs beyond the already-sanitized
//! summary string the daemon hands in (C-7): the tray is a pure consumer of
//! [`crate::state::model::PresenceModel`]-derived text and never sees raw
//! prompts, file contents, or full paths.

use std::time::{Duration, Instant};

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoop};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// How often the event loop wakes to pull the latest [`TraySummary`] and update
/// the menu's summary line. The presence itself is event-driven; this only
/// governs how quickly the *menu text* catches up, so a low-frequency poll keeps
/// idle CPU negligible (NFR-1) while staying responsive enough for a glance.
const SUMMARY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A sanitized, Discord-safe one-line summary of the current presence, produced
/// by the daemon for display in the tray menu (FR-9/AC-1).
///
/// The daemon builds this from its [`PresenceModel`](crate::state::model::PresenceModel)
/// (e.g. `"Editing tray.rs — claude-presence (main) · 3 sessions"`) *after* the
/// same sanitizers that gate the Discord card, so the tray never widens the
/// privacy surface (C-7). When no session is live the daemon sends an idle line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraySummary {
    /// The headline line shown (disabled) in the menu, already sanitized.
    pub line: String,
    /// Number of live sessions, for an at-a-glance count in the line/tooltip.
    pub live_count: u32,
}

impl TraySummary {
    /// The text rendered on the disabled summary menu item.
    ///
    /// Falls back to a neutral idle string when the daemon has nothing to show,
    /// so the menu is never blank.
    fn menu_text(&self) -> String {
        if self.line.is_empty() {
            "No active sessions".to_string()
        } else {
            self.line.clone()
        }
    }
}

/// A control command the tray emits back to the daemon (FR-9/AC-1).
///
/// The daemon owns the receiving end and is responsible for acting on these:
/// toggling whether the presence is pushed, pausing/resuming, and shutting down.
/// The tray only reflects user intent — it does not itself talk to Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Enable (`true`) or disable (`false`) pushing the presence to Discord.
    SetEnabled(bool),
    /// Pause (`true`) or resume (`false`) presence updates without disabling.
    SetPaused(bool),
    /// Request a clean daemon shutdown (clears the Discord presence, FR-8/AC-3).
    Quit,
}

/// Wiring the daemon hands to [`run_tray`].
///
/// Construct it from the channels the daemon already owns: a `watch` carrying
/// the latest [`TraySummary`] (cloned cheaply each poll) and an `mpsc` sender
/// the tray uses to push [`TrayCommand`]s. `enabled`/`paused` seed the initial
/// check-mark state so the menu matches the daemon's current mode on open.
pub struct TrayConfig {
    /// Latest presence summary, refreshed by the daemon; polled by the tray.
    pub summary: watch::Receiver<TraySummary>,
    /// Channel the tray pushes [`TrayCommand`]s onto for the daemon to act on.
    pub commands: mpsc::UnboundedSender<TrayCommand>,
    /// Initial on/off state of the presence (the on/off toggle's checkmark).
    pub enabled: bool,
    /// Initial paused state (the pause toggle's checkmark).
    pub paused: bool,
}

/// Run the macOS menu-bar tray on the **current (main) thread**, blocking for
/// the process lifetime (FR-9/AC-1).
///
/// This is the module's only public entry point. A later integration in
/// [`crate::run`] is expected to spawn the daemon's `tokio` runtime on a
/// background thread, then call `run_tray(cfg)` from `main` so the event loop
/// owns the main thread as macOS requires. Because [`tao`]'s
/// [`run`](tao::event_loop::EventLoop::run) returns `!`, control never comes
/// back here — selecting **Quit** sends [`TrayCommand::Quit`] to the daemon and
/// then exits the process via `ControlFlow::Exit`.
///
/// Tray construction failures (icon/menu build, unavailable UI session) are
/// logged via `tracing` and degrade gracefully: the function still enters the
/// event loop so the daemon keeps running headless, just without a visible menu
/// (NFR-2). No `unwrap`/`expect` is used on any runtime path.
pub fn run_tray(cfg: TrayConfig) -> ! {
    let TrayConfig {
        mut summary,
        commands,
        enabled,
        paused,
    } = cfg;

    let event_loop: EventLoop<()> = EventLoop::new();

    // Built lazily inside `StartCause::Init`: on macOS the tray icon must be
    // created after the event loop has started (tray-icon 0.24 requirement).
    // Held across iterations so the icon and its menu items stay alive.
    let mut tray: Option<TrayState> = None;

    let menu_channel = MenuEvent::receiver();

    event_loop.run(move |event, _target, control_flow| {
        // Wake periodically to refresh the summary line; between wakes, sleep.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + SUMMARY_POLL_INTERVAL);

        match event {
            // macOS: create the tray once the loop is live.
            Event::NewEvents(StartCause::Init) => match TrayState::build(enabled, paused) {
                Ok(state) => {
                    state.apply_summary(&summary.borrow());
                    info!("claude-presence: tray menu up");
                    tray = Some(state);
                }
                Err(err) => {
                    warn!(%err, "tray init failed; running without a menu-bar icon");
                }
            },

            // Periodic / general wake: pull the freshest summary and re-render.
            Event::NewEvents(_) => {
                if let Some(state) = tray.as_ref() {
                    // `has_changed` is cheap; only re-render the line on change.
                    if summary.has_changed().unwrap_or(false) {
                        state.apply_summary(&summary.borrow_and_update());
                    }
                }
            }

            _ => {}
        }

        // Drain any pending menu clicks regardless of which event woke us.
        while let Ok(menu_event) = menu_channel.try_recv() {
            if let Some(state) = tray.as_ref() {
                if let Some(cmd) = state.command_for(&menu_event) {
                    debug!(?cmd, "tray command");
                    if commands.send(cmd).is_err() {
                        // Daemon side is gone → nothing to control; exit the UI.
                        warn!("tray command channel closed; exiting tray");
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    if cmd == TrayCommand::Quit {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    // Keep the checkmarks in sync with the toggle just made.
                    state.reflect(cmd);
                }
            }
        }
    })
}

/// The live tray icon plus the menu items whose ids we match click events
/// against. Kept together so a single owner controls their lifetime (dropping
/// any of them would remove it from the menu).
struct TrayState {
    /// Held so the icon stays in the menu bar; not otherwise read after build.
    _icon: TrayIcon,
    summary_item: MenuItem,
    enabled_item: CheckMenuItem,
    pause_item: CheckMenuItem,
    quit_item: MenuItem,
}

impl TrayState {
    /// Build the menu (summary line · separator · on/off · pause · separator ·
    /// quit) and the menu-bar icon, seeding the toggle checkmarks (FR-9/AC-1).
    ///
    /// Menu mutation (`muda`) and icon construction (`tray-icon`) raise *distinct*
    /// error types, so this returns a boxed `std::error::Error`; the sole caller
    /// only logs it via `Display`.
    fn build(enabled: bool, paused: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let menu = Menu::new();

        // A disabled, text-only line showing the current presence summary.
        let summary_item = MenuItem::new("Starting…", false, None);
        // On/off toggle: whether the presence is pushed to Discord at all.
        let enabled_item = CheckMenuItem::new("Presence enabled", true, enabled, None);
        // Pause toggle: hold updates without fully disabling.
        let pause_item = CheckMenuItem::new("Paused", true, paused, None);
        let quit_item = MenuItem::new("Quit claude-presence", true, None);

        menu.append(&summary_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&enabled_item)?;
        menu.append(&pause_item)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit_item)?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Claude Code → Discord presence")
            .build()?;

        Ok(Self {
            _icon: icon,
            summary_item,
            enabled_item,
            pause_item,
            quit_item,
        })
    }

    /// Update the disabled summary line and the tooltip from the latest summary.
    fn apply_summary(&self, summary: &TraySummary) {
        self.summary_item.set_text(summary.menu_text());
        let tooltip = if summary.live_count == 0 {
            "Claude Code → Discord presence".to_string()
        } else {
            format!(
                "Claude Code → Discord presence — {} session{}",
                summary.live_count,
                if summary.live_count == 1 { "" } else { "s" }
            )
        };
        // A tooltip update failure is purely cosmetic; log and carry on.
        if let Err(err) = self._icon.set_tooltip(Some(tooltip)) {
            debug!(%err, "tray tooltip update failed");
        }
    }

    /// Map a raw [`MenuEvent`] to a [`TrayCommand`], reading the *new* checkmark
    /// state for the toggles. `muda` flips a [`CheckMenuItem`] before the event
    /// fires, so [`is_checked`](CheckMenuItem::is_checked) already reflects the
    /// click.
    fn command_for(&self, event: &MenuEvent) -> Option<TrayCommand> {
        if event.id == *self.enabled_item.id() {
            Some(TrayCommand::SetEnabled(self.enabled_item.is_checked()))
        } else if event.id == *self.pause_item.id() {
            Some(TrayCommand::SetPaused(self.pause_item.is_checked()))
        } else if event.id == *self.quit_item.id() {
            Some(TrayCommand::Quit)
        } else {
            None
        }
    }

    /// Re-assert the checkmark state after a command, so the menu always matches
    /// the intent we just sent the daemon (defensive — `muda` already toggled).
    fn reflect(&self, cmd: TrayCommand) {
        match cmd {
            TrayCommand::SetEnabled(on) => self.enabled_item.set_checked(on),
            TrayCommand::SetPaused(paused) => self.pause_item.set_checked(paused),
            TrayCommand::Quit => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_menu_text_falls_back_when_empty() {
        let empty = TraySummary::default();
        assert_eq!(empty.menu_text(), "No active sessions");

        let filled = TraySummary {
            line: "Editing tray.rs — claude-presence (main)".to_string(),
            live_count: 1,
        };
        assert_eq!(
            filled.menu_text(),
            "Editing tray.rs — claude-presence (main)"
        );
    }

    #[test]
    fn tray_command_equality_is_value_based() {
        assert_eq!(TrayCommand::SetEnabled(true), TrayCommand::SetEnabled(true));
        assert_ne!(
            TrayCommand::SetEnabled(true),
            TrayCommand::SetEnabled(false)
        );
        assert_ne!(TrayCommand::SetPaused(true), TrayCommand::Quit);
    }
}
