<div align="center">

# claude-presence

**One Discord Rich Presence card for *all* your Claude Code sessions — live, on macOS.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Latest release](https://img.shields.io/github/v/release/jaymadeapp/claude-presence?sort=semver)](https://github.com/jaymadeapp/claude-presence/releases)
[![Homebrew](https://img.shields.io/badge/Homebrew-jaymadeapp%2Ftap-orange)](https://github.com/jaymadeapp/homebrew-tap)
[![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey)](#requirements)
[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ec4899)](https://github.com/sponsors/jaymadeapp)

<br>

<img src="docs/card.png" alt="Discord Rich Presence card showing CC — Working on Projects — Opus 4.8 · 574K tok · Ctx 57%, with a live elapsed timer" width="440">

<br>
<br>

A tiny Rust daemon that folds every running Claude Code session into a **single**
honest Discord card — project, model, agent count, tokens, context %, live timer.
**No network egress. No bot token.** Local Discord IPC only.

<sub>The card's bold name is **CC** — Discord blocks "Claude" / "Claude Code"; "Claude Code" shows on hover.</sub>

</div>

---

## Quick start

```sh
brew install jaymadeapp/tap/claude-presence   # prebuilt binary, no Rust toolchain
claude-presence install                       # wires launchd + statusLine + hooks, then starts
```

Have the **Discord desktop app** running and **Claude Code** installed, then start a
session — a card appears on your profile. Verify anytime with `claude-presence doctor`.

> Works with the **Claude Code app** on macOS today. CLI support is in the works — coming soon.

## Why

- **All sessions, one card** — Discord shows one presence per app, so every session is folded into it, with a live count when several run.
- **Private by default** — only sanitized fields ever leave the process; hide the project or command with one flag, or go fully dark with private mode.
- **No egress** — local Discord IPC only; no OAuth, no outbound connection.
- **Clean in, clean out** — chains (never overwrites) your statusLine + hooks; every install action has an exact, tested uninstall.

<br>

<details>
<summary><b>Install &amp; uninstall</b></summary>

<br>

**Requirements** — macOS, the Discord desktop app running and logged in, and Claude Code installed.

**Toggle without uninstalling** — clears the card and stays cleared across reboots:

```sh
claude-presence disable   # alias: off
claude-presence enable    # alias: on
```

**Uninstall** — Homebrew only removes the binary; unwire first, then remove it:

```sh
claude-presence uninstall      # unwires launchd + statusLine + hooks, clears the card, keeps config.toml
brew uninstall claude-presence
```

</details>

<details>
<summary><b>Let Claude Code set it up (paste-prompts)</b></summary>

<br>

**Install**

```text
Set up claude-presence (a Discord Rich Presence for Claude Code) on my Mac:

1. First ask me these two questions and WAIT for my answers:
   - "Show your project name on the Discord card, or keep it hidden?"
   - "Show the command you're currently running, or keep it hidden?"
2. Run: brew install jaymadeapp/tap/claude-presence
3. Run `claude-presence install` with -y (so it doesn't prompt again) and the
   flags matching my answers:
   - project: shown -> --show-project, hidden -> --hide-project
   - command: shown -> --show-command, hidden -> --hide-command
   e.g. for "hide project, show command": claude-presence install -y --hide-project --show-command
4. Make sure the Discord desktop app is running, then run: claude-presence doctor
   and fix any [FAIL] lines you find.

Report the doctor output when done.
```

**Uninstall**

```text
Remove claude-presence (the Discord Rich Presence for Claude Code) from my Mac:

1. Run: claude-presence uninstall
   (this unwires launchd + statusLine + hooks and clears the Discord card; it
   keeps your config.toml as user data)
2. Then run: brew uninstall claude-presence
3. Run `claude-presence doctor` if it still resolves, or `which claude-presence`,
   to confirm the binary is gone.

Report what each step printed. If `claude-presence uninstall` prints any [warn]
lines (e.g. statusLine drift), show them to me verbatim — do not try to fix them
yourself.
```

**Change privacy** (idempotent — re-runs the installer and restarts the daemon for you)

```text
Change the privacy settings of my already-installed claude-presence. Don't edit
any files by hand — re-run the installer, which is idempotent and restarts the
daemon for me.

1. First ask me these THREE questions and WAIT for my answers:
   - "Show your PROJECT name on the Discord card, or hide it?"
   - "Show the COMMAND you're currently running, or hide it?"
   - "Turn on PRIVATE mode (hides everything: project, command, model, and
      metrics)? yes / no"
2. Build a single command with an explicit flag for EVERY axis — never omit one,
   because a flag-less axis defaults to HIDDEN on a non-interactive re-run:
   - project shown  -> --show-project   | project hidden -> --hide-project
   - command shown  -> --show-command   | command hidden -> --hide-command
   - private = yes  -> add --private     | private = no   -> add nothing for it
   Example (show project, hide command, private off):
     claude-presence install -y --show-project --hide-command
   Example (private mode on):
     claude-presence install -y --hide-project --hide-command --private
3. Run that command, then run `claude-presence status` and tell me it succeeded.

Note: --private is a one-way switch via these flags — there is no --no-private. To
turn private mode back OFF, tell me and I'll clear privacy.redact in
~/.config/claude-presence/config.toml and restart the daemon with:
  launchctl kickstart -k gui/$(id -u)/com.jakubsladek.claude-presence
```

</details>

<details>
<summary><b>Privacy</b></summary>

<br>

Baseline sanitization is always on. Transcripts contain your prompts, file paths, and
possibly secrets — none of that ever leaves the process or reaches the daemon's own
logs. Only structured, sanitized fields are emitted, to Discord **and** to logs.

- **Hide the project / the running command** (`[privacy.fields]`). Config defaults are *shown* (`project = true`, `command = true`), but `claude-presence install` asks whether to hide each, with the prompt defaulting to *hide* — so an interactive install is privacy-first. Set directly:
  - `privacy.fields.project = false` collapses the project (and its branch) to a generic label.
  - `privacy.fields.command = false` hides the running command in the small-icon tooltip (only the bare verb, e.g. "Running", shows).

  The Bash command target is **always** sanitized to a bare program name regardless — a leading `VAR=value`, a `$(…)` substitution, a path, or a secret can never appear on the card.
- **Private mode** (`privacy.redact`, default **off**) — emits only generic/sanitized labels: no project, branch, activity target, AI title, model, or metrics. `install --private` sets it.
- **Bash arguments are dropped by default.** With `privacy.scrub_bash_args = true` a fuller command may show, but only after secrets (tokens, keys, passwords, `Authorization`, `WORD=value`, credentialed URLs, long base64/hex blobs) are stripped and truncated. `privacy.fields.command = false` takes precedence.
- **AI-generated session title is off by default** (`show_ai_title`); even enabled it shows only for non-blacklisted projects and is secret-scrubbed.
- **Paths are reduced to a basename** — never a full path or your home directory.
- **Project blacklist** (`privacy.blacklist_paths`) — listed projects are shown generically.
- **The card ships one button** — `Get claude-presence` → `https://claude-presence.com` — shown to *other* viewers, not on your own card. Buttons must be `https://`. Remove with `buttons = []` and restart the daemon.

</details>

<details>
<summary><b>How it works</b></summary>

<br>

`install` performs three reversible, idempotent steps and then starts the daemon:

1. **statusLine wrapper** — stores your current `statusLine.command`, points the setting at a bundled wrapper that runs your **stored original** and passes its output straight through (your visible status line is unchanged), additionally teeing the statusLine JSON to the daemon. Supplies exact cost / context % / model.
2. **Lifecycle hooks** — **appends** a forwarder entry into each event's existing `hooks[]` group (`SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStart`, `SubagentStop`), preserving your own entries (e.g. a `Stop` hook playing `afplay … Submarine.aiff`). Gives the lowest-latency "Running X" the instant a tool starts.
3. **launchd user agent** — writes `~/Library/LaunchAgents/com.jakubsladek.claude-presence.plist` and `bootstrap`s it into your `gui/<uid>` domain (no root). Runs at login, restarts only on crash; logs under `~/.local/state/claude-presence/logs/`.

Install **chains** your existing statusline and hooks — never overwrites — and re-running is safe; on failure, applied steps roll back in reverse. `uninstall` reverses every change in exact reverse order: `launchctl bootout` runs first so launchd can't relaunch after the card clears; hooks remove **only our exact entry** by identity; statusLine is restored only if it still points at our wrapper (drift is left untouched and warned). Your `config.toml` is treated as user data and left in place.

Config lives at `~/.config/claude-presence/config.toml` (see [`config.example.toml`](./config.example.toml)); every field is optional with a safe default. **Changes take effect on daemon restart** — no hot reload:

```sh
launchctl kickstart -k gui/$(id -u)/com.jakubsladek.claude-presence
```

</details>

<details>
<summary><b>Commands</b></summary>

<br>

| Command | What it does |
|---|---|
| `claude-presence install` | Install the launchd agent + chained statusLine wrapper + chained hooks (reversible), then start the daemon. Prompts whether to hide your project / running command; scriptable with `--hide-project`/`--show-project`, `--hide-command`/`--show-command`, `--private`, `-y`/`--non-interactive`. |
| `claude-presence uninstall` | Fully revert everything `install` set up (leaves your `config.toml`). |
| `claude-presence enable` (`on`) | Re-load the launchd agent after a `disable`. |
| `claude-presence disable` (`off`) | Unload the launchd agent and clear the Discord card without uninstalling (survives reboot). |
| `claude-presence run` | Run the daemon in the **foreground** (the same code launchd runs; useful for debugging). |
| `claude-presence status` | Show detected live sessions (pid, project, branch), whether a Discord IPC socket is present, and whether a daemon is already running. |
| `claude-presence doctor` | Diagnose the install with `[PASS]`/`[WARN]`/`[FAIL]` lines + hints: Discord socket, statusLine wiring, hooks wiring (e.g. `6/6`), launchd plist, config validity, single-instance conflicts, detected sessions, and the buttons-on-own-profile caveat. |

**Build from source** (needs a stable Rust toolchain):

```sh
cargo build --release   # binary at target/release/claude-presence
```

</details>

<details>
<summary><b>Troubleshooting</b></summary>

<br>

```sh
claude-presence doctor
```

Each check prints `[PASS]`/`[WARN]`/`[FAIL]` with an actionable hint (e.g. "Discord
not running — start the Discord desktop app", "wrapper not installed — run
`claude-presence install`"). Common case: no card appears because Discord isn't
running or no Claude Code session is active — both show as `WARN`, not errors.

</details>

<br>

---

<div align="center">

**Free and MIT-licensed, and it'll stay that way.** If it earns a spot on your Discord
profile, you can [sponsor **@jaymadeapp**](https://github.com/sponsors/jaymadeapp) —
optional, and stars + bug reports help just as much.

Built by [jaymade](https://jaymade.app) · MIT — see [LICENSE](./LICENSE)

</div>
