# Requirements: claude-presence Hardening

> Follow-up hardening spec. Closes all 41 adversarially-verified findings from the
> `cp-deep-audit` workflow (0 critical, 0 high, 10 medium, 24 low, 7 info). The full
> finding dossier (location / impact / verifier reasoning / recommended fix per item)
> is the authoritative reference; findings are cited below as **F1–F41**.

## Goal

Eliminate every confirmed defect while preserving 100% of existing behavior, the
spec-driven product contract (`specs/requirements.md`, `specs/design.md`), and the
CLAUDE.md conventions. No feature changes — only correctness, privacy, reliability,
and performance hardening. The C-7 privacy invariant is the highest priority: **no
raw path, secret, or prompt fragment may reach Discord or the logs.**

## Constraints (inherited, non-negotiable)

- **C-1** Rust edition 2021; `cargo fmt --check`, `cargo clippy -- -D warnings`, and
  `cargo test` must all pass. No new clippy allowances.
- **C-2** No `unwrap()`/`expect()`/`panic!` on any runtime/daemon path. Errors via
  `thiserror`; logs via `tracing` (never `println!` in the daemon path).
- **C-3** Privacy is non-negotiable (C-7/ADR-8): only structured, sanitized fields
  leave the process; full paths reduce to basename; secrets are scrubbed; blacklist
  and redact switches are honored on **every** field, in the card **and** the logs.
- **C-4** Installers chain (append), never overwrite, the user's statusLine + hooks,
  and every install action has an exact, tested uninstall. Settings writes stay
  atomic + durable (fsync + rename).
- **C-5** Internal `~/.claude` schema reads go only through `src/claude/schema.rs`
  adapters; on unknown/changed schema degrade — never panic.
- **C-6** The module skeleton is fixed: do not add/remove `mod` declarations or edit
  module wiring (`mod.rs`/`main.rs`) unless a task's Files list says so.
- **C-7** Every change ships with a unit/integration test proving the fix (especially
  the privacy invariants), added in the same file's `#[cfg(test)]` module or
  `tests/`.

## Functional Requirements

### FR-1 — Privacy & secret sanitization hardening (C-7)
The sanitizers must never let an identifying path or a real-world secret reach the
public Discord card or the logs.

