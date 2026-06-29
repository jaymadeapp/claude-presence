# Design: claude-presence Hardening

How each requirement is satisfied, file by file, with the design decisions that need
justification captured as ADRs. The guiding principle: **smallest change that closes
the finding without altering observable behavior under default config.** Every change
is local to one module's internals; no public signatures change unless noted, so the
work parallelizes by file (see `tasks.md`).

## Architecture impact

None structural. The pipeline (collectors → aggregator → debounced presence → Discord
sink, with the ingest overlay path) is unchanged. All work tightens existing
functions, adds guards, and adds tests. Module wiring (`mod.rs`/`main.rs`) is
untouched (C-6).

## Per-area design

### A. Privacy sanitizers — `src/privacy.rs`, `src/logging.rs`, `src/state/aggregator.rs` (FR-1)

The sanitizers are pure functions over borrowed primitives; harden them in place.

- **Path-stripping (AC-1, F6).** Add a path-basename stage to `scrub_token` that runs
  **last** — only AFTER `looks_like_secret_blob`/`looks_like_known_secret`, the
  `key=value`/`key:value` redaction, and `strip_url_credentials` + query-strip have all
  had their chance — and only fires on an **unambiguous filesystem path**: leading `/`
  or `~`; or contains `/` AND is not a `scheme://…` URL, not a `key=value`/`key:value`
  pair, not a known MIME type/scheme. It then reduces the token to its last
  `/`-delimited segment (basename), mirroring `claude::activity::path_target`. Because
  `redact_text` and `scrub_bash_command` both map `scrub_token` over whitespace tokens,
  this closes the AI-title path leak **and** the log path leak in one place. This
  ordering is consistent with R1 ("URL handling first"): a credentialed URL is
  credential-stripped (not basename'd), and a `KEY=/secret/path` is redacted by the
  `key=value` branch before it can reach the path stage.
- **AI-title project gate (AC-2, F9).** In `aggregator.rs` (~L369) change the
  ai-title guard from `if !private {` to `if !private && cfg.privacy.fields.project {`
  — exactly the gate the branch field already uses (L354). `private` stays
  `redact || blacklisted`; the per-field toggle now also suppresses the title.
- **Secret coverage (AC-3, F7/F8).** Extend `looks_like_secret_blob`: add `_` and `.`
  to the allowed charset, and add a length-independent `looks_like_known_secret`
  helper matching explicit prefixes (`ghp_`,`gho_`,`ghu_`,`ghs_`,`ghr_`,`github_pat_`,
  `sk-`,`sk-proj-`,`xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-`,`AKIA`,`ASIA`,`AIza`) and a
  JWT shape (three `.`-split base64url segments, first starting `eyJ`). `scrub_token`
  redacts a token matching either. Prefixes are anchored at token start to bound false
  positives.
- **URL query + header keys (AC-4, F30).** In `is_sensitive_key`, normalize
  `-`→`_` after the existing `trim_start_matches('-').to_ascii_lowercase()`, so
  `x-api-key`/`api-key` hit the `api_key` needle. For URLs, after credential
  stripping, also scan the query string: split on `?`/`&`, and for any `k=v` whose
  key `is_sensitive_key`, replace `v` with `[redacted]`.
- **Blacklist matching (AC-5, F29/F31).** Add `path_matches_blacklist(cwd, entry)`
  that case-folds each `OsStr` **component** on macOS (exact elsewhere) and does direct
  **component-prefix** matching (iterate components, compare folded equality, succeed
  iff `entry` is a component-prefix of `cwd`). **Do NOT call `fs::canonicalize` on the
  live `cwd`** — no syscall on the per-aggregation / per-`tool_use` hot path, and no
  risk that symlink resolution moves a cwd out of a blacklisted root (which would
  *regress* a currently-working C-7 match). `~`-expansion of the blacklist **ENTRIES**
  is applied **once at config-load** by the `config.rs` owner (task 1.4) — symlinks are
  deliberately NOT resolved, to preserve the superset property; the `privacy.rs` matcher
  (task 1.1) receives already-expanded entries. The new match is a **superset** of
  today's lexical `starts_with` (it never makes a previously-matching path stop
  matching). The component-boundary rule (`/a/b` ∌ `/a/bc`) falls out of
  per-component equality, not `str::starts_with`. Both `is_blacklisted` call sites
  (`aggregator.rs:317` `is_private`, `privacy.rs:60` `project_label`) use this matcher;
  the cwd is fed as-is (no syscall).
