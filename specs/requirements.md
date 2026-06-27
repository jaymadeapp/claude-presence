# Requirements: Claude Code Discord Rich Presence

> A Rust daemon for macOS that aggregates live Claude Code activity into a single
> Discord Rich Presence card — the "Claude Code" equivalent of the old VSCode Discord
> presence and the Codex rich-presence screenshot.

All facts below were live-verified on the author's Mac against Claude Code **v2.1.181**
(see `specs/research-dossier.json`) and adversarially re-verified before build
(`specs/verification-report.json`). Internal `~/.claude` layouts are undocumented and
version-specific; the spec treats them as a pinned, adapter-isolated contract.

## Product goal

One Discord presence card, updated in real time, that mirrors what's happening across all
running Claude Code sessions on the machine:

- **details** (≤128): current activity + project + branch — e.g. `Running cargo check — private (master)`
- **state** (≤128): model · plan · cost · tokens · context% — e.g. `Opus 4.8 (High) · Max · $0.98 · 837K tok · Ctx 8%`
- **timestamps.start**: elapsed timer (Discord renders it from an epoch-**milliseconds** value)
- **party.size** `[live_count, capacity]`: how many sessions are running (renders "(2 of 5)")
- **assets**: Claude Code logo (large) + current-tool badge (small)
- optional **buttons** (off by default — see FR-7/AC-2)

## Constraints

- **C-1** Rust, macOS only (initial). Structure to allow later Linux/Windows.
- **C-2** Discord shows **one presence per application per user** → all sessions MUST be
  aggregated into a single card (count goes to `party.size`), never one presence per session.
- **C-3** `details` and `state` are hard-capped at **128 characters** each.
- **C-4** All `~/.claude` internal schemas are undocumented and change between CLI versions.
  The only Anthropic-stable contracts are the **statusLine JSON** and the **hooks JSON**.
  Internal-schema reads MUST go through a versioned adapter and degrade gracefully.
- **C-5** No root, no bot token, no OAuth. Local Discord IPC only.
- **C-6** MUST NOT break or slow down Claude Code. Installed hooks/statusline wrappers MUST
  **chain** the user's existing config, never overwrite it, and be fully reversible.
- **C-7** Privacy: transcripts contain prompt text, file paths, and possibly secrets. The
  daemon emits only structured, sanitized fields — never raw prompt/file content — to Discord
  **or to its own logs**.

## Functional requirements

### FR-1 — Session discovery & liveness
- **AC-1** Enumerate running Claude Code engine processes by **executable-path filter**
  (path contains `/claude-code/<ver>/` and ends `/MacOS/claude`), excluding the Claude.app
  GUI, the `Helpers/disclaimer` wrapper, and Discord. MUST NOT use `pgrep -f` (verified to
  truncate long argv and miss sessions).
- **AC-2** For each engine PID, read `~/.claude/sessions/<PID>.json`
  (`pid, sessionId, cwd, startedAt, procStart, version, peerProtocol, kind, entrypoint`) and
  confirm liveness via `kill(pid, 0)`; discard stale/dead registry files.
- **AC-3** Resolve each PID's cwd via `lsof -d cwd` / libproc (argv has no `--cwd`), and map
  to its transcript file by cwd→project-slug→newest-mtime `*.jsonl`, cross-checking the
  filename equals the registry `sessionId`. MUST NOT trust argv `--resume`/`--session-id`
  (verified stale on forked sessions).
- **AC-4** Output a deduped set of live sessions: `{pid, sessionId, cwd, project_name, branch, started_at, version}`.

### FR-2 — Activity & state extraction from transcripts
- **AC-1** Watch each live session's transcript with FS events (no polling), parse only newly
  appended lines incrementally, and tolerate a truncated final JSON line.
- **AC-2** Derive current activity from the latest assistant `tool_use` block. Default mapping
  uses **only a sanitized verb**, never raw arguments: Bash→`Running <program>` (first token of
  the command only, e.g. `Running cargo`), Edit/Write→`Editing <basename>`, Read→`Reading <basename>`,
  Grep/Glob→`Searching`, Agent/Task→`Orchestrating agents`, `mcp__<server>__*`→`Using <server>`.
  Showing a fuller (scrubbed) Bash command is opt-in via config and MUST pass the secret-scrubber
  (FR-7/AC-2). Streaming partials (lines sharing `message.id` with `stop_reason:null`) MUST be
  deduped to the final line.
- **AC-3** Extract `message.model`, `gitBranch` (handle `HEAD`/detached and null), `cwd`, and
  `ai-title`. Extraction ≠ emission: `ai-title` is model-generated from the user's prompt and
  MUST be routed through redaction + blacklist and is **off by default** (FR-7/AC-2).
