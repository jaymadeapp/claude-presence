# Design: Claude Code Discord Rich Presence

Implements `requirements.md`. Read that first. Verified facts: `specs/research-dossier.json`;
adversarial pre-build verification: `specs/verification-report.json`.

## 1. Architecture overview

A single long-running **launchd user agent** (`claude-presence`) with three input collectors
feeding a state aggregator, which drives one Discord IPC sink. (`collectors::` below is a
conceptual grouping; the real modules are `claude::*` and `ingest::*` per §3.)

```
                ┌───────────────────────── claude-presence daemon (tokio) ─────────────────────────┐
                │                                                                                    │
 FS events ───► │  claude::sessions    (process scan + ~/.claude/sessions/<PID>.json + liveness)    │
 (notify)  ───► │  claude::transcript  (tail <sessionId>.jsonl → activity/model/usage/branch/subs)  │
 unix sock ───► │  ingest::socket      (statusline-wrapper + hooks POST JSON events here)            │
                │                 │                                                                   │
                │                 ▼                                                                   │
                │        state::aggregator  ── merges → PresenceModel (focus, party, details/state)  │
                │                 │  (debounced)                                                      │
                │                 ▼                                                                   │
                │        discord::sink  ── SET_ACTIVITY over discord-ipc-N (reconnect, keepalive)     │
                └────────────────────────────────────────────────────────────────────────────────────┘
        installed once by `claude-presence install` (all reversible):
          • ~/.claude statusLine.command  → chained statusline-wrapper.sh (runs the STORED original, tees JSON)
          • ~/.claude hooks[<Event>].hooks[] → append chained forwarder (preserve user entries)
          • ~/Library/LaunchAgents/*.plist → launchd user agent
```

Why three sources (ADR-1): each covers a different gap.
- **sessions + transcript** = always-on, zero-config; gives the session set, project, branch,
  model, tokens, current activity, subagent count (the whole card can be built from these alone → MVP).
- **statusLine** = Anthropic-stable, exact cost + ctx% + window size (avoids our pricing drift).
- **hooks** = lowest-latency "Running X" the instant a tool starts.
The daemon must render a usable card from sessions+transcript alone (the MVP guarantee, FR-5/AC-1)
and upgrade fidelity as the others report in.

## 2. Technology stack

| Concern | Choice | Justification / gotcha |
|---|---|---|
| Async runtime | `tokio` | FS-watch task + unix-socket server + timers + channel to sink |
| FS watching | `notify = "8"` | FSEvents wrapper; transcripts are open-append-close (safe to tail). Pin to stable 8.x (9.x is prerelease). Scope: transcript + subagent files only (NOT a $TMPDIR socket watch) |
| Process enum | `sysinfo` | exe-path filter is the verified-reliable enumerator. **cwd/exe are NOT populated by default** — refresh with `ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_cwd(UpdateKind::Always).with_cpu()`; `cpu_usage()` needs two refreshes ≥ `MINIMUM_CPU_UPDATE_INTERVAL` apart |
| cwd fallback | `lsof` | invoke `lsof -a -p <PID> -d cwd -Fn` for the PID's cwd descriptor if sysinfo cwd is empty (libproc's `pidcwd` is a no-op stub on macOS) |
| Liveness | `nix` (`kill(pid, 0)`, `getpeereid`) | prune stale registry; verify ingest socket peer uid |
| JSON | `serde` + `serde_json` | transcript lines, sessions/statusline/hook payloads |
| Errors | `thiserror` | crate-wide error enum (`src/error.rs`) — CLAUDE.md mandates it |
| Discord | `discord-rich-presence = "1.1"` (vionya) | sync, maintained; exposes details/state/timestamps/assets/party/buttons. **`DiscordIpcClient::new(id)` returns `Self` (no `?`)**; `connect/reconnect/set_activity/clear_activity/close` return `Result<(), discord_rich_presence::error::Error>`. The dossier C1/C2 snippets are 0.2-era and use `new(...)?` — drop the `?` |
| Config | `toml` + `serde` | human-editable config |
| CLI | `clap` (derive) | `run`/`install`/`uninstall`/`status`/`doctor`/`forward` |
| Logging | `tracing` + `tracing-subscriber` + rotating file appender | sanitized logs only (FR-8/AC-4) |
| Paths | `directories` / `dirs` | resolve `~/.claude`, `$TMPDIR`, LaunchAgents, state/config dirs |
| Tray (Phase 5) | `tray-icon` + `tao` | optional menu-bar control |

`discord-rich-presence` is synchronous → run the sink on a dedicated OS thread (or
`spawn_blocking`) fed by a `tokio::sync::watch` channel carrying the latest `PresenceModel`
(ADR-6). This isolates blocking IPC from the async collectors.

## 3. Module layout

