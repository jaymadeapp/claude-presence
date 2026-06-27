# AGENTS.md — Codex-presence

Rust daemon (macOS) that aggregates live Codex activity into a single Discord Rich Presence.
Spec-driven: read `specs/requirements.md` → `specs/design.md` → `specs/tasks.md` before touching code.
Verified research backing every decision: `specs/research-dossier.json` (13 agents, live-tested on
Codex v2.1.181). No code is written without a corresponding task + acceptance criteria.

## Commands
- `cargo fmt --check` · `cargo clippy -- -D warnings` · `cargo test` — the verify gate (must all pass).
- `cargo run -- run` — run the daemon in the foreground.
- `cargo run -- doctor` — diagnose Discord socket / sessions / settings wiring.

## Conventions
- Edition 2021, `rustfmt` defaults, clippy clean (`-D warnings`). Prefer `thiserror` for errors,
  `tracing` for logs (never `println!` in the daemon path). No `unwrap()`/`expect()` on runtime paths.
- **The full module skeleton (every `mod` declaration + empty stub files) is created in task 0.1.**
  Later tasks FILL IN an already-declared file from their `Files` list. Do NOT add/remove `mod`
  declarations or edit `mod.rs`/`main.rs` module wiring unless your task's `Files` explicitly lists
  it — those are owned by other tasks and editing them breaks parallel waves.
- Internal `~/.Codex` schema reads go **only** through `src/Codex/schema.rs` adapters, gated on
  `version`. On unknown/changed schema: degrade to a reduced card and log — never panic.
- The only Anthropic-stable contracts are the **statusLine JSON** and **hooks JSON**; treat
  transcript/sessions layouts as version-pinned best-effort.
- Installers MUST chain (never overwrite) the user's existing statusline + hooks, and every install
  action MUST have an exact, tested uninstall (see C-6, FR-8/AC-3).
- Privacy is non-negotiable (C-7): only structured, sanitized fields leave the process; never emit
  raw prompt text, file contents, or full paths (basename only); honor the repo/path blacklist.

## Hard-won facts (don't relearn — verified, see specs/verification-report.json)
- Enumerate sessions by **exe-path filter** via `sysinfo`, not `pgrep -f` (truncates long argv).
  sysinfo `cwd()`/`exe()` are **None unless** you refresh with `ProcessRefreshKind` `with_cwd`/`with_exe(UpdateKind::Always)`.
- Source of truth for the session set: `~/.Codex/sessions/<PID>.json` + `kill(pid,0)` liveness.
  argv `--resume/--session-id` is often **stale** — never use it for the active sessionId.
- PID→cwd via `lsof -d cwd` / libproc (no `--cwd` in argv).
- Transcripts are open-append-close (safe to tail with `notify`), per-message sub-second. Cost is
  **not** stored — compute from `usage × pricing`. Dedupe streaming partials on `message.id`.
- Opus 4.8 context window is **1M**, not 200k (wrong denominator inflates ctx% ~5×).
- Discord = one presence per app per user → aggregate all sessions into one card.
  `party.size = [live_count, capacity]` (NOT busy_count). `timestamps.start` is **epoch MILLISECONDS**
  (seconds → ~1000× wrong timer). `discord-rich-presence` **1.1**: `DiscordIpcClient::new(id)` returns
  `Self` (no `?`); buttons may not render on your OWN profile over local IPC.
- busy/idle primary signal = last assistant `tool_use` without a matching `tool_result`; CPU% is weak.
- statusLine data arrives on **stdin** (CC injects no custom env var); chain by **storing** the
  original command and baking it into the wrapper. Chain hooks by **appending** into the existing
  `hooks[<Event>].hooks[]` group and removing only our entry by identity (preserve the user's afplay Stop hook).
- Privacy: never let raw `tool_input`/prompt/paths/secrets reach Discord **or logs**; Bash args are
  dropped by default; `ai-title` is off by default.

## Project specifics (real values to bake in)
- Discord application: name **"CC"** (Discord blocked "Codex"/"Codex"), **client_id = `1518007333324587168`**.
  Use this as the default `client_id` in config. The bold name on the card is "CC"; put "Codex"
  in `large_text` (tooltip).
- Assets (uploaded by the user in the Developer Portal → Rich Presence → Art Assets): `large_image`
  = the app picture; `small_image` = the Codex asterisk ("Codex"). Images are OPTIONAL — the MVP
  must run and show a valid card even before asset keys exist (omit `large_image`/`small_image` if unset).

## Orchestrated execution
When running `specs/tasks.md` via the `executing-plans` skill, spawn subagents on **Opus 4.8**
(`Codex-opus-4-8`) unless overridden. Respect the `Depends`/`Files` graph for safe parallelism.