- **Log sanitization truth (AC-6, F27).** One canonical model: after AC-1/AC-3,
  `redact_text` **is** a genuine path+secret sanitizer, so its name, the
  `redact_text_strips_secrets_and_paths_for_logs` test (owned by task 1.1 in
  `privacy.rs`), and its docstring are truthful. `logging.rs` (task 1.16) documents
  that the privacy module is the sanitizer of record and points at `redact_text`
  rather than claiming an independent "by construction" model. Audit every `tracing`
  call that interpolates a user-derived value to confirm none logs a raw path/secret.
  Task 1.16 `Depends: 1.1` so its docstring references the finalized `redact_text`
  contract rather than racing it.

### B. Discord IPC reliability — `src/discord/sink.rs`, `src/lib.rs`, `src/config.rs` (FR-2)

- **Rate limiter (AC-1, F3/F19).** The rolling window is the **sole** rate guarantee.
  Bump `DEFAULT_MIN_INTERVAL_SECS` 2.5 → **4.0** in `config.rs` (task 1.4).
  Independently, in `sink.rs` (task 1.3) replace the hardcoded `Duration::from_millis(2500)`
  fallback at `sink.rs:103` with a **local** `const FALLBACK_MIN_INTERVAL: Duration =
  Duration::from_secs(4);` (no `config.rs` import, no shared-file edit) so an invalid
  `min_interval` (0/NaN/inf) cannot fall back below the budget — the two 4.0s values are
  kept disjoint on purpose so tasks 1.3 and 1.4 stay file-disjoint for the parallel wave;
  they must agree by comment, not by import. Clamp
  `keepalive_interval = keepalive_interval.max(min_interval)` at config load (F19), and
  gate the keepalive publish behind `debounce_remaining(*last_publish_at, min_interval)`
  so a keepalive cannot fire inside the debounce window. Add a small rolling-window
  limiter in the sink: a `VecDeque<Instant>` of the last publish timestamps; before
  **any** `publish` (update, keepalive, on-connect), drop entries older than 20s and,
  if 5 remain, wait until the oldest ages out (interruptible by shutdown). This counts
  all three publish sources together — which `min_interval` alone (and the default-4.0s
  floor alone, given a 15s keepalive) does not guarantee. `min_interval` remains the
  debounce floor; the window is the hard ceiling. (ADR-1)
- **Bounded shutdown join (AC-2, F2/F4).** In `lib.rs` (~L119) wrap the
  `spawn_blocking(|| sink_handle.join())` in `tokio::time::timeout(Duration::from_secs(3), …)`.
  On elapse, `warn!` and fall through to process exit (drop the handle; the OS reaps
  the wedged thread). The in-crate socket read timeout cannot be set
  (`discord-rich-presence` keeps the `UnixStream` private), so the bounded join is the
  correct in-tree fix. (ADR-2)
- **Interruptible connect (AC-3, F12).** Run the synchronous `client.connect()` via
  `tokio::task::spawn_blocking` (or a `select!` against the shutdown watch around the
  backoff) so a shutdown during a wedged handshake pre-empts the backoff loop instead
  of waiting for the blocking call to return.
- **Shutdown ordering (AC-4, F13).** In `run()` move the `collector_handle.abort()`
  (and ingest abort) to **before** the blocking sink join, so the collector is not
  re-enumerating sessions / spawning `lsof` while we wait to clear the presence.
  (Re-verify the presence-clear path still has the data it needs — the sink owns its
  own last model, so aborting the collector first is safe.)
- **Clone reduction (AC-5, F20/F37).** In the serve loop, avoid cloning the whole
  `PresenceUpdate` each turn — compare/borrow and only clone when publishing a changed
  value; keepalive republishes from the already-held `last_published` without a fresh
  deep clone.
- **Skip discover() on overlay wakes (AC-6, F40).** Publishing is already gated on a
  card-relevant change by `sessions_eq` at `lib.rs:286` (kept). The residual waste F40
  names is that `sessions::discover()` (sysinfo enumeration + `lsof` cwd resolution)
  re-runs on *every* overlay wake. Hoist `discover()` so it runs only on the
  discovery-interval **ticker** arm of the `select!`; the **overlay** arm rebuilds the
  snapshot from the already-held `live`/`watchers` set (reusing the last `live`), and a
  burst of overlays still coalesces into a single rebuild. Test: `discover()` is not
  called on an overlay-only wake.