- **AC-4** Compute token totals from `message.usage`, and live context tokens =
  `input + cache_read + cache_creation` of the latest request.
- **AC-5** Classify busy vs idle: **busy** if the last assistant `tool_use` has no matching
  `tool_result` yet (primary signal); CPU% is only a weak corroborator (verified misleading).
- **AC-6** Count live subagents = `subagents/**/agent-*.jsonl` (`isSidechain:true`) files
  modified within a configurable recency window (subagents are in-process, not separate PIDs).
  This count is surfaced separately (e.g. in `state` / `small_text`) and is **never** added to
  `party.size` (FR-5/AC-2).

### FR-3 — Cost / context% / model via statusLine (with fallback)
- **AC-1** Install a **statusline-wrapper** that, at install time, captures the user's current
  `statusLine.command` and stores it; the installed wrapper then invokes that stored inner
  command, passes its stdout straight through to Claude Code (visible statusline unchanged), and
  additionally forwards the statusLine JSON (arriving on **stdin** — Claude Code injects no custom
  env vars) to the daemon's local socket via `claude-presence forward`. Fully reversible.
- **AC-2** From statusLine JSON, extract `cost.total_cost_usd`,
  `context_window.used_percentage`, `context_window.context_window_size`,
  `model.display_name`, `version`, `effort.level`, `cost.total_duration_ms`. (Field shapes
  confirmed present in the live v2.1.181 binary.)
- **AC-3** Fallback when statusLine data is absent/stale: compute cost from `usage × pricing`
  (per-model, including cache-read and cache-creation rates) and ctx% =
  `live_context_tokens / context_window_size`.
- **AC-4** Maintain an external, easily-updatable **pricing + context-window table** keyed by
  model id. MUST default Opus 4.8/4.7/4.6 and Sonnet 4.6 to a **1M** window, and Opus 4.5 /
  Sonnet 4.5 / Haiku 4.5 to **200k** (verified against official docs; wrong denominator inflates
  ctx% ~5×). Prefer `context_window.context_window_size` from statusLine when present.

### FR-4 — Lifecycle hooks for realtime activity
- **AC-1** Install hooks (`SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`,
  `SubagentStart`, `SubagentStop` — all confirmed to exist in v2.1.181) that POST a compact
  JSON event to the daemon's socket. MUST chain any existing hook for the same event by
  **appending** our entry into that event's existing `hooks[]` group while preserving the
  user's entries (e.g. the existing `Stop` → `afplay … Submarine.aiff`); create the event group
  if absent; on uninstall remove **only our exact command entry by identity** (`SessionEnd` /
  `CwdChanged` exist too and are optional later adds).
- **AC-2** `PreToolUse` updates current activity immediately (fires ms before the tool runs);
  `PostToolUse`/`Stop` transition to next/idle.
- **AC-3** Hook handlers are fast, non-blocking, and never fail the tool call if the daemon is
  down. Target: forwarder returns in **<10ms p95 over 100 invocations** (relax to the documented
  ~50ms hook budget if unattainable on the box).

### FR-5 — State aggregation
- **AC-1** Merge all collectors into one `PresenceModel`. The headline/"focused" session =
  most-recently-active by transcript mtime / last event, with a sticky window to avoid thrash
  (true OS focus is unobtainable inside Claude.app — verified).
- **AC-2** `party.size = [live_count, capacity]`, where `live_count` = number of live top-level
  sessions and `capacity` = the configured max (FR-7/AC-1), defaulting to `live_count` when
  unset. `party.current` is therefore ≥1 whenever any session exists. Busy/idle is conveyed via
  `state` text or `small_image`, not via `party.current`.
- **AC-3** Build sanitized `details` and `state` strings from a configurable template, fitting
  ≤128 chars via this ordered ladder (identical in design §5): (1) abbreviate model
  (`Opus 4.8`→`O4.8`); (2) abbreviate plan; (3) if still over, drop metrics from the tail in
  order ctx% → tokens → cost.
- **AC-4** `timestamps.start` = focused session `started_at` expressed as **epoch milliseconds**
  (Discord/the crate interpret this field as ms; a seconds value yields a ~1000× wrong timer).
  Configurable: session start vs current turn start.
- **AC-5** Recompute on any collector event; coalesce/debounce before handing to the sink.
- **AC-6** Empty state: when zero live sessions exist, clear the Discord presence
  (`activity:null`) and hold cleared until a session appears (configurable: clear vs a generic
  idle card).

