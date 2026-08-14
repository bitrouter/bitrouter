#!/usr/bin/env bash
# Record a harness's terminal output as a replayable VT fixture.
#
# The fidelity matrix's automated tier replays these through BitRouter's
# emulator (`apps/bitrouter/src/tui/term.rs`), so a rendering regression in the
# wrapper fails in CI instead of in someone's terminal.
#
#   scripts/record-vt-fixture.sh <name> <command> [args...]
#
#   scripts/record-vt-fixture.sh claude-help claude --help
#   scripts/record-vt-fixture.sh codex-session codex          # interactive: quit when done
#
# A pty is allocated with a fixed 100x30 window so replays are deterministic —
# a capture at your terminal's size renders differently at any other.
#
# These are raw byte streams, not transcripts. **Scrub before committing** if
# the session touched anything private: prompts, paths, tokens, file contents.
set -euo pipefail

if [ $# -lt 2 ]; then
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
fi

name="$1"
shift

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/apps/bitrouter/src/tui/fixtures/$name.vt"

# `script` differs between BSD and GNU in argument order and in how the command
# is passed; both are common on contributors' machines.
if script -q /dev/null true >/dev/null 2>&1; then
    runner=(script -q /dev/null sh -c)   # BSD / macOS
else
    runner=(script -q -c)                # GNU / Linux
fi

quoted=$(printf '%q ' "$@")
inner="stty rows 30 cols 100 2>/dev/null || true; $quoted"

if [ "${runner[0]}" = "script" ] && [ "${runner[1]}" = "-q" ] && [ "${runner[2]}" = "-c" ]; then
    script -q -c "$inner" "$out" >/dev/null || true
else
    "${runner[@]}" "$inner" >"$out" 2>&1 || true
fi

bytes=$(wc -c <"$out" | tr -d ' ')
printf 'recorded %s (%s bytes)\n' "$out" "$bytes"
printf 'review it before committing:  xxd %s | head\n' "$out"