```
src/
  main.rs                 CLI dispatch (clap); run boots collectors+aggregator+sink+logging
  error.rs                crate Error enum (thiserror)
  logging.rs              tracing-subscriber init + rotating file appender (sanitized)
  config.rs               Config struct + load/defaults + [privacy] settings
  privacy.rs              redaction, repo/path blacklist, sanitizers (paths→basename, bash-arg scrub, secret strip)
  claude/
    schema.rs             versioned adapters: transcript line, sessions/<PID>.json, statusline JSON, hook JSON
    sessions.rs           process enum (exe filter) + registry read + liveness → Vec<LiveSession>
    transcript.rs         notify watcher + incremental JSONL parse + activity/model/usage/branch + subagent count
    pricing.rs            model→price + context_window table; cost & ctx% calc
    activity.rs           tool_name(+sanitized input) → Activity{verb,target,small_image_key}
  ingest/
    socket.rs             unix-socket server (0600, peer-uid checked) receiving statusline + hook events
    events.rs             IngestEvent enum (Hook{...}, StatusLine{...})
  state/
    model.rs              SessionState, Activity, PresenceModel  (owned by task 0.3)
    aggregator.rs         merge collectors, focus, party.size, format details/state, debounce, empty-state
  discord/
    sink.rs               connect ipc-0..9, SET_ACTIVITY, clear, reconnect/backoff, keepalive, rate-limit
  install/
    launchd.rs            write/remove LaunchAgent plist; bootstrap/bootout gui/<uid>
    hooks.rs              chain-install/uninstall hook forwarder in settings.json (append/remove by identity)
    statusline.rs         chain-install/uninstall statusline-wrapper (store + restore original command)
    paths.rs              well-known paths, daemon socket path, 0700 state dir
  platform/macos.rs       $TMPDIR discord sockets, lsof cwd fallback
  tray.rs                 (feature = "tray") menu-bar control
assets/
  statusline-wrapper.sh   execs the STORED original command, passes stdout through, tees stdin JSON to `claude-presence forward --kind statusline`
  hook-forward.sh         pipes hook stdin JSON to `claude-presence forward --kind hook` (chained alongside user entries)
  LaunchAgent.plist.tmpl  launchd template (keys in §3.1)
```

### 3.1 launchd plist keys
`Label` = `com.<author>.claude-presence`; `ProgramArguments` = `[<abs binary path>, "run"]`;
`RunAtLoad` = true; `KeepAlive` = `{ SuccessfulExit: false }` (restart only on crash, so a clean
exit-0 that cleared the presence is NOT relaunched); `StandardOutPath`/`StandardErrorPath` under
the log dir; `ProcessType` = `Background`. **Do NOT set `EnvironmentVariables` for TMPDIR/HOME** —
`gui/<uid>` agents inherit per-user values; hardcoding the dynamic `/var/folders` TMPDIR is a bug.
`uninstall` runs `launchctl bootout gui/$(id -u) <label>` before the process exits.

## 4. Key data contracts

### 4.1 Daemon ingest socket
Unix socket at `~/.local/state/claude-presence/daemon.sock` (0600, inside a 0700 dir),
newline-delimited JSON, peer-uid verified. The chained shell scripts pipe their stdin into
`claude-presence forward --kind hook|statusline`, which connects to the socket and exits — no
external dependency (ADR-9).

```jsonc
// hook event (sanitized before it influences any Discord field)
{ "kind":"hook", "event":"PreToolUse", "session_id":"d4f6…", "cwd":"/…/private",
  "tool_name":"Bash", "tool_input":{"command":"cargo check"}, "ts": 1781989000 }
// statusline event (subset of CC's statusline JSON, arriving on stdin in the wrapper)
{ "kind":"statusline", "session_id":"d4f6…", "cwd":"/…/private", "model":"Opus 4.8",
  "effort":"high", "cost_usd":0.98, "ctx_pct":8.3, "ctx_size":1000000, "version":"2.1.181" }
```

### 4.2 Domain types (src/state/model.rs)
```rust
struct Activity { verb: String, target: Option<String>, small_image_key: Option<String> }
struct SessionState {
  session_id: String, pid: i32, project: String, branch: Option<String>,
  model: Option<String>, started_at: SystemTime, last_active: SystemTime,
  busy: bool, activity: Option<Activity>,
  cost_usd: Option<f64>, ctx_pct: Option<f64>, tokens_total: Option<u64>, subagents: u32,
}
struct PresenceModel {
  sessions: Vec<SessionState>, focused: usize, live_count: u32, capacity: u32,
  details: String, state: String, started_at_ms: i64,
  large_image: String, large_text: String, small_image: Option<String>, small_text: Option<String>,
  buttons: Vec<(String,String)>,
}
```

