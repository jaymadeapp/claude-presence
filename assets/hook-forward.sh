#!/bin/sh
# claude-presence hook forwarder (chain-installed by `claude-presence install`).
#
# Claude Code runs this script as one entry in a lifecycle event's `hooks[]` group
# (SessionStart / PreToolUse / PostToolUse / Stop / SubagentStart / SubagentStop),
# passing the hook JSON on STDIN. The script forwards that JSON to the
# claude-presence daemon so the presence card can react in real time (FR-4/AC-1).
#
# It is appended ALONGSIDE the user's existing hook entries (e.g. a Stop hook that
# plays `afplay … Submarine.aiff`) and never replaces them; chaining is handled by
# `src/install/hooks.rs`.
#
# This forwarder must NEVER block or fail the tool call (C-6, FR-4/AC-3): the
# forward is backgrounded with all output discarded and the script always exits 0,
# so a missing daemon, a missing binary, or a slow socket can never delay or break
# Claude Code. The bundled `forward` subcommand already swallows delivery errors;
# this script additionally guarantees a zero exit no matter what.
#
# Claude Code injects no custom env var, so the absolute path to the claude-presence
# binary is substituted in at install time by `src/install/hooks.rs`
# (placeholder {{FORWARD_BIN}}).

# Best-effort, non-blocking forward to the daemon. Read STDIN once and pipe it to
# the forwarder, backgrounded with `&` so the foreground spawn returns 0
# immediately (its exit status is the fork, not the pipeline's outcome) and
# stdout/stderr are discarded so it can never delay the tool call. The pipeline's
# own exit code is never inspected; the explicit `exit 0` below is what guarantees
# a zero exit regardless of whether the binary is missing or the socket is slow.
cat | {{FORWARD_BIN}} forward --kind hook >/dev/null 2>&1 &

# Always succeed: a failed background spawn must not fail the originating tool call.
exit 0
