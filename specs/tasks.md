# Tasks: Claude Code Discord Rich Presence

> Verify: `cargo fmt --check` `cargo clippy -- -D warnings` `cargo test`

Implements `specs/requirements.md` per `specs/design.md`. Each task is self-contained for a
fresh agent: read CLAUDE.md + the two specs + `specs/research-dossier.json` (and
`specs/verification-report.json` for pitfalls), then this block.
Milestone: **Phase 0–2 = a working MVP presence** (zero-config, JSONL-only). Phases 3–5 add the
statusLine/hooks push path, install/lifecycle, and polish.

## Phase 0: Scaffold

### [x] 0.1 Cargo project + deps + FULL module skeleton + CLI
Initialize the binary crate. `Cargo.toml`: package + deps (design §2): tokio (features
rt-multi-thread, macros, net, sync, time), notify = "8", sysinfo, libproc, nix (features for
`kill`/`getpeereid` — `signal`, `user`), serde (+derive), serde_json, thiserror,
discord-rich-presence = "1.1", toml, clap (+derive), tracing, tracing-subscriber (+env-filter),
tracing-appender, directories; plus an optional `tray` feature gating tray-icon + tao.
**Create the FULL module skeleton so the crate compiles end-to-end from the start**: `src/main.rs`
declaring every module (`mod error; mod logging; mod config; mod privacy; mod claude; mod ingest;
mod state; mod discord; mod install; mod platform;` and `#[cfg(feature = "tray")] mod tray;`) plus a
`clap` CLI with subcommands `run | install | uninstall | status | doctor | forward` (stubs returning
Ok/"not implemented", except `run`; `forward` is internal, not user-facing). Create an EMPTY stub
file for every module path in design §3, with the needed `mod`/`pub mod` lines in each `mod.rs`:
`src/error.rs`, `src/logging.rs`, `src/config.rs`, `src/privacy.rs`,
`src/claude/{mod,schema,sessions,transcript,pricing,activity}.rs`,
`src/ingest/{mod,socket,events}.rs`, `src/state/{mod,model,aggregator}.rs`,
`src/discord/{mod,sink}.rs`, `src/install/{mod,paths,launchd,hooks,statusline}.rs`,
`src/platform/{mod,macos}.rs`, `src/tray.rs`. Bake the default `client_id = 1518007333324587168`
("CC") where config defaults will live. The empty skeleton MUST pass all verify commands.
- **AC**: FR-8/AC-2
- **Files**: `Cargo.toml`, `src/` (full skeleton — every file above)
- **Depends**: -
- **Scope**: broad

### [x] 0.2 Config model + privacy settings + defaults
`Config` (TOML): `client_id`, `plan_label`, `capacity`, `min_interval` (2.5s), `keepalive_interval`
(15s), field toggles, `assets` (large/small keys), `tool_verbs`, `subagent_recency_secs`,
`show_ai_title` (default false), `buttons` (opt-in), and a `[privacy]` section (`redact`,
`blacklist_paths`, `scrub_bash_args`). Load from `~/.config/claude-presence/config.toml` with
built-in defaults; missing config must not crash. Changes apply on restart (no hot reload).
- **AC**: FR-7/AC-1, FR-7/AC-3
- **Files**: `src/config.rs`, `src/privacy.rs`
- **Depends**: 0.1

### [x] 0.3 Core domain types
Define the shared domain types in `src/state/model.rs`: `Activity {verb, target, small_image_key}`,
`SessionState`, and the `PresenceModel` skeleton (design §4.2). No behavior — just the types the
collectors and aggregator compile against. `started_at_ms: i64` (epoch milliseconds).
- **AC**: FR-5/AC-1
- **Files**: `src/state/model.rs`
- **Depends**: 0.1

### [x] 0.4 Error enum + sanitized logging
`src/error.rs`: crate-wide `Error` (thiserror). `src/logging.rs`: tracing-subscriber init with a
rotating file appender under the 0700 state dir, files 0600. The log layer MUST NOT emit raw
`tool_input`, prompt/transcript text, full paths, or unredacted statusline JSON (FR-8/AC-4) — wire
it to use the same sanitizers as Discord output.
- **AC**: FR-8/AC-4, NFR-3
- **Files**: `src/error.rs`, `src/logging.rs`
- **Depends**: 0.1

