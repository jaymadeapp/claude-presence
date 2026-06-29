# Tasks: claude-presence Hardening

> Verify: `cargo fmt --check` `cargo clippy --all-targets -- -D warnings` `cargo test`

Closes findings F1–F41 (see `specs/hardening/requirements.md` for ACs and the audit
dossier for full per-finding detail: location, impact, verifier reasoning, fix).

**Global rules for every task** (a fresh agent reads only its block + CLAUDE.md + the
spec):
- Make the **smallest** change that closes the cited findings; do not alter behavior
  under default config; do not reformat unrelated lines.
- Add the proving test(s) **in the touched module's own `#[cfg(test)]` block** — do
  NOT edit the shared `tests/e2e.rs` / `tests/privacy.rs` (other tasks/the review own
  those). Every privacy AC needs a test that would fail against today's code.
- Do not add/remove `mod` declarations or edit module wiring (C-6). Keep `thiserror`/
  `tracing`; no `unwrap`/`expect`/`panic!` on runtime paths (C-2).
- All three `> Verify:` commands must pass before marking `[x]`.

## Phase 1: Per-file hardening (all independent — disjoint files, one parallel wave)

### [x] 1.1 Harden the privacy sanitizers in `privacy.rs`
Implement FR-1 sanitizer fixes, all inside `src/privacy.rs`:
- **Path stripping (F6):** add a path-basename stage to `scrub_token` that runs **as
  the LAST stage** — after the secret-blob/known-prefix detection, the `key=value`/
  `key:value` redaction, AND the URL credential-strip + query-param redaction — and
  only when the token is unambiguously a filesystem path (leading `/` or `~`; or
  contains `/` AND is not a `scheme://…` URL, not a `key=value`/`key:value`, not a
  known MIME type/scheme). Reduce it to its basename, mirroring
  `claude::activity::path_target`. This is what `redact_text` and `scrub_bash_command`
  map over, so it closes the AI-title path leak and the log path leak at once **without**
  mangling a credentialed URL (which is credential-stripped, not basename'd) or a
  `KEY=/secret/path` (redacted by the `key=value` branch first).
- **Secret coverage (F7, F8):** add `_` and `.` to `looks_like_secret_blob`'s charset
  and add length-independent known-prefix detection (`ghp_/gho_/ghu_/ghs_/ghr_/
  github_pat_`, `sk-/sk-proj-`, `xoxb-/xoxa-/xoxp-/xoxr-/xoxs-`, `AKIA/ASIA`, `AIza`)
  plus a JWT shape (3 `.`-split base64url segments, first starting `eyJ`); `scrub_token`
  redacts on either. Anchor prefixes at token start to bound false positives.
- **URL query + header keys (F30):** normalize `-`→`_` in `is_sensitive_key`; after
  credential stripping, redact `k=v` query params whose key is sensitive — at minimum
  `token`, `access_token`, `api_key`, `apikey`, `secret`, `password`, `key`, `sig`,
  `signature` — inside a URL query string.
- **Blacklist matching (F29, F31):** add `path_matches_blacklist(cwd, entry)` that
  case-folds each `OsStr` **component** on macOS and does direct **component-prefix**
  matching (succeed iff `entry` is a component-prefix of `cwd`); used by
  `is_blacklisted`/`project_label`. Keep the `/a/b` ∌ `/a/bc` rule via per-component
  equality. **Do NOT call `fs::canonicalize` on the live cwd** (no per-match syscall;
  no symlink-resolution regression). (Blacklist **entry** normalization, i.e. `~`
  expansion, is owned by task 1.4 in `config.rs` — this matcher receives already-expanded
  entries.) The match must be a **superset** of today's lexical `starts_with` (never make
  a currently-matching path stop matching).
- **Doc/test truth (F27, privacy side — this file only):** make the existing
  `redact_text_strips_secrets_and_paths_for_logs` test actually assert a full path is
  reduced to a basename (true after this task); make `redact_text`/`ai_title`
  docstrings truthful (do not touch logging behavior — that is task 1.16).