### C. Installer robustness — `src/install/statusline.rs`, `src/install/hooks.rs`, `src/install/launchd.rs`, `src/config.rs` (FR-3)

- **Shell-safe command (AC-1, F5/F23).** Claude Code runs `statusLine.command` and
  hook commands through a shell. Wrap the stored absolute path in the existing
  POSIX-safe `shell_single_quote` (already in `statusline.rs`) so spaces/metacharacters
  in the install path are inert. The uninstall identity match compares against the
  same quoted form, preserving exact reversibility. (ADR-3)
- **Plist XML escaping (AC-2, F25).** Add a tiny `xml_escape` for `&`/`<`/`>`/`"`/`'`
  and apply it to the binary path before it is substituted into the plist `<string>`.
- **Atomic artifact writes (AC-3, F26).** Reuse the existing `write_atomic`
  (temp+fsync+rename) for the wrapper script and state file, and set their final mode
  after rename, so a re-install never exposes a partial executable.
- **No-panic apply_install (AC-4, F24).** Replace the `.expect()` invariants in
  `ensure_object_at`/`ensure_array_at` with graceful handling (the callers already
  guarantee an object root via `read_settings`; make the helpers return/normalize
  instead of `expect`, or assert the invariant once at the boundary with a typed
  error). No runtime panic path remains.
- **tmp cleanup (AC-5, F11).** In `Config::save`, on any error after the temp file is
  created, `remove_file(&tmp)` (best-effort) before returning the error.

### D. Transcript collector — `src/claude/transcript.rs` (FR-4)

- **Bounded re-derive with carried state (AC-1, F1).** Bound the **per-line** work to
  the last 64 KiB via the existing `read_tail_lines`, but **carry derived state across
  ticks** so the cap can never change the card (NFR-1). The watch loop already seeds
  `state` from a full-file `derive_state` at startup (`transcript.rs:812`); make each
  tick **merge** the bounded re-derive into the carried `state` instead of replacing
  it wholesale (`transcript.rs:868`):
  - **Sticky latest-wins fields** — `model`, ai-`title`, git `branch`, **and the
    co-located metrics `usage`, `tokens_total`, `context_tokens`, `activity`** — are
    latest-wins values emitted once (or only on recent assistant/tool lines) that can
    scroll out of a 64 KiB tail. Carry them all forward: prefer a freshly-derived
    non-`None` value, else keep the carried one. (A multi-MB session whose
    model/usage/title lines precede the window keeps reporting the real model, token
    total, ctx%, activity + title, so the card and the pricing/ctx% denominator are
    never blanked — CLAUDE.md's "wrong denominator inflates ctx% ~5×" footgun is
    avoided.)
  - **Turn / busy state** — `in_turn` is opened by a `user` line and closed only by a
    later assistant line with a terminal `stop_reason`; an unresolved `tool_use` makes
    `busy`. Do **not** recompute these from a naive last-N-KiB slice (the opener or the
    originating `tool_use` line can scroll out while the model is *still working*,
    flipping a busy session to idle under default config). Carry the open-turn marker
    and the pending-`tool_use` id set across ticks and seed `derive_state` with them;
    a turn only degrades to idle once a terminal `stop_reason` for it is observed.
  - `message.id` dedupe + token totals stay correct for recent lines.
  Net: cost is O(64 KiB + appended) per tick, and the card output is byte-identical to
  the full-file derive under default config. The full incremental `Tail` rewire (carry
  a byte offset, never re-read the window) is the ideal but higher-risk follow-up —
  out of scope here (ADR-4).
- **Single deserialize (AC-2, F17).** `parse_and_dedupe` currently parses each line as
  `TranscriptLine` and again as `serde_json::Value`. Parse once to `Value` (or once to
  the typed struct) and derive both needs from the single parse.
- **UTF-8-safe tail (AC-3, F18).** In `Tail::read_appended`, on a chunk that ends
  mid-UTF-8, retain the incomplete trailing bytes in the struct's buffer and prepend
  them to the next read instead of `str::from_utf8`-erroring. Use
  `std::str::from_utf8`'s `Utf8Error::valid_up_to()` to split the valid prefix from the
  incomplete suffix.

### E. Session discovery — `src/claude/sessions.rs` (FR-5)