## Phase 1: Claude data layer

### [x] 1.1 Schema adapters (transcript / sessions / statusline / hook)
`claude/schema.rs`: serde structs + tolerant parsers for an assistant transcript line
(`message.model`, `message.usage{input,output,cache_creation,cache_read}`, `message.content[].tool_use`,
`message.id`, `stop_reason`, top-level `cwd`,`gitBranch`,`sessionId`,`timestamp`,`isSidechain`),
`sessions/<PID>.json`, the statusLine JSON (`cost.total_cost_usd`, `cost.total_duration_ms`,
`context_window.used_percentage`, `context_window.context_window_size`, `model.display_name`,
`version`, `effort.level`), and the hook JSON. Gate on `version`; unknown → degrade + log, never
panic. Unit-test against fixtures captured from `specs/research-dossier.json`.
- **AC**: FR-2/AC-3, FR-2/AC-4, FR-3/AC-2, C-4
- **Files**: `src/claude/schema.rs`, `tests/fixtures/`
- **Depends**: 0.1

### [x] 1.2 Pricing & context-window table + cost/ctx% calc
`claude/pricing.rs`: model-id → per-MTok prices (input/output/cache_read/cache_create) and
`context_window` (Opus 4.8/4.7/4.6 & Sonnet 4.6 = 1_000_000; Opus 4.5/Sonnet 4.5/Haiku 4.5 = 200_000).
`cost(usage)` and `ctx_pct(live_ctx_tokens, window)`; prefer a provided `ctx_size` when present.
Externalize the table (embedded TOML/const) for easy updates. Unit-test the worked example
(input131,out12570,cacheRead59709,cc1h10493 @ Opus4.8 ⇒ ≈$0.4497).
- **AC**: FR-3/AC-3, FR-3/AC-4
- **Files**: `src/claude/pricing.rs`
- **Depends**: 0.1

### [x] 1.3 Session discovery & liveness
`claude/sessions.rs`: enumerate engines via `sysinfo` exe-path filter (`/claude-code/` + ends
`/MacOS/claude`), exclude GUI/disclaimer/Discord; read `sessions/<PID>.json`; `kill(pid,0)` liveness;
resolve cwd; map cwd→slug→newest `*.jsonl` and cross-check sessionId; return `Vec<LiveSession>`.
**sysinfo gotcha**: refresh with `ProcessRefreshKind::nothing().with_exe(UpdateKind::Always).with_cwd(UpdateKind::Always).with_cpu()`
over `ProcessesToUpdate::All` before reading `exe()`/`cwd()` (else cwd/exe are `None`); fall back to
libproc when cwd is empty. Never use `pgrep` or argv `--resume`.
- **AC**: FR-1/AC-1, FR-1/AC-2, FR-1/AC-3, FR-1/AC-4
- **Files**: `src/claude/sessions.rs`, `src/platform/macos.rs`
- **Depends**: 1.1