Add tests: path→basename in `redact_text`; each secret format redacted; a credentialed
URL `https://user:pw@host/x/secret.rs` → `https://[redacted]@host/x/secret.rs` (NOT
basename'd); `application/json` and an `a/b` option token pass through unmangled
(R1/R3); a sensitive URL query param redacted; blacklist matches under case variants
and still matches what it matches lexically today, but not `/a/bc`.
- **AC**: FR-1/AC-1, FR-1/AC-3, FR-1/AC-4, FR-1/AC-5, FR-1/AC-6
- **Files**: `src/privacy.rs`
- **Depends**: -

### [x] 1.2 Gate AI-title + fix token/debounce math in `aggregator.rs`
Inside `src/state/aggregator.rs`:
- **AI-title project gate (F9):** change the ai-title guard (~L369) from
  `if !private {` to `if !private && cfg.privacy.fields.project {`, matching the
  existing branch-field gate.
- **Token overflow (F32):** `format_tokens` uses `tokens.saturating_add(500)`.
- **No zero debounce at interval 0 (F33):** in `aggregate_channel`, clamp a
  non-positive/non-finite `cfg.min_interval` to a non-zero 2.5s coalesce floor (the
  aggregator's own anti-busy-spin floor — independent of the sink's 4.0s rate floor)
  before computing the debounce (≈`aggregator.rs:273`); the aggregator-side
  `duration_from_seconds`
  (≈`aggregator.rs:690`) currently returns `Duration::ZERO` for bad input and must gain
  the fallback. The inner coalesce loop must never sleep zero in a hot loop.
Add tests: `fields.project = false` + `show_ai_title = true` + a non-empty title →
title suppressed from `details`; `format_tokens(u64::MAX)` does not panic;
`min_interval = 0.0` (and NaN/negative) yields the 2.5s default debounce, not
`Duration::ZERO`.
- **AC**: FR-1/AC-2, FR-6/AC-1, FR-6/AC-3
- **Files**: `src/state/aggregator.rs`
- **Depends**: -

### [x] 1.3 Add a rolling-window rate limiter + clone/interrupt fixes in `sink.rs`
Inside `src/discord/sink.rs` (ADR-1, ADR-2 context):
- **5/20s ceiling (F3, F19):** add a `VecDeque<Instant>` of recent publish times;
  before EVERY `publish` (update, keepalive, on-(re)connect), drop entries older than
  20s and, if 5 remain, wait (interruptibly via the shutdown watch) until the oldest
  ages out. Keep `min_interval` as the debounce floor. Replace the hardcoded
  `Duration::from_millis(2500)` fallback at `sink.rs:103` with a **local** sink
  constant `const FALLBACK_MIN_INTERVAL: Duration = Duration::from_secs(4);` (comment:
  "must match `config::DEFAULT_MIN_INTERVAL_SECS` = 4.0, set by task 1.4") so an invalid
  `min_interval` cannot fall back below the budget — defined in `sink.rs` only, no
  `config.rs` edit. Also gate the keepalive publish behind
  `debounce_remaining(*last_publish_at, min_interval)` so a keepalive cannot fire inside
  the debounce window (F19). Fix the misleading comment that claims `min_interval` alone
  bounds the budget.
- **Interruptible connect (F12):** run the synchronous `client.connect()` so a shutdown
  signal pre-empts a wedged handshake/backoff (e.g. `select!` the backoff against the
  shutdown watch; the blocking connect itself may use `spawn_blocking`).
- **Clone reduction (F20, F37):** avoid deep-cloning `PresenceUpdate`/`PresenceModel`
  each serve-loop turn and on keepalive; clone only when actually publishing a changed
  value.
Add/extend tests: the window limiter caps at ≤5 publishes in any rolling 20s INCLUDING
keepalive + on-connect; `min_interval = 0.0` resolves to the 4.0s `FALLBACK_MIN_INTERVAL`
(extend `duration_from_seconds_falls_back_on_bad_input`); a keepalive cannot publish
inside the debounce window.
- **AC**: FR-2/AC-1, FR-2/AC-3, FR-2/AC-5
- **Files**: `src/discord/sink.rs`
- **Depends**: -

### [x] 1.4 Raise `min_interval` default + atomic-save cleanup in `config.rs`
Inside `src/config.rs`:
- **Default min_interval (F3):** `DEFAULT_MIN_INTERVAL_SECS` 2.5 → **4.0** so
  steady-state spacing yields ≤5/20s (keep this value in sync with the sink's local
  `FALLBACK_MIN_INTERVAL` from task 1.3). Update the `defaults_are_safe` /
  `partial_toml_*` / `round_trips_through_toml` tests that assert `2.5`.
- **Keepalive clamp (F19):** after load / in `from_toml` (or a normalize step), clamp
  `keepalive_interval = keepalive_interval.max(min_interval)` so a misconfigured
  keepalive cannot undercut the rate floor.
- **Blacklist entry normalization (F29/F31):** in the same one-time normalize step over
  the deserialized config (`from_toml`/`load`), expand a leading `~` in each
  `blacklist_paths` entry to the home dir (best-effort; leave the entry as-is if the
  home dir can't be resolved). This is the config-load entry normalization the
  `privacy.rs` matcher (task 1.1) relies on; keep it `~`-expansion only (do NOT resolve
  symlinks, to preserve the superset guarantee). Add a test that a `~/private` entry is
  expanded to the home-relative absolute path.
- **tmp cleanup (F11):** in `Config::save`, on any error after the temp file exists,
  best-effort `remove_file(&tmp)` before returning the error.
Add tests: a save failure leaves no `.toml.tmp` (or unit-test the cleanup helper);
`keepalive_interval = 1.0` with `min_interval = 4.0` clamps keepalive up to 4.0.
- **AC**: FR-2/AC-1, FR-3/AC-5, FR-1/AC-5
- **Files**: `src/config.rs`
- **Depends**: -

### [x] 1.5 Bounded shutdown join + ordering + publish-on-change in `lib.rs`
Inside `src/lib.rs`:
- **Bounded join (F2, F4):** wrap the `spawn_blocking(|| sink_handle.join())` in
  `tokio::time::timeout(Duration::from_secs(3), …)`; on elapse `warn!` and fall through
  to exit (drop the handle).
- **Shutdown ordering (F13):** `abort()` the collector (and ingest) handle BEFORE the
  blocking sink join so the collector stops re-enumerating / spawning `lsof` during
  teardown. Confirm the presence-clear path still works (the sink owns its last model).
- **Skip discover() on overlay wakes (F40):** publishing is already gated by
  `sessions_eq` at `lib.rs:286` (keep it). The waste F40 names is that
  `sessions::discover()` (sysinfo enumeration + `lsof`) re-runs on every overlay wake.
  Hoist `discover()` to run only on the discovery-interval **ticker** arm of the
  `select!`; the **overlay** arm rebuilds the snapshot from the already-held
  `live`/`watchers` set (reuse the last `live`), and an overlay burst still coalesces
  into one rebuild. Do NOT change `discover()`'s signature (task 1.7 owns
  `sessions.rs`).
Add/adjust tests: `discover()` is not called on an overlay-only wake (e.g. via a call
counter / refactor that makes this observable); the bounded-join timeout path is
time-boxed if practical (a fake join that never returns).
- **AC**: FR-2/AC-2, FR-2/AC-4, FR-2/AC-6
- **Files**: `src/lib.rs`
- **Depends**: -

### [x] 1.6 Bound re-derive + single-parse + UTF-8-safe tail in `transcript.rs`
Inside `src/claude/transcript.rs` (ADR-4):
- **Bounded re-derive with carried state (F1):** cap the per-line work to the last
  **64 KiB** via the existing `read_tail_lines` instead of `read_all_lines`, but
  **carry derived state across ticks** so the cap can NEVER change the card (NFR-1).
  The loop already seeds `state` from a full-file derive at start (`transcript.rs:812`);
  on each tick MERGE the bounded re-derive into the carried `state`:
  - Carry the **sticky latest-wins** fields forward — `model`, ai-`title`, git `branch`,
    **and the metrics `usage`, `tokens_total`, `context_tokens`, `activity`** — prefer a
    freshly-derived non-`None` value, else keep the carried one (these are emitted once /
    only on recent lines and scroll out of a 64 KiB window; losing `model`/`usage` would
    blank the card model, token total, and the ctx%/pricing denominator).
  - Carry the **open-turn marker and the pending-`tool_use` id set** across ticks (do
    NOT recompute `in_turn`/`busy` from the naive slice): an in-flight turn whose `user`
    opener or originating `tool_use` line aged out of the window must still report
    `working`/`busy`. A turn degrades to idle only when its terminal `stop_reason` is
    observed.
  - Keep `message.id` dedupe + token totals correct for recent lines.
  Do NOT attempt the full incremental `Tail` byte-offset rewire (deferred, ADR-4).
- **Single deserialize (F17):** in `parse_and_dedupe`, parse each line once (one
  `serde_json` call) and derive both the typed needs and any `Value` needs from it.
- **UTF-8-safe tail (F18):** in `Tail::read_appended`, retain an incomplete trailing
  UTF-8 byte sequence in the buffer (use `Utf8Error::valid_up_to()`) and complete it on
  the next read instead of returning `Err(InvalidData)`.
Add tests: a multi-MB transcript whose `model`/`usage`/ai-`title` lines precede the
64 KiB tail window still reports the correct `state.model`/`state.title` and non-`None`
`tokens_total`/`context_tokens`/`activity` after a bounded tick; an in-flight turn whose
`user` opener predates the window still reports `working`/`busy`; a completed turn older
than the window degrades to idle; a chunk split mid multibyte char tails cleanly.
- **AC**: FR-4/AC-1, FR-4/AC-2, FR-4/AC-3
- **Files**: `src/claude/transcript.rs`
- **Depends**: -

### [x] 1.7 Reuse sysinfo + PID cross-check in `sessions.rs`
Inside `src/claude/sessions.rs` only (do NOT change `discover()`'s signature or
`lib.rs`):
- **sysinfo reuse (F16):** keep a module-level `std::sync::OnceLock<Mutex<sysinfo::System>>`
  (or equivalent owned-in-module state) refreshed in place each tick instead of
  `System::new*` per call; keep the exe-path filter and `with_cwd`/`with_exe` refresh
  kinds.
- **PID cross-check (F36):** cross-check the registry `pid` field against the
  filename-derived / enumerated PID; on mismatch prefer the authoritative enumerated
  PID and log at debug, so PID reuse cannot resurrect a stale session.
Add tests for the PID-mismatch resolution.
- **AC**: FR-5/AC-1, FR-5/AC-2
- **Files**: `src/claude/sessions.rs`
- **Depends**: -

### [x] 1.8 Delimiter-boundary model matcher in `pricing.rs`
Inside `src/claude/pricing.rs`: require a matched prefix key to be followed by `-` or
end-of-string in the pricing/effective-window lookup (or switch to exact-id keys with an
explicit family fallback), so `claude-opus-4-50` cannot inherit `claude-opus-4-5`'s
window/price (F15). Add a test asserting that exact case.
- **AC**: FR-6/AC-2
- **Files**: `src/claude/pricing.rs`
- **Depends**: -

### [x] 1.9 Shell-safe + atomic statusline installer in `statusline.rs`
Inside `src/install/statusline.rs`:
- **Shell-safe command (F5, ADR-3, NFR-4):** wrap the stored absolute wrapper path
  written into `statusLine.command` in the existing `shell_single_quote` so
  spaces/metacharacters are inert. Handle the **two uninstall identity branches
  distinctly**: the **equality** branch (`statusline.rs:223`) compares against the
  **quoted** `wrapper_invocation()` (exact round-trip), AND also accepts the **legacy
  bare** path (a pre-quoting install stored the unquoted path) so an upgrade-then-
  uninstall still restores cleanly; the **contains/drift** branch (`statusline.rs:239`)
  matches membership against the **unquoted raw** `wrapper_path()` so a drifted value
  embedding the bare path still drops the dangling key. `is_wired` recognizes both the
  quoted and the legacy bare form.
- **Atomic artifacts (F26):** write the wrapper script and state file via the existing
  `write_atomic` (temp+fsync+rename) and set their final mode after rename.
Update/extend tests: the round-trip + wiring tests for the quoted form; a drifted value
embedding the BARE path (`format!("{BARE_WRAPPER} ; echo extra")`) drops the
`statusLine` key; a legacy settings value whose command is exactly the bare path
(pre-quoting install) uninstalls without leaving a dangling reference.
- **AC**: FR-3/AC-1, FR-3/AC-3
- **Files**: `src/install/statusline.rs`
- **Depends**: -

### [x] 1.10 Shell-safe + no-panic hooks installer in `hooks.rs`
Inside `src/install/hooks.rs`:
- **Shell-safe command (F23):** write the hook command entry's script path shell-safely
  (same quoting approach as 1.9); keep remove-by-identity exact against the same form.
- **No-panic apply (F24):** replace the `.expect()` invariants in
  `ensure_object_at`/`ensure_array_at` with graceful handling (callers guarantee an
  object root via `read_settings`; normalize/return instead of `expect`). No runtime
  panic path remains.
Update the install/uninstall identity tests for the quoted form; add a test that a
degenerate settings shape does not panic.
- **AC**: FR-3/AC-1, FR-3/AC-4
- **Files**: `src/install/hooks.rs`
- **Depends**: -

### [x] 1.11 XML-escape the launchd plist path in `launchd.rs`
Inside `src/install/launchd.rs`: add a small `xml_escape` (`&`/`<`/`>`/`"`/`'`) and
apply it to the binary path before substitution into the plist `<string>` so a path
with `&`/`<`/`>` yields a well-formed plist `launchctl` loads (F25). Add a test with a
path containing `&`.
- **AC**: FR-3/AC-2
- **Files**: `src/install/launchd.rs`
- **Depends**: -

### [x] 1.12 Read timeout + concurrency cap in ingest `socket.rs`
Inside `src/ingest/socket.rs`:
- **Read timeout + cap (F22):** add a per-read idle timeout and a total-connection
  deadline via `tokio::time::timeout`; bound in-flight connection tasks with a
  `tokio::sync::Semaphore` (e.g. 16 permits) so a flood cannot exhaust fds/tasks; over
  the cap, drop the new connection. Keep the same-uid `getpeereid` check and "never log
  raw bytes" guarantee.
- **Oversize frame closes (F39):** the oversized-unterminated-frame branch must `break`
  (close the connection), not clear-and-continue.
Add tests for the oversize-close and (if practical) the idle-timeout path.
- **AC**: FR-7/AC-1, FR-7/AC-2
- **Files**: `src/ingest/socket.rs`
- **Depends**: -

### [x] 1.13 Drop unused frame fields + avoid full clone in ingest `events.rs`
Inside `src/ingest/events.rs`:
- **Unused fields + doc (F21):** remove (or consume) the parsed-but-unused
  `StatuslineFrame.cwd/effort/ctx_size/version` and `HookFrame.cwd`; fix the docstring's
  false blacklist-path claim.
- **No full Value clone (F38):** `StatuslineFrame::from_value` should move/borrow the
  fields it needs rather than cloning the entire `serde_json::Value`.
Keep `Overlay`/`apply_to` behavior identical; update tests as needed.
- **AC**: FR-7/AC-3, FR-7/AC-4
- **Files**: `src/ingest/events.rs`
- **Depends**: -

### [x] 1.14 Bounded, absolute `lsof` in `platform/macos.rs`
Inside `src/platform/macos.rs`: invoke `/usr/sbin/lsof` by absolute path (not a bare
name resolved via `$PATH`) and run it with a bounded wait so a hung `lsof` cannot stall
the 3s discovery tick (F28, F41). Preserve the existing cwd-resolution result on
success; on timeout/error return `None` and log at debug. Add a test for the absolute
path / timeout handling where feasible.
- **AC**: FR-8/AC-1
- **Files**: `src/platform/macos.rs`
- **Depends**: -

### [x] 1.15 Graceful tray Quit + dead-code disposition in `tray.rs`
Inside `src/tray.rs` (feature `tray`, off by default, wiring intentionally deferred):
- **Graceful Quit (F10):** the `Quit` handler must invoke a passed-in shutdown hook
  (e.g. accept a `tokio::sync::watch::Sender<bool>` or `FnOnce`) that triggers the
  daemon's graceful clear-and-exit, instead of `std::process::exit`. Change `run_tray`'s
  signature to take the hook (no caller exists yet, so this is safe).
- **Dead-code disposition (F34):** add a module doc note marking the tray as
  intentionally-unwired/deferred and ensure the default build carries nothing (it is
  already `#[cfg(feature = "tray")]`).
- **Verify**: `cargo fmt --check` `cargo clippy --all-targets --features tray -- -D warnings` `cargo test --features tray`
- **AC**: FR-9/AC-1, FR-9/AC-2
- **Files**: `src/tray.rs`
- **Depends**: -

### [x] 1.16 Correct log-sanitization docs in `logging.rs`
Inside `src/logging.rs`: correct the module/file docstrings to state that user-derived
values are sanitized via the privacy module's `redact_text` (its finalized,
path+secret-stripping behavior from task 1.1) before logging, and that the privacy
module is the sanitizer of record — do NOT claim an independent "by construction" model.
Verify by inspection that no log call site interpolates a raw path/secret (F27, logging
side). No behavior change beyond doc truth.
- **AC**: FR-1/AC-6
- **Files**: `src/logging.rs`
- **Depends**: 1.1
- **Notes**: depends on 1.1 only so the docstring references the finalized `redact_text`
  contract (different file, so this just orders 1.16 after 1.1 — no file conflict).