- **sysinfo reuse (AC-1, F16).** Hold a `sysinfo::System` across discovery ticks
  (in the collector loop's owned state or a `OnceCell`/passed-in handle) and
  `refresh_*` it in place each tick instead of `System::new*` every call, so CPU
  sampling is warm. Keep the exe-path filter + `with_cwd`/`with_exe` refresh kinds.
- **PID cross-check (AC-2, F36).** When reading `sessions/<PID>.json`, compare the
  registry `pid` field against the filename-derived PID (and the enumerated live PID);
  on mismatch, prefer the authoritative enumerated PID and log at debug, so a stale
  registry `pid` (post PID-reuse) cannot mislabel a session.

### F. Aggregator & pricing — `src/state/aggregator.rs`, `src/claude/pricing.rs` (FR-6)

- **Saturating tokens (AC-1, F32).** `format_tokens`: `tokens.saturating_add(500)`.
- **Delimiter-boundary matcher (AC-2, F15).** In the pricing/window lookup, require
  the char after a matched prefix key to be `-` or end-of-string (or switch the table
  to exact model ids with an explicit family fallback). Add a test that
  `claude-opus-4-50` does not match `claude-opus-4-5`.
- **No zero debounce at interval 0 (AC-3, F33).** The aggregator's own
  `duration_from_seconds` (≈`aggregator.rs:690`) has **no floor** today — it returns
  `Duration::from_millis(0)` for non-positive/non-finite input (unlike the sink's
  `duration_from_seconds(seconds, fallback)` variant), so `aggregate_channel`
  (≈`aggregator.rs:273`) collapses the coalesce window to zero when `min_interval <= 0`
  and the inner loop (≈`aggregator.rs:281`) busy-spins. Fix: give the aggregator's
  `duration_from_seconds` a fallback — clamp a non-positive/non-finite `min_interval` to
  a safe non-zero coalesce floor `Duration::from_millis(2500)` before computing the
  debounce. (This is the aggregator's anti-busy-spin floor only; it intentionally need
  NOT equal the sink's `FALLBACK_MIN_INTERVAL` = 4.0s rate floor — the sink's rolling
  window is the independent rate ceiling.)

### G. Ingest socket — `src/ingest/socket.rs`, `src/ingest/events.rs` (FR-7)

- **Read timeout + concurrency cap (AC-1/AC-2, F22/F39).** Wrap each connection's
  read in a per-read idle timeout and a total-connection deadline via
  `tokio::time::timeout`; on elapse, close. Track in-flight connection tasks with a
  `tokio::sync::Semaphore` (bounded permits, e.g. 16) so a flood cannot spawn unbounded
  tasks/fds; over the cap, drop the new connection. Make the oversized-frame branch
  `break` (close) instead of `clear`-and-continue.
- **Unused frame fields + doc (AC-3, F21).** Remove the parsed-but-unused
  `cwd`/`effort`/`ctx_size`/`version` fields (or `#[allow]`/consume them) and fix the
  docstring's false blacklist-path claim.
- **No full Value clone (AC-4, F38).** `StatuslineFrame::from_value` should borrow /
  take ownership of fields it needs rather than cloning the whole `Value`.

### H. Platform — `src/platform/macos.rs` (FR-8)

- **Bounded, absolute `lsof` (AC-1, F28/F41).** Invoke `/usr/sbin/lsof` (absolute) and
  run it with a bounded wait (spawn + kill-on-timeout, or a short deadline) so a hung
  `lsof` cannot stall the 3s discovery tick and `$PATH` cannot hijack the binary.

### I. Tray — `src/tray.rs` (FR-9)

- **Graceful Quit (AC-1, F10).** The tray `Quit` handler must signal the daemon's
  shutdown channel (clear presence) rather than `std::process::exit`. Thread the
  existing shutdown `watch::Sender` (or a callback) into the tray so Quit flips it.
- **Dead-code disposition (AC-2, F34).** Since tray wiring is intentionally deferred,
  add a module-level doc note marking it unwired-by-design and gate the dead-code
  cleanly under the `tray` feature so a default build carries nothing; no behavior
  change to the default binary.

### J. Dependencies — `Cargo.toml` (FR-10)

- **Document + record (AC-1, F14/F35).** Record that the GTK/`proc-macro-error`
  advisories and the heavy transitive tree are reachable **only** via the optional,
  off-by-default `tray` feature; capture `cargo audit` for the default feature set in
  `verification.md`. **Do not change the default dependency set** (per F14/F35 no
  removal is required) — there is no "trim broader feature flags" action, which would
  contradict that. The legacy `uuid 0.8.2` pulled transitively by
  `discord-rich-presence` is noted as upstream and out of our control.

## ADRs

- **ADR-1 — Rolling-window rate limiter is the sole guarantee.** A bare `min_interval`
  floor cannot bound 5/20s because the keepalive (default 15s) and on-connect publishes
  are not counted against it: even at a 4.0s floor, a keepalive landing between two
  4.0s-spaced updates can produce 6 publishes in 20s. Raising the floor helps steady
  state but a reconnect burst can still stack. So the `VecDeque<Instant>` window that
  gates **all** publishes (updates + keepalive + on-connect) is the actual ceiling; the
  4.0s floor (incl. the sink fallback for invalid input) and the
  `keepalive >= min_interval` clamp are secondary defenses. Cost: a few timestamps and
  one extra wait branch.
- **ADR-2 — Bounded join, not in-crate socket timeout.** `discord-rich-presence` 1.1
  exposes no handle to set `set_read_timeout` on its private `UnixStream`, so the only
  in-tree fix that guarantees bounded shutdown is to time-box the sink-thread join in
  `lib.rs` and exit regardless. Trade-off: on a wedged Discord the presence may not be
  cleared (Discord will expire it), but the daemon always exits promptly.
- **ADR-3 — Quote the path; split the two uninstall identity branches.** Wrapping the
  stored path in `shell_single_quote` changes the literal written into `settings.json`,
  so uninstall must handle two cases distinctly: (a) the **equality** branch
  (`statusline.rs:223`) compares against the **quoted** `wrapper_invocation()` (our own
  install wrote exactly that — exact round-trip); (b) the **contains/drift** branch
  (`statusline.rs:239`) matches membership against the **unquoted raw** `wrapper_path()`
  (a drifted/hand-composed value won't carry our exact quoting), so a dangling segment
  is still dropped. **Migration:** an install written by the pre-quoting version stored
  the bare path; the new uninstall must still recognize it — accept the legacy bare
  form in equality (or let it fall through to the contains-on-bare-path branch). An
  upgrade-then-uninstall from a pre-quoting install leaves no dangling `statusLine`.
- **ADR-4 — Bounded per-line work + carried state; defer full incremental tail.**
  Capping only the per-line work to a 64 KiB window while **carrying the derived state
  forward** (sticky `model`/`title`/`branch` and the open-turn / pending-`tool_use`
  markers) is O(window) per tick AND keeps the card byte-identical to a full-file
  derive — the cap can never blank the model/title or flip an in-flight turn to idle.
  A naive window without carry would regress both (the spec-verification flagged this);
  the carried-state merge is the safe form. A true incremental `Tail` (carry a byte
  offset, never re-read the window) is the ideal but higher-risk change; deferred to a
  follow-up so this pass stays safe.

## Risks & mitigations

- **R1 — Over-aggressive path stripping breaks legitimate non-path tokens** (e.g. a
  URL path, a `a/b` regex). Mitigation: only strip tokens that are clearly filesystem
  paths (leading `/`/`~`, or contain `/` and look path-shaped), keep URL handling
  first, and add tests for URLs and option-like tokens.
- **R2 — A naive bounded re-derive would flip an *actively-thinking* session to idle
  and blank the model/title (a default-config card regression), not merely affect
  "long-idle" turns.** The `in_turn` opener (a `user` line) and an unresolved
  `tool_use` can scroll out of the window while the model is still working; the sticky
  `model`/`title` lines are emitted once and scroll out in any long session.
  Mitigation: **carry derived state across ticks** (sticky fields + open-turn /
  pending-`tool_use` markers) so the window bounds only per-line work and never the
  card output; assert with tests for the active-turn and pre-window-model/title cases
  (AC-1). Only a fully-completed (terminal `stop_reason`) turn older than the window
  degrades to idle.
- **R3 — Secret prefix patterns cause false positives** on ordinary tokens. Mitigation:
  anchor at token start, require the known prefix, and test that common non-secret
  tokens (`reading`, `https://github.com/...`) are not redacted.
- **R4 — Rate-limiter wait stalls shutdown.** Mitigation: every limiter wait is
  `select!`-ed against the shutdown watch (same pattern as `wait_or_shutdown`).
- **R5 — Reusing `sysinfo::System` across ticks holds memory / staleness.** Mitigation:
  refresh the specific process set each tick; only the allocation is reused.
