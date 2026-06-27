#!/bin/sh
# claude-presence statusline wrapper (chain-installed by `claude-presence install`).
#
# Claude Code calls this script as its `statusLine.command`, passing the statusLine
# JSON on STDIN and rendering whatever the script writes to STDOUT. The wrapper must
# stay byte-for-byte transparent to Claude Code:
#
#   1. it reads STDIN once,
#   2. feeds that JSON to the user's ORIGINAL statusline command and streams that
#      command's STDOUT straight through unchanged, and
#   3. ALSO tees the same JSON to the claude-presence daemon via
#      `claude-presence forward --kind statusline`.
#
# Claude Code injects no custom env var, so the wrapper resolves everything it needs
# from a fixed state file under $HOME (which Claude Code always sets). The installer
# (`src/install/statusline.rs`) writes that state file at install time with two
# shell assignments:
#
#   CP_FORWARD_BIN     absolute path to the claude-presence binary
#   CP_INNER_COMMAND   the user's stored original statusLine.command (may be empty)
#
# The forward step must NEVER block or fail Claude Code (C-6, FR-3/AC-1): it is
# backgrounded and all of its output and errors are discarded, so a missing daemon,
# a missing binary, or a slow socket can never delay or break the visible
# statusline. If there is no original command, the inner branch is a no-op `:`.

# Fixed, env-var-free location of the state written by the installer.
CP_STATE_DIR="${HOME}/.local/state/claude-presence"
CP_STATE_FILE="${CP_STATE_DIR}/statusline-state.sh"

# Defaults so the wrapper degrades safely if the state file is missing.
CP_FORWARD_BIN=""
CP_INNER_COMMAND=""

# Load the stored original command and the forwarder path (best-effort).
if [ -r "$CP_STATE_FILE" ]; then
	# shellcheck source=/dev/null
	. "$CP_STATE_FILE"
fi

# Read the statusLine JSON from stdin exactly once.
input=$(cat)

# Best-effort, non-blocking tee to the daemon. Backgrounded with stdout/stderr
# discarded so it can never delay or fail the statusline; guarded so a missing
# binary or path can never make the pipeline return non-zero.
if [ -n "$CP_FORWARD_BIN" ]; then
	printf '%s' "$input" | "$CP_FORWARD_BIN" forward --kind statusline >/dev/null 2>&1 &
fi

# Run the user's original command (if any), passing the same JSON on stdin, and
# stream its stdout straight through unchanged (the visible statusline must be
# identical). With no original command, `:` is a no-op and produces empty output.
if [ -n "$CP_INNER_COMMAND" ]; then
	printf '%s' "$input" | sh -c "$CP_INNER_COMMAND"
else
	:
fi
