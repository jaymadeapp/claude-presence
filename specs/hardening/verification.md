# Spec Verification (Phase 4)

Multi-agent audit of `requirements.md` / `design.md` / `tasks.md` before any code, per
the spec-driven-development workflow. 7 dimension auditors → adversarial verification of
each finding. **29 confirmed** (4 high, 7 medium, 18 low), 0 rejected after verify. All
29 were applied to the spec; summary below.

## High (4) — applied

- **H1 (design-coverage) Bounded re-derive drops model/ai-title.** The original F1 fix
  capped the re-derive to a tail window, but `model`/ai-`title`/`branch` are single-value
  "latest-wins" fields emitted once that scroll out of the window → blanked model + a
  wrong/absent ctx% denominator. **Applied:** redesigned F1 to **carry derived state
  across ticks** and merge the bounded re-derive (FR-4/AC-1, design §D, ADR-4, task 1.6).
- **H2 (risk-security-edge) Bounded re-derive flips an active turn to idle.** The
  `in_turn` opener / unresolved `tool_use` can age out of the window while the model is
  still working → busy→idle under default config (NFR-1 violation). **Applied:** carry
  the open-turn marker + pending-`tool_use` set across ticks; only a terminal
  `stop_reason` degrades a turn (FR-4/AC-1, design §D + R2, task 1.6).
- **H3 (risk-security-edge) Blacklist canonicalization could REGRESS a C-7 match.**
  `fs::canonicalize` on the live cwd resolves symlinks and could move a cwd out of a
  blacklisted root (and adds a per-match syscall). **Applied:** no syscall on the cwd;
  component-prefix match on case-folded `OsStr` components; normalize blacklist
  **entries** once at config-load; the new match is a **superset** of today's lexical
  match (FR-1/AC-5, design §A).
- **H4 (dossier-coverage) Path-strip ordering contradicted URL/secret handling.** Design
  R1 said "URL handling first" but task 1.1 said path-strip "first". **Applied:** path
  basename is the **LAST** `scrub_token` stage, after secret/key=value/URL handling, and
  only on an unambiguous non-URL path (FR-1/AC-1, design §A, task 1.1).

## Medium (7) — applied

- **M1** FR-10/AC-1 untestable ("trim advisory surface") → concrete: `cargo audit`
  default set has zero non-tray advisories; documented; no removal (FR-10/AC-1, §J,1.17).
- **M2** FR-2/AC-6 (F40) mis-scoped → re-scoped to "overlay wake does not run
  `discover()`"; publish is already gated by `sessions_eq` (FR-2/AC-6, §B, task 1.5).
- **M3** FR-6/AC-3 (F33) relied on a non-existent guard → the aggregator's
  `duration_from_seconds` has no floor; add the 2.5s clamp (FR-6/AC-3, §F, task 1.2).
- **M4** Path-strip ordering / over-strip risk → explicit order + negative tests for
  URLs, MIME types, option tokens (FR-1/AC-1, §A, task 1.1).
- **M5** Installer quoting breaks the drift `contains` identity → split equality (quoted)
  vs contains (bare) branches + legacy-bare-path migration (FR-3/AC-1, ADR-3, task 1.9).
- **M6** Raising the default left the sink's hardcoded 2500ms fallback + keepalive
  uncounted → local `FALLBACK_MIN_INTERVAL` = 4s; keepalive gated + clamped
  (FR-2/AC-1, §B, tasks 1.3/1.4).
- **M7** FR-1/AC-6 (F27) unreconciled either/or → single canonical resolution:
  `redact_text` becomes the real sanitizer; logging points at it; task 1.16 `Depends: 1.1`
  (FR-1/AC-6, §A, tasks 1.1/1.16).

## Low (18) — applied

Testability/precision: NFR-2 thresholds; FR-2/AC-2 = 3s bound; FR-7/AC-1 cap = 16;
FR-9/AC-2 objective marker; FR-1/AC-6 single model. Consistency: 64 KiB window value
unified; Slack prefix list unified; F30 query-param names (`key`/`sig`/`signature`/
`access_token`); keepalive-clamp (F19) made explicit; task 1.16 `Depends: 1.1`; the F40
residual (overlay-wake `discover()`) addressed; blacklist case-fold component-boundary
correctness; cwd vs entry normalization split. All folded into the AC/design/task edits
above.

## Cross-doc couplings introduced (checked)