### 4.3 Discord Activity mapping (the Codex card)
| Activity field | Source field in PresenceModel | Notes |
|---|---|---|
| `details` (≤128) | `details` | `"{verb} {target} — {project} ({branch})"`, sanitized |
| `state` (≤128) | `state` | `"{model} ({effort}) · {plan} · ${cost} · {tokens} · Ctx {pct}%"`, truncate ladder §5 |
| `timestamps.start` | `started_at_ms` | **epoch milliseconds** (`Timestamps::new().start(ms)`); 13 digits for current dates |
| `party.size` | `[live_count, capacity]` | renders "(N of M)"; current ≥1 when any session exists |
| `assets.large_image` | `large_image` | uploaded asset key (e.g. `claude-logo`) |
| `assets.small_image` | `small_image` | per-tool badge key from `activity.rs` |
| `buttons` | `buttons` | **off by default**; may NOT render on the user's OWN profile over local IPC; https:// only |
| application name | (Discord app) | the registered app is named "CC" (client_id 1518007333324587168); `large_text` shows "Claude Code" |

## 5. Algorithms

- **Focus selection**: max `last_active` across live sessions; tie-break by most-recent hook
  event. Sticky window (configurable, e.g. 8s) to avoid thrash between two active sessions.
- **busy/idle**: scan transcript tail for assistant `tool_use` ids without a following
  `tool_result`; pending>0 ⇒ busy. Conveyed via `state`/`small_image`, not `party.current`.
- **Truncation ladder (identical to FR-5/AC-3)**: build `state` from the metric template; if >128:
  (1) abbreviate model (`Opus 4.8`→`O4.8`); (2) abbreviate plan; (3) drop metrics from the tail
  in order ctx% → tokens → cost.
- **Debounce**: coalesce model updates; emit at most one SET_ACTIVITY per `min_interval`
  (default 2.5s) and a keepalive every `keepalive_interval` (default 15s) if unchanged.
- **Empty state**: zero live sessions ⇒ `clear_activity` (or generic idle card per config); hold
  until a session appears.

## 6. Architecture decision records (summary)
- **ADR-1** Hybrid sources; statusLine+hooks are the only Anthropic-stable contract; transcript/
  sessions are version-pinned fallbacks behind an adapter.
- **ADR-2** Enumerate engines by exe-path filter via `sysinfo`, never `pgrep -f` (verified truncation).
- **ADR-3** One aggregated presence; `party.size = [live_count, capacity]` carries the session count.
- **ADR-4** Chain-install (never overwrite) the user's statusline + hooks: store the original
  statusLine command and bake it into the wrapper; for hooks, append our entry into the existing
  `hooks[<Event>].hooks[]` group (create the group if absent) and on uninstall remove only our
  exact command entry by identity; at uninstall, restore the original only if the current value
  still equals our installed wrapper, else surgically remove our segment and warn (drift).
- **ADR-5** `claude/schema.rs` adapter gated on `sessions/<PID>.json.version`; on unknown schema,
  degrade to a reduced card and log, never panic.
- **ADR-6** tokio async core + dedicated sync thread for the blocking Discord crate via a watch channel.
- **ADR-7** Focus = most-recently-active proxy; do not claim per-tab focus inside Claude.app.
- **ADR-8** Privacy-by-default: only structured/sanitized fields to Discord AND logs; repo blacklist;
  no prompt/file content; bash args dropped by default; ai-title off by default.
- **ADR-9** Hook/statusline scripts forward via a bundled `claude-presence forward` subcommand
  (no `socat`/extra dependency); they always exec the chained inner command so CC behavior is unchanged.

## 7. Risks & mitigations
| Risk | Mitigation |
|---|---|
| CC version bumps internal schema | adapter + version gate (ADR-5); statusLine/hooks keep working |
| statusLine quiet while idle / mid-subagent | fall back to transcript-derived cost/ctx; keepalive holds the card |
| Cost figure ≠ real bill (subscription) | label as "est."; prefer statusLine's own number |
| Discord rate-limit blanks presence | debounce + coalesce + keepalive (FR-6); single-instance lock (FR-8/AC-1) |
| Discord restart drops socket | detect send error, reconnect+re-probe ipc-0..9 with backoff |
| `KeepAlive=true` relaunches after clean exit | `KeepAlive={SuccessfulExit:false}`; bootout before exit (FR-8/AC-3) |
| Two daemon instances (launchd + manual run) | single-instance lock; doctor reports conflict |
| Buttons invisible on own profile / leak URLs | buttons off by default, https-only, no file://, no private remote (FR-7/AC-2) |
| Two sessions thrash the focus | sticky focus window |
| Leaking private project/prompt/secret data | sanitizers + blacklist + bash-arg drop + ai-title gating + structured-only emit to Discord and logs (ADR-8) |
| Clobbering user's statusline/hooks (incl. drift) | store-and-restore + append/remove-by-identity (ADR-4) |
| 128-char overflow | truncation ladder (§5) |