### [x] 1.4 Activity mapping (tool → sanitized verb + badge)
`claude/activity.rs`: map `tool_name` → `Activity{verb,target,small_image_key}`. Default emits a
verb only (Bash→"Running <program>" using the first command token only, args DROPPED; Edit/Write→
"Editing <basename>"; Read→"Reading <basename>"; Grep/Glob→"Searching"; Agent/Task→"Orchestrating
agents"; mcp__*→"Using <server>"). Behind `scrub_bash_args`, optionally show a scrubbed command via
privacy.rs (strip token/key/secret/password/Authorization, WORD=value, credentialed URLs, long
base64/hex), then truncate. Apply path→basename + blacklist.
- **AC**: FR-2/AC-2, FR-7/AC-2
- **Files**: `src/claude/activity.rs`
- **Depends**: 1.1, 0.2, 0.3

### [x] 1.5 Transcript watcher (tail + derive per-session state)
`claude/transcript.rs`: `notify` watch on live transcript files; incrementally parse appended lines
(tolerate truncated final line); dedupe streaming partials on `message.id`; derive model, branch,
tokens, live-context tokens, current activity (via 1.4), busy/idle (pending tool_use without
tool_result), and subagent count (recent `subagents/**/agent-*.jsonl` mtimes). Emit `SessionState`.
- **AC**: FR-2/AC-1, FR-2/AC-2, FR-2/AC-5, FR-2/AC-6
- **Files**: `src/claude/transcript.rs`
- **Depends**: 1.1, 1.2, 1.4, 0.3

## Phase 2: Aggregation + Discord (MVP)

### [x] 2.1 Aggregator (focus, party, format, truncate, empty-state)
`state/aggregator.rs`: build `PresenceModel` from the session set; pick focus = most-recently-active
(sticky window); `party.size = [live_count, capacity]` (capacity = config max, default live_count);
subagent count surfaced in `state`/`small_text`, never in party. Format `details`/`state` with the
≤128 truncation ladder (design §5: abbreviate model → abbreviate plan → drop ctx%→tokens→cost).
`started_at_ms` in ms; assets/buttons. Empty-state: zero sessions → signal clear. Debounce over a
`watch` channel.
- **AC**: FR-5/AC-1, FR-5/AC-2, FR-5/AC-3, FR-5/AC-4, FR-5/AC-5, FR-5/AC-6, C-3
- **Files**: `src/state/aggregator.rs`
- **Depends**: 1.3, 1.5, 0.3

### [x] 2.2 Discord IPC sink (connect, set, reconnect, keepalive)
`discord/sink.rs`: run the sync `discord-rich-presence` 1.1 client on a dedicated thread fed by the
aggregator's `watch` channel; probe `discord-ipc-0..9`, handshake, `SET_ACTIVITY` (set
`timestamps.start` from `started_at_ms` in **milliseconds**; map party/assets/buttons), clear on
shutdown and on empty-state; debounce (`min_interval`), keepalive republish, reconnect with backoff;
retry if no socket at startup. **API note**: `DiscordIpcClient::new(id)` returns `Self` (no `?`);
methods return `Result<(), discord_rich_presence::error::Error>`. Must compile against the pinned 1.1.0.
- **AC**: FR-6/AC-1, FR-6/AC-2, FR-6/AC-3, FR-6/AC-4, FR-6/AC-5, FR-5/AC-6, C-2
- **Files**: `src/discord/sink.rs`
- **Depends**: 2.1

### [x] 2.3 Wire `run` end-to-end (MVP, JSONL-only)
`run` initializes logging (0.4), acquires the single-instance lock (flock on the state dir; exit
clearly if another instance is live), boots config + sessions/transcript collectors + aggregator +
sink; clean shutdown clears presence. First visible Discord card driven purely by sessions+transcript.
- **AC**: FR-8/AC-1, FR-8/AC-3
- **Files**: `src/main.rs`
- **Depends**: 2.2, 0.4

## Phase 3: Hybrid push (statusline + hooks)

### [x] 3.1 Ingest socket server + `forward` subcommand
`ingest/`: tokio unix-socket server at the daemon socket path (0600, inside 0700 dir, peer-uid
verified via getpeereid); parse newline-delimited `IngestEvent::{Hook, StatusLine}`; sanitize then
forward into the aggregator (statusLine overrides computed cost/ctx%/model for its session; hooks
set immediate activity). Implement the `forward` CLI subcommand that pipes stdin→socket (no external
dep). Never log raw payloads (FR-8/AC-4).
- **AC**: FR-3/AC-1, FR-3/AC-2, FR-4/AC-1, FR-4/AC-3, FR-8/AC-4
- **Files**: `src/ingest/socket.rs`, `src/ingest/events.rs`, `src/main.rs`
- **Depends**: 2.1, 2.3

### [x] 3.2 statusline-wrapper chain-install
`install/statusline.rs` + `assets/statusline-wrapper.sh`: at install, read and STORE the user's
current `statusLine.command`; install the wrapper that execs the stored inner command (stdout
passthrough) and tees the stdin JSON to `claude-presence forward --kind statusline`. Claude Code
injects NO custom env var — the original command is baked into the wrapper / a fixed state path.
Uninstall restores the original only if the current value still equals our wrapper (drift handling),
else surgically removes our segment and warns.
- **AC**: FR-3/AC-1, C-6, FR-8/AC-3
- **Files**: `src/install/statusline.rs`, `assets/statusline-wrapper.sh`
- **Depends**: 3.1