### [x] 1.17 Document tray-only advisories + record cargo-audit in `Cargo.toml`
Inside `Cargo.toml`: add a comment block documenting that the GTK/`proc-macro-error`
advisories and the heavy duplicated transitive tree are reachable ONLY via the optional,
off-by-default `tray` feature (and that legacy `uuid 0.8.2` is pulled transitively by
`discord-rich-presence`, upstream). Record `cargo audit` output for the DEFAULT feature
set in `specs/hardening/verification.md` (append), confirming zero advisories reachable
without `--features tray`. **Do not change the default dependency set** and do NOT trim
feature flags (that would contradict F14/F35 — no removal is required).
- **AC**: FR-10/AC-1
- **Files**: `Cargo.toml`, `specs/hardening/verification.md`
- **Depends**: -
- **Notes**: `cargo audit` may not be installed; if absent, note that and review
  `Cargo.lock` versions instead.

## Phase 2: Review & Finalize

### [x] 2.1 Deep code-review (workflow), apply fixes, run full suite
The primary agent runs a multi-agent code-review **Workflow** over the entire diff from
tasks 1.1–1.17, then applies every confirmed fix and re-runs the suite.
- Run by the primary agent as a `Workflow` (subagents cannot spawn workflows). Fan out
  reviewers by dimension — correctness, **C-7 privacy/secret-leak**, acceptance-criteria
  conformance, concurrency/shutdown, installer reversibility, performance/allocation —
  then have independent verify agents **adversarially re-check each finding** and drop
  false positives before anything is applied. Do NOT use `/code-review ultra`.
- Apply all confirmed (verified) patches to the working tree.
- Run every `> Verify:` command (plus `--features tray` for the tray-touching files);
  all must pass before marking `[x]`.
- **AC**: (all — guards the whole hardening pass)
- **Files**: `**`
- **Depends**: 1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8, 1.9, 1.10, 1.11, 1.12, 1.13, 1.14, 1.15, 1.16, 1.17
- **Scope**: broad
- **Run**: orchestrator-workflow

### [x] 2.2 Final verification & sign-off
The plan's final task: confirm the full gate is green and every AC is met.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, and the same three with `--features tray`.
- Smoke `cargo run -- doctor` to confirm the socket/settings wiring still reports
  correctly. Spot-check that default-config card behavior is unchanged (no field
  regressions) and that the privacy invariant tests from FR-1 are present and passing.
- Append a short results summary to `specs/hardening/verification.md` (findings closed,
  gate status, anything deferred).
- **AC**: (all — final sign-off)
- **Files**: `specs/hardening/verification.md`
- **Depends**: 2.1