- Task 1.3 (sink) uses a **local** `FALLBACK_MIN_INTERVAL = 4s` rather than importing
  `config::DEFAULT_MIN_INTERVAL_SECS`, so 1.3 and 1.4 stay file-disjoint (no shared
  `config.rs` edit); both docs note the two values must agree (4.0s).
- Task 1.16 `Depends: 1.1` (different files — `logging.rs` vs `privacy.rs` — so it only
  orders them; no parallel-wave file conflict).
- Task 1.5 must NOT change `discover()`'s signature (task 1.7 owns `sessions.rs`).

## Re-verification

A focused second-pass audit (internal consistency, parallelism-graph soundness, risk
re-check of the H1/H2 carried-state design, dossier coverage) was run after applying the
fixes. **6 confirmed (0 high, 4 medium, 2 low), all applied:**

- **R1+R2 (medium, dup)** design §B still prescribed the sink→config coupling (`expose
  the constant` / `from_secs_f64(DEFAULT_MIN_INTERVAL_SECS)`) that the revision had
  replaced with a local `FALLBACK_MIN_INTERVAL` in tasks.md — design §B was stale and
  would have reintroduced a 1.3↔1.4 shared-file conflict. **Fixed** design §B to the
  local-constant form.
- **R3 (medium)** the H1/H2 carried-state set omitted the other latest-wins card fields
  — `usage`, `tokens_total`, `context_tokens`, `activity` would still blank on a long
  session (wrong tok/ctx%, dropped activity). **Fixed:** extended the carried set to all
  of them in FR-4/AC-1, design §D, task 1.6 + tests.
- **R4 (medium)** blacklist-entry normalization was assigned to task 1.1 (`privacy.rs`),
  which cannot touch `config.rs`. **Fixed:** moved entry `~`-expansion to task 1.4
  (config owner); task 1.1's matcher receives already-expanded entries.
- **R5 (low)** the aggregator's 2.5s clamp was wrongly justified as "matching sink.rs"
  (now 4.0s). **Fixed:** stated the 2.5s coalesce floor is the aggregator's own
  anti-busy-spin floor, independent of the sink's 4.0s rate floor.
- **R6 (low, cosmetic)** AC-1's one-line `Order:` summary omitted the `key=value` stage.
  **Fixed.**

A manual cross-doc consistency sweep then confirmed the key values agree everywhere
(sink 4.0s `FALLBACK_MIN_INTERVAL` vs aggregator 2.5s coalesce floor; 64 KiB window;
16-permit ingest cap; 3s shutdown bound; carried-state metric set; blacklist owner
split), the stale `from_secs_f64`/"expose the constant" references are gone, and the
1.3/1.4/1.16 parallel-wave file-disjointness holds. **Phase 4 closed: spec verified.**

<!-- Phase-6 final results appended below this line. -->

## FR-10 / cargo-audit (Phase 5)

Recorded for task 1.17 (FR-10/AC-1; design §J; findings F14/F35). Tooling:
`cargo-audit 0.22.1`, advisory DB at run time = 1139 advisories, scanning the
workspace `Cargo.lock` (296 crate deps).

### `cargo audit` — workspace lockfile

```
Scanning Cargo.lock for vulnerabilities (296 crate dependencies)
0 vulnerabilities found
11 warnings emitted (unmaintained / unsound)
```

The 11 warnings (all `unmaintained` except one `unsound`):

| ID | Crate | Kind |
| --- | --- | --- |
| RUSTSEC-2024-0370 | proc-macro-error 1.0.4 | unmaintained |
| RUSTSEC-2024-0411 | gdkwayland-sys 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0412 | gdk 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0413 | atk 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0414 | gdkx11-sys 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0415 | gtk 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0416 | atk-sys 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0418 | gdk-sys 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0419 | gtk3-macros 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0420 | gtk-sys 0.18.2 | unmaintained (gtk-rs GTK3) |
| RUSTSEC-2024-0429 | glib 0.18.5 | unsound (`VariantStrIter`) |

`cargo audit` reads the lockfile, which includes optional deps, so all 11
appear in the raw scan.

### Default-feature reachability — `cargo tree`

`cargo audit` has no feature filter, so the default-build proof is via
`cargo tree`. In the **default** feature set (no `--features tray`), NONE of the
flagged crates are present:

