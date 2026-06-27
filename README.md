# claude-presence

A Rust daemon for macOS that aggregates the live activity of **all** your running
Claude Code sessions into a **single** Discord Rich Presence card — model, plan,
cost, token total, context %, current activity, project, branch, and how many
sessions are running — updated in real time. Discord shows one presence per
application per user, so every session is merged into one card (the count goes to
`party.size`), never one card per session.

## Prerequisites

- **macOS** (the only supported platform today).
- A **Rust toolchain** (stable) to build the binary: <https://rustup.rs>.
- The **Discord desktop app** running and logged in (the daemon connects over
  Discord's local IPC socket; there is no bot token, OAuth, or network egress).
- **Claude Code** installed (this is what the daemon observes).

## Build

```sh
cargo build --release
```

The binary lands at `target/release/claude-presence`. Put it somewhere on your
`PATH` (or invoke it by full path); the installer records the absolute path of the
binary that ran `install`, so launchd and the chained scripts always call the same
one.

## Discord app setup

A Discord application named **"CC"** is already registered (Discord blocked the
names "Claude" / "Claude Code"), with **client_id `1518007333324587168`**. This is
the default `client_id`, so the MVP works out of the box — no Discord Developer
Portal step is required to get a card.

If you prefer your own application:

1. Open the [Discord Developer Portal](https://discord.com/developers/applications)
   and create an application.
2. Copy its **Application ID** and set it as `client_id` in your config.
3. (Optional) Under **Rich Presence → Art Assets**, upload images and note their
   keys:
   - a `large_image` (the app picture), and
   - a `small_image` (the Claude asterisk, e.g. keyed `claude`).

   Then put those keys under `[assets]` in your config.

**Images are optional.** With no asset keys set, the card still renders a valid
presence — the images are simply omitted. The bold name on the card is the app
name ("CC"); the string **"Claude Code"** appears as the `large_text` tooltip when
you hover the large image.

## Install / Uninstall

```sh
claude-presence install
```

`install` performs three reversible steps and then starts the daemon:

1. **statusLine wrapper** — captures your current `~/.claude/settings.json`
   `statusLine.command`, stores it, and points `statusLine.command` at a bundled
   wrapper. The wrapper runs your **stored original** command and passes its output
   straight through (your visible status line is unchanged), and additionally tees
   the statusLine JSON to the daemon. This gives exact cost / context % / model.
2. **Lifecycle hooks** — **appends** a forwarder entry into the existing
   `hooks[]` group of each event (`SessionStart`, `PreToolUse`, `PostToolUse`,
   `Stop`, `SubagentStart`, `SubagentStop`) in `settings.json`, preserving your own
   entries (e.g. a `Stop` hook that plays `afplay … Submarine.aiff`). This gives
   the lowest-latency "Running X" the instant a tool starts.
3. **launchd user agent** — writes `~/Library/LaunchAgents/com.jakubsladek.claude-presence.plist`
   and `bootstrap`s it into your `gui/<uid>` domain (no root). It runs at login and
   restarts only on crash, with logs under `~/.local/state/claude-presence/logs/`.

Install **chains** your existing statusline and hooks — it never overwrites them —
and is idempotent, so re-running it is safe. If any step fails, the steps already
applied are rolled back.

```sh
claude-presence uninstall
```

`uninstall` reverses every change, in the exact reverse order:

1. `launchctl bootout gui/<uid>` runs **first** (and before the process exits) so
   launchd cannot relaunch the daemon after it clears the presence, then the plist
   is removed.
2. Hooks: removes **only our exact entry** from each event group by identity; your
   own hook entries are kept byte-for-byte.
3. statusLine: restores your original command exactly **if** `statusLine.command`
   still points at our wrapper. If you changed it since install (drift), your value
   is left untouched and a warning is printed (restore-or-warn). The wrapper and
   state files are then removed.

Every install action has a tested, exact uninstall (NFR-6).

## Commands

| Command | What it does |
|---|---|
| `claude-presence run` | Run the daemon in the **foreground** (the same code launchd runs; useful for debugging). |
| `claude-presence install` | Install the launchd agent + chained statusline wrapper + chained hooks (reversible), then start the daemon. |
| `claude-presence uninstall` | Fully revert everything `install` set up. |
| `claude-presence status` | Show detected live sessions (pid, project, branch), whether a Discord IPC socket is present, and whether a daemon is already running. |
| `claude-presence doctor` | Diagnose the install with PASS/WARN/FAIL lines + hints: Discord socket, statusLine wiring, hooks wiring (e.g. `6/6`), launchd plist, config validity (effective `client_id`/capacity), single-instance conflicts, detected sessions, and the buttons-on-own-profile caveat. |

(There is also an internal `forward` subcommand used by the chained scripts to pipe
events to the daemon socket; it is not part of the user-facing surface.)

## Configuration

See [`config.example.toml`](./config.example.toml) for every option, grouped and
commented with its default. The daemon loads:

```
~/.config/claude-presence/config.toml
```

Copy the example into place and edit:

```sh
mkdir -p ~/.config/claude-presence
cp config.example.toml ~/.config/claude-presence/config.toml
```

Every field is optional and ships with a safe default, so a missing or invalid
config never crashes the daemon. **Changes take effect on daemon restart** — there
is no hot reload in v1. Restart with:

```sh
launchctl kickstart -k gui/$(id -u)/com.jakubsladek.claude-presence
```

or just re-run `claude-presence install`.

State (the daemon socket, logs, the installed scripts) lives under
`~/.local/state/claude-presence` (a `0700` dir; sensitive files are `0600`).

## Privacy

Privacy is on by default (C-7). Transcripts contain your prompts, file paths, and
possibly secrets — none of that ever leaves the process or reaches the daemon's own
logs. Only structured, sanitized fields are emitted, to Discord **and** to logs:

- **Private mode** (`privacy.redact`, default **on**): only generic/sanitized
  labels are emitted — no targets, no AI title.
- **Bash arguments are dropped by default.** With `privacy.scrub_bash_args = true`
  a command may be shown, but only after secrets (tokens, keys, passwords,
  `Authorization`, `WORD=value` env-assignments, credentialed URLs, long base64/hex
  blobs) are stripped and the result truncated.
- **AI-generated session title is off by default** (`show_ai_title`); it is only
  ever shown when explicitly enabled **and** the project is not blacklisted.
- **Paths are reduced to a basename** — never a full path or your home directory.
- **Project blacklist** (`privacy.blacklist_paths`): listed projects are shown
  generically or hidden.
- **Buttons are off by default** and, when enabled, must be `https://` (never
  `file://` or a private/credentialed URL). The card and its buttons are public.

## The card

The Discord activity is built from the aggregated `PresenceModel` (design §4.3):

| Card field | What it shows |
|---|---|
| `details` (≤128) | `"{verb} {target} — {project} ({branch})"`, sanitized — the current activity. |
| `state` (≤128) | `"{model} ({effort}) · {plan} · {tokens} tok · Ctx {pct}%"`, shortened by a truncation ladder if needed. (`Ctx` = live context-window fill = latest-request context tokens ÷ the model's window, e.g. 1M for Opus 4.8. Cost is **off by default** — enable `[fields] cost = true`, ideally with the statusLine wrapper for an exact figure.) |
| `timestamps.start` | The focused session's start time as an elapsed timer (epoch **milliseconds**). |
| `party.size` | `[live_count, capacity]` → renders "(N of M)" — how many sessions are running. |
| `assets.large_image` / `large_text` | Your uploaded app picture (if set) + the "Claude Code" tooltip. |
| `assets.small_image` | A per-tool badge (if set). |
| `buttons` | Off by default; https-only when enabled. |

**Caveat:** Discord may **not render buttons on your OWN profile** over local IPC —
they are still visible to other people viewing your profile.

## Troubleshooting

Run:

```sh
claude-presence doctor
```

Each check prints `[PASS]/[WARN]/[FAIL]` with an actionable hint (e.g. "Discord not
running — start the Discord desktop app", "wrapper not installed — run
`claude-presence install`"). A common case: no card appears because Discord is not
running or no Claude Code session is active — both show as `WARN`, not errors.