### FR-6 — Discord IPC sink
- **AC-1** Probe `discord-ipc-0`..`-9` in `$TMPDIR`, connect to the first that handshakes with
  the configured `client_id`, complete handshake, handle `READY`. If no socket is found at
  startup (Discord not running), retry with backoff rather than exiting.
- **AC-2** Push `SET_ACTIVITY` from the `PresenceModel`; on shutdown send `activity:null` to clear.
- **AC-3** Debounce: publish only on change, minimum `min_interval` (default 2.5s) apart, with a
  keep-alive republish every `keepalive_interval` (default 15s) so the presence doesn't expire.
- **AC-4** Detect send failure (Discord quit/restart), tear down, and reconnect with backoff,
  re-probing sockets.
- **AC-5** Respect Discord's update rate limit (5 updates / 20s, per official docs) by coalescing
  to the newest model.

### FR-7 — Configuration & privacy
- **AC-1** TOML config: `client_id`, `plan_label`, `capacity`, `min_interval` (2.5s),
  `keepalive_interval` (15s), field toggles, `assets` keys, tool→verb/icon maps,
  `subagent_recency_secs`, `show_ai_title` (default false), `buttons` (opt-in, per-project), and a
  `[privacy]` section (`redact`, `blacklist_paths`, `scrub_bash_args`).
- **AC-2** Privacy (the card and buttons are PUBLIC):
  - Only structured, sanitized fields leave the process; never raw prompt text or file contents;
    paths → basename.
  - Bash arguments are dropped by default; if `scrub_bash_args` shows a command, strip
    token/key/secret/password/`Authorization` patterns, `WORD=value` env-assignments, credentialed
    URLs, and long base64/hex blobs, then truncate.
  - `ai-title` only shown when `show_ai_title` is set AND the project is not blacklisted, via the
    same redaction path as `details`.
  - Blacklisted projects show a generic label or nothing.
  - Buttons: off by default; when enabled, URLs MUST be `https://` (never `file://`); never emit a
    full home path or a private-repo remote URL.
- **AC-3** Ship safe defaults; missing config must not crash. Config changes take effect on
  daemon restart (no hot reload in v1).

### FR-8 — Daemon lifecycle & packaging
- **AC-1** Run as a **launchd user agent** (no root), with rotating logs. Acquire a
  single-instance lock at startup (flock on the state dir / socket ownership); if another live
  instance exists, exit clearly (two writers would breach the 5/20s rate limit).
- **AC-2** CLI subcommands: `run` (foreground), `install` (launchd + chained hooks + statusline
  wrapper), `uninstall` (full revert), `status`, `doctor` (diagnose Discord socket, detected
  sessions, settings wiring, instance conflicts), and an internal `forward` (used by the chained
  hook/statusline scripts to pipe events to the daemon socket; not user-facing).
- **AC-3** Graceful shutdown clears the Discord presence; `uninstall` restores the user's
  original statusline and hooks exactly, and `launchctl bootout gui/$(id -u)` MUST run **before**
  the process exits so launchd cannot relaunch it after it has cleared the presence.
- **AC-4** Logs MUST NOT contain raw `tool_input`, prompt/transcript text, full paths, or
  unredacted statusline JSON at any level — only the same sanitized summaries used for Discord.
  State/config dirs are `0700`; `config.toml`, logs, and `daemon.sock` are `0600`; the ingest
  server verifies peer uid == own uid (`getpeereid`).

### FR-9 — Menu-bar tray (optional, later phase)
- **AC-1** macOS tray icon (on/off state, current presence summary, pause toggle, quit).

## Non-functional requirements
- **NFR-1 Performance**: event-driven (FS watch + sockets), negligible idle CPU; hook forwarder
  meets the FR-4/AC-3 latency target.
- **NFR-2 Reliability**: never crash or slow Claude Code; tolerate schema drift; degrade to a
  reduced card rather than failing.
- **NFR-3 Privacy/Security**: local-only; the sole egress is the local Discord IPC socket; no
  secrets/prompts/file contents ever leave the machine or reach logs (C-7, FR-8/AC-4).
- **NFR-4 Maintainability**: internal-schema reads isolated behind a `claude/schema` adapter
  with a version check; pricing/window table external and trivially updatable.
- **NFR-5 Portability**: macOS first; socket/path/process specifics behind a platform module.
- **NFR-6 Reversibility**: every install action has an exact, tested uninstall.

## Out of scope (initial)
- Windows/Linux support; per-tab/true-focus detection inside Claude.app; auto-detecting the
  subscription plan/price (user-configured constant); uploading Discord art assets
  programmatically (done once by the user in the Developer Portal).