```
$ cargo tree -e no-dev | grep -E 'tao|tray-icon|gtk|glib|atk|gdk|pango|cairo|muda|libappindicator|proc-macro-error'
(no output — confirmed tray-only)
```

With `--features tray`, `tao` and `tray-icon` enter the tree and pull the entire
GTK3 / `glib` / `proc-macro-error` subtree. Every advisory's `cargo audit`
dependency tree roots in `tao` and/or `tray-icon` → `claude-presence`.

**Conclusion:** zero advisories are reachable in the DEFAULT build. The 11
warnings are reachable ONLY via the optional, off-by-default `tray` feature.

### `uuid 0.8.2` provenance

```
$ cargo tree -e no-dev -i uuid
uuid v0.8.2
└── discord-rich-presence v1.1.0
    └── claude-presence v0.1.2
```

Legacy `uuid 0.8.2` is pulled transitively by `discord-rich-presence` (a
non-optional default dep), present in the default build. It carries no active
advisory in the current DB (not in the 11 warnings above) and cannot be bumped
without an upstream `discord-rich-presence` release — upstream / out of our
control, documented as such in the `Cargo.toml` advisory block.

**Disposition (FR-10/AC-1):** default dependency set unchanged; no feature
flags trimmed (per F14/F35 no removal is required). Advisories documented in the
`Cargo.toml` comment block as tray-only / upstream, off-by-default, with this
tracked record.

---

## Phase 5 deep code-review + Phase 6 sign-off

**Deep code-review (task 2.1)** ran as a multi-agent workflow over the +3133/-394 diff
(7 dimensions: C-7 privacy leak, runtime panics, concurrency/shutdown, the carried-state
transcript change, installer reversibility, AC conformance, regressions), each finding
adversarially verified. **8 confirmed, all fixed:**

- **HIGH (transcript, NFR-1 regression in this pass):** the bounded carried-state re-derive
  used a *stateless* 64 KiB tail, so a `tool_result`/terminal line that scrolled past the
  window on a >64 KiB append between ticks was never seen → `busy`/`working` stuck forever.
  **Fixed:** a resync guard (track processed length; full-file re-derive when the per-tick
  append delta exceeds the 64 KiB cap or on truncation) + clear pending tool_uses on a
  terminal `stop_reason`. Regression test covers a resolving line scrolled past the window.
- **HIGH (hooks, upgrade round-trip):** the new shell-quoting made `uninstall` blind to the
  *bare* hook entries every shipped v0.1.x wrote → 6 dangling entries after upgrade.
  **Fixed:** legacy-bare migration (accept quoted OR bare identity in uninstall/doctor; install
  normalizes bare→quoted). Validated live: `doctor` reports the installed v0.1.2's bare
  entries as 6/6.
- **HIGH (statusline, upgrade round-trip):** the install self-reference guard only knew the
  quoted form, so upgrading from a legacy bare install stored a self-reference that uninstall
  would restore to a just-deleted wrapper. **Fixed:** guard recognizes the legacy bare form.
- **MEDIUM (privacy C-7):** `scrub_url` missed token-as-userinfo and `#fragment` secrets.
  **Fixed:** strip userinfo whenever present; peel the fragment; redact every query/fragment
  value by secret-detection, not only sensitively-named keys.
- **LOW x3:** `is_unambiguous_path` mangled `s/foo/bar/g`-style tokens (now extension-gated);
  long absolute paths were `[redacted]` instead of basename (blob heuristic now excludes
  path-shaped tokens); ingest/sink `connect_with_backoff` could leave a placeholder client
  installed on a spurious shutdown-watch wakeup (now always recovers the real client); hooks
  `write_atomic` used `with_extension` (now appends `.tmp`, matching statusline).

**Final gate (task 2.2) — PASS:**
- `cargo fmt --check` — clean
- `cargo clippy --all-targets -- -D warnings` — clean (default) and `--features tray` — clean
- `cargo test` — 280 lib + 8 e2e + 6 + 14 privacy + doc, 0 failed (default); 283 lib with `--features tray`
- `cargo run -- doctor` — 8 checks, 0 FAIL (1 expected WARN: the user's installed daemon holds the single-instance lock)

Baseline was 246 lib tests; +34 regression tests now lock in every fixed finding. All 41 audit
findings (F1–F41) closed; 0 critical/high remained in the original audit, and the 3 high
*regressions this hardening pass introduced* were caught by the mandatory review and fixed
before sign-off. **Hardening complete.**