- **AC-1** `privacy::scrub_token`/`redact_text` reduce a token to its **basename**
  ONLY as the **last** sanitization stage — after the URL-credential/query strip and
  the secret-blob/known-secret checks have run — and ONLY when the token is
  unambiguously a filesystem path (leading `/` or `~`, or contains `/` AND is not a
  `scheme://…` URL, not a `key=value`/`key:value` pair, not a known MIME type/scheme).
  Order: known-secret/blob redaction → `key=value`/`key:value` redaction → URL
  credential+query strip → path basename. So
  an AI title or any sanitized text carrying a full filesystem path leaks only the
  basename, while a credentialed URL still redacts its credentials (not its path).
  Proven by tests: (a) `redact_text("Refactoring /Users/x/secret/auth.rs")` has no
  `/`-prefixed path component; (b) `"https://user:pw@host/x/secret.rs"` →
  `"https://[redacted]@host/x/secret.rs"` (credentials stripped, NOT basename'd);
  (c) `"application/json"` and an `"a/b"` option token pass through unmangled. (F6)
- **AC-2** The AI-generated session title is suppressed from the card whenever the
  user has hidden the project (`privacy.fields.project = false`), mirroring the
  existing branch gate — not only when global `redact`/blacklist is set. Proven by a
  test combining `fields.project = false` + `show_ai_title = true`. (F9)
- **AC-3** `looks_like_secret_blob` and the Bash-arg scrubber detect common real
  secret formats regardless of `_`/`.` content or 32-char length: GitHub PATs
  (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_`), OpenAI keys (`sk-`/`sk-proj-`),
  Slack (`xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-`), AWS access-key ids (`AKIA`/`ASIA`,
  20 chars), Google (`AIza`), and JWT shape (three `.`-separated base64url segments,
  first starting `eyJ`). The blob charset includes `_` and `.`. The exact prefix list
  is identical in requirements/design/tasks. Proven by a test per format. (F7, F8)
- **AC-4** Secrets carried in a URL query string and sensitive header keys written
  with hyphens (`x-api-key`, `api-key`) are redacted: `is_sensitive_key` normalizes
  `-`→`_` before matching, and URL query parameters whose name is sensitive — at
  minimum `token`, `access_token`, `api_key`, `apikey`, `secret`, `password`, `key`,
  `sig`, `signature` (the names the dossier F30 example uses) — have their value
  replaced with `[redacted]`. Proven by a test that
  `"https://api/v1?access_token=sk-SECRET&sig=abc"` redacts both values. (F30)
- **AC-5** Blacklist matching is case-insensitive on macOS and **never regresses**
  today's lexical match (the new match is a superset). The LIVE cwd is matched
  **without any filesystem syscall** (no `fs::canonicalize` on the hot path): match by
  **component-prefix on case-folded `OsStr` components** (case-folded on macOS, exact
  elsewhere), so the component-boundary rule (`/a/b` ∌ `/a/bc`) is preserved by
  per-component equality, not by `str`/`Path::starts_with` on a joined string. Any
  symlink/tilde/canonical normalization is applied **once at config-load to the
  blacklist ENTRIES only** — never to the live cwd — so symlink resolution can never
  move a cwd out of a blacklisted root. Proven by tests: a differently-cased path
  under a blacklisted root collapses to generic; `/a/bc` does not match a `/a/b`
  entry; a path that matches lexically today still matches. (F29, F31)
- **AC-6** Log sanitization is real, not aspirational. **Canonical resolution** (one
  model, no either/or): `redact_text` becomes a genuine path+secret sanitizer (the
  path stage of AC-1 plus the secret formats of AC-3), so its name, docstring, and the
  `redact_text_strips_secrets_and_paths_for_logs` test are truthful; `privacy.rs` owns
  the `redact_text`/`ai_title` docstrings + that test (task 1.1), and `logging.rs`
  documents that the privacy module is the sanitizer of record and points at
  `redact_text` (task 1.16). No log call site interpolates a raw path/secret; verified
  by inspection of every `tracing` call that takes user-derived text. (F27)

### FR-2 — Discord IPC reliability
The single Discord writer must respect Discord's rate limit and never let a
half-open Discord wedge daemon shutdown.

- **AC-1** Sustained presence updates never exceed Discord's ~5 `SET_ACTIVITY`/20s
  budget. A **rolling-window rate limiter is the sole guarantee**: it counts **all**
  publishes together — distinct updates, the keepalive republish, and the
  on-(re)connect publish — and blocks a publish that would exceed 5 in any rolling
  20s. The default `min_interval` is raised to **4.0s** (the debounce floor), and the
  same 4.0s floor applies to the sink's fallback for an invalid (`0`/NaN/inf)
  `min_interval` (no silent fallback below the budget). The `keepalive_interval` is
  clamped to `>= min_interval` so a misconfigured keepalive cannot undercut the floor.
  Proven by: a sink test that `min_interval = 0.0` resolves to the 4.0s floor; a
  limiter test that ≤5 publishes occur in any rolling 20s **including** keepalive +
  on-connect; a config test that `keepalive_interval` clamps up to `min_interval`.
  (F3, F19)
- **AC-2** A clean shutdown clears the Discord presence and completes within a
  bounded **3 seconds** even when the IPC handshake/read is blocked on a half-open
  Discord: the sink-thread join in `lib.rs` is wrapped in
  `tokio::time::timeout(Duration::from_secs(3), …)`; on elapse the daemon logs and
  exits anyway (the OS reaps the wedged thread). (F2, F4)
- **AC-3** The reconnect backoff loop is interruptible by the shutdown signal even
  while a synchronous `connect()` is in flight (run the blocking connect such that a
  shutdown can pre-empt the backoff/handshake). (F12)
- **AC-4** During shutdown the collector loop stops re-enumerating sessions /
  spawning `lsof` before the blocking sink join, so teardown does no useless work.
  (F13)
- **AC-5** The serve loop and keepalive do not clone the full `PresenceModel`/
  `PresenceUpdate` (incl. the sessions `Vec`) every iteration when nothing changed.
  (F20, F37)
- **AC-6** An overlay-only wakeup does **not** trigger a fresh `sessions::discover()`
  (sysinfo enumeration + `lsof` cwd resolution); `discover()` runs only on the
  discovery-interval tick, while overlay wakeups rebuild the snapshot from the
  already-held `live`/`watchers` set, and a burst of overlays coalesces into a single
  rebuild. (Publishing is already gated on a card-relevant change via `sessions_eq` at
  `lib.rs:286` — that gate is kept; this AC removes the redundant per-overlay
  enumeration.) Proven by a test asserting `discover()` is not called on an
  overlay-only wake. (F40)

### FR-3 — Installer robustness (shell-safe, atomic, no-panic)
- **AC-1** `statusLine.command` and hook command entries are written shell-safely:
  the absolute binary/script path is quoted/escaped so an install path containing a
  space or shell metacharacter neither breaks Claude Code nor allows injection.
  (F5, F23)
- **AC-2** The launchd binary path is XML-escaped before substitution into the
  plist, so `&`/`<`/`>` in the path yields a well-formed plist `launchctl` loads.
  (F25)
- **AC-3** The wrapper script and state file are written atomically (temp + fsync +
  rename) so a re-install never exposes a truncated/partial executable to Claude
  Code while `settings.json` already points at it. (F26)
- **AC-4** `install/hooks.rs::apply_install` no longer relies on `.expect()`
  invariants that could panic; it degrades gracefully on an unexpected settings
  shape. (F24)
- **AC-5** `Config::save` removes its `.toml.tmp` scratch file on any write/fsync
  failure (no turd left behind). (F11)

### FR-4 — Transcript collector performance & correctness
- **AC-1** The watcher no longer re-reads and re-parses the entire transcript on
  every FS event / 500ms tick: it maintains **carried derived state across ticks** and
  bounds the per-line work to the last **64 KiB** of the transcript, so steady-state
  CPU is bounded by recent activity, not total session length. **NFR-1 must hold under
  default config** — i.e. the windowing may NOT change the card. Specifically:
  - The **sticky latest-wins fields** — `model`, ai-`title`, git `branch`, **and the
    co-located metrics `usage`, `tokens_total`, `context_tokens`, and `activity`** (the
    fields feeding "N K tok", "Ctx X%", and the small-icon activity tooltip) — are
    seeded once from a full-file derive at watch start and **carried forward** when a
    bounded re-derive yields `None` for them, preferring a freshly-derived non-`None`
    value (a multi-MB session whose model/usage/title lines precede the window still
    reports the correct model, token total, ctx%, and title — the pricing/ctx%
    denominator is never blanked).
  - An **in-flight turn** (a `user` opener with no terminal assistant `stop_reason`
    yet) and any **unresolved `tool_use`** continue to report `working = true` /
    `busy = true` even when the opener / originating `tool_use` line has aged out of
    the window — `in_turn` and the pending-tool_use set are carried across ticks, not
    recomputed from a naive slice. Only a **fully-completed** (terminal `stop_reason`)
    turn older than the window may degrade to idle.
  - `message.id` dedupe and token totals stay correct for recent lines.
  Proven by tests: a multi-MB transcript whose model + ai-title lines precede the tail
  window still reports the correct `state.model`/`state.title` (non-`None` window
  denominator); an in-flight turn whose opener predates the window still reports
  `working`/`busy`. The full incremental `Tail` rewire is out of scope (ADR-4). (F1)
- **AC-2** Each transcript line is deserialized at most once per derive (no second
  `serde_json::from_str` of the same line). (F17)
- **AC-3** `Tail::read_appended` never returns `Err(InvalidData)` when an appended
  chunk ends mid-UTF-8 character: an incomplete trailing byte sequence is buffered
  and completed on the next read. (F18)

### FR-5 — Session discovery efficiency & PID-reuse safety
- **AC-1** The `sysinfo` `System` instance is reused across discovery ticks (refresh
  in place) instead of being recreated every 3s, so CPU sampling is warm and kernel
  work is not duplicated. (F16)
- **AC-2** The registry `pid` field is cross-checked against the enumerated/filename
  PID before a session is treated as live, so PID reuse cannot resurrect a stale
  session. (F36)

### FR-6 — Aggregator & pricing correctness
- **AC-1** `format_tokens` uses saturating arithmetic (no `tokens + 500` overflow on
  a saturated `u64` total). (F32)
- **AC-2** The pricing/context-window model matcher requires a delimiter boundary
  (the matched key must be followed by `-` or end-of-string), so a future
  `claude-opus-4-50` cannot silently inherit `claude-opus-4-5`'s window/price. (F15)
- **AC-3** The aggregator's inner debounce loop does not busy-spin when
  `min_interval` is configured to 0/negative/non-finite: `aggregate_channel` clamps a
  non-positive/non-finite `cfg.min_interval` to a non-zero **2.5s coalesce floor**
  before computing the debounce, so the coalesce window is never `Duration::ZERO`. (The
  aggregator's own `duration_from_seconds` currently returns `0` for bad input — it must
  gain this fallback. This anti-busy-spin floor is independent of and need not equal the
  sink's 4.0s `FALLBACK_MIN_INTERVAL` rate floor.) Proven by a test that `min_interval = 0.0` (and NaN/negative) yields the
  default debounce, not zero. (F33)

### FR-7 — Ingest socket hardening
- **AC-1** Each ingest connection has an idle/total read timeout and the server caps
  concurrent connection tasks at **16** (a `tokio::sync::Semaphore` of 16 permits;
  connections over the cap are dropped), so a slowloris peer or a connection flood
  cannot exhaust file descriptors or pin tasks forever. The same-uid `getpeereid`
  check and the "never log raw bytes" guarantee are preserved. (F22, F39)
- **AC-2** An oversized unterminated frame closes the connection (not just clears the
  buffer and keeps reading). (F39)
- **AC-3** `StatuslineFrame`/`HookFrame` fields that are parsed but never used
  (`cwd`/`effort`/`ctx_size`/`version`) are either consumed or dropped, and the
  module docstring's false blacklist-path claim is corrected. (F21)
- **AC-4** `StatuslineFrame::from_value` does not clone the entire JSON `Value` on
  every frame. (F38)

### FR-8 — Platform process hygiene
- **AC-1** The `lsof` cwd fallback runs with a bounded timeout and is invoked by an
  **absolute trusted path** (`/usr/sbin/lsof`), not a bare name resolved through
  `$PATH`, so a hijacked `$PATH` cannot substitute a malicious `lsof` and a hung
  `lsof` cannot stall discovery. (F28, F41)

### FR-9 — Tray correctness & dead-code disposition
- **AC-1** The tray `Quit` action triggers the daemon's graceful shutdown (clear the
  Discord presence) instead of hard-exiting the process, so FR-8/AC-3 of the product
  spec holds when the (optional, off-by-default) tray feature is enabled. (F10)
- **AC-2** The unreferenced tray module is dispositioned with an objective marker: a
  module-level doc comment in `src/tray.rs` explicitly states the tray is
  intentionally unwired/deferred, and the whole module is gated under
  `#[cfg(feature = "tray")]` so a default build links none of it. Verified by
  `cargo build` (default) carrying no tray symbols and the doc comment being present.
  (F34)

### FR-10 — Dependency / supply-chain hygiene
- **AC-1** `cargo audit` (or, if uninstalled, a recorded `Cargo.lock` version review)
  for the **default feature set** reports zero advisories reachable without
  `--features tray`; the GTK/`proc-macro-error`/`uuid 0.8.2` advisories are documented
  in a `Cargo.toml` comment block as tray-only / upstream, off-by-default, with a
  tracked disposition; the audit output is recorded in `verification.md`. Per F14/F35
  the **default dependency set is NOT changed** — no removal is required. (F14, F35)

## Non-Functional Requirements

- **NFR-1 (no regression)** All existing tests (6 e2e + 14 privacy + unit/doc) keep
  passing; the public behavior of the card under default config is unchanged.
- **NFR-2 (CPU bounded)** Per-tick transcript work is bounded by the 64 KiB window +
  newly-appended bytes, NOT by total session length (FR-4): a test/derivation shows a
  multi-MB transcript re-derives from at most a 64 KiB slice per tick. Discovery reuses
  one `sysinfo::System` across ticks rather than reallocating it (FR-5). No per-match
  filesystem syscall on the blacklist hot path (FR-1/AC-5).
- **NFR-3 (privacy testable)** Every privacy AC (FR-1) ships with a test that would
  fail against today's code, locking the invariant in.
- **NFR-4 (reversibility)** Installer changes preserve exact, tested install↔uninstall
  round-trips (no drift in the chained statusLine/hooks contract).
- **NFR-5 (degrade, never crash)** Every new failure path (timeouts, malformed input,
  unexpected settings shape) degrades to a reduced/cleared state and logs — no panic.