### [x] 3.3 Hooks chain-install
`install/hooks.rs` + `assets/hook-forward.sh`: for each of `SessionStart/PreToolUse/PostToolUse/Stop/
SubagentStart/SubagentStop`, read the existing `hooks[<Event>]`; create the group if absent; APPEND
`{type:command, command:"claude-presence forward --kind hook"}` into the existing group's `hooks[]`,
preserving the user's entries (e.g. `Stop` → `afplay … Submarine.aiff`). Uninstall removes ONLY our
exact command entry by identity. Add a fixture test using the real afplay Stop entry.
- **AC**: FR-4/AC-1, FR-4/AC-2, FR-4/AC-3, C-6
- **Files**: `src/install/hooks.rs`, `assets/hook-forward.sh`
- **Depends**: 3.1

## Phase 4: Lifecycle, install, doctor

### [x] 4.1 launchd user agent
`install/launchd.rs` + `assets/LaunchAgent.plist.tmpl`: render the plist (keys in design §3.1:
Label, ProgramArguments=[abs path, "run"], RunAtLoad, `KeepAlive={SuccessfulExit:false}`,
StandardOut/ErrorPath, ProcessType=Background; NO EnvironmentVariables) to `~/Library/LaunchAgents`;
`launchctl bootstrap gui/$(id -u)`. `uninstall` runs `launchctl bootout gui/$(id -u)` BEFORE the
process exits (so it can't relaunch after clearing presence) and removes the plist.
- **AC**: FR-8/AC-1, FR-8/AC-2, FR-8/AC-3
- **Files**: `src/install/launchd.rs`, `assets/LaunchAgent.plist.tmpl`
- **Depends**: 2.3

### [x] 4.2 `install` / `uninstall` / `status` / `doctor`
Compose 3.2+3.3+4.1 into `install`; `uninstall` reverts every chained change exactly (restore-or-warn
on drift); `status` shows detected sessions + Discord connection; `doctor` checks Discord socket,
settings wiring, config validity, single-instance conflicts, and the buttons-on-own-profile caveat,
with actionable diagnostics.
- **AC**: FR-8/AC-2, FR-8/AC-3, NFR-6
- **Files**: `src/main.rs`, `src/install/launchd.rs`, `src/install/hooks.rs`, `src/install/statusline.rs`
- **Depends**: 4.1, 3.2, 3.3
- **Scope**: broad

## Phase 5: Polish

### [x] 5.1 Privacy hardening + tests
Verify no raw prompt/file/secret content can reach Discord OR logs. Tests: a Bash command containing
a fake token never appears in `details`; `ai-title` is suppressed unless `show_ai_title` and project
not blacklisted; the log formatter omits command/paths/raw JSON; buttons never emit a `file://` URL.
Add a global "private mode" generic card.
- **AC**: FR-7/AC-2, FR-8/AC-4, NFR-3, C-7
- **Files**: `src/privacy.rs`, `tests/privacy.rs`
- **Depends**: 2.1, 1.4

### [x] 5.2 End-to-end smoke test
`tests/e2e.rs`: feed fixture sessions through the aggregator and assert a non-empty, correct
`PresenceModel` (details/state ≤128, party.size shape, ms timestamp), plus the empty-state clear path.
- **AC**: NFR-2, FR-5/AC-6
- **Files**: `tests/e2e.rs`
- **Depends**: 2.3

### [x] 5.3 Menu-bar tray (optional)
`tray-icon`+`tao` menu-bar control: on/off, pause, current summary, quit. Behind `--features tray`.
- **AC**: FR-9/AC-1
- **Files**: `src/tray.rs`, `Cargo.toml`
- **Depends**: 2.3

### [x] 5.4 README + sample config + asset-key list
User-facing setup: create the Discord app, asset keys to upload, `install`/`uninstall`, config reference.
- **AC**: FR-7/AC-1
- **Files**: `README.md`, `config.example.toml`
- **Depends**: 4.2
