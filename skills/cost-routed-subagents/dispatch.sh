#!/usr/bin/env bash
#
# dispatch.sh — spawn one headless Claude Code worker on a cheap model via BitRouter.
#
# A "worker" is a `claude -p` (print/headless) child process whose Anthropic-shaped
# environment is redirected at a BitRouter endpoint, so a sub-task runs on an
# inexpensive `provider/model` while the controller session stays on its own billing.
#
# This script touches ONLY the HTTP API + environment variables. It makes no
# BitRouter CLI calls and stores no secret — the key is read from the environment
# and forwarded to the child; it is never printed (see --dry-run, which redacts it).
#
# Protocols / contracts this integrates with (cite the source per repo policy):
#   - Claude Code headless mode and ANTHROPIC_* environment variables:
#       https://code.claude.com/docs/en/headless
#       https://code.claude.com/docs/en/settings
#   - Anthropic Messages API (the wire shape BitRouter accepts at /v1/messages):
#       https://docs.anthropic.com/en/api/messages
#   - BitRouter exposes that shape; for endpoint/auth setup see the `bitrouter` skill.
#
# License: Apache-2.0. Dispatch/review methodology adapted from obra/superpowers
# (MIT) — see references/attribution.md.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<'EOF'
Usage: dispatch.sh --task <file|-> (--tier cheap|standard|flagship | --model <provider/model>) [options]

Required:
  --task <file|->          Task prompt: a file path, or "-" to read stdin.
  --tier <name>            cheap | standard | flagship (resolved from $BITROUTER_MODEL_<TIER>).
  --model <provider/model> Explicit model id; overrides --tier.

Options:
  --role <name|path>       Role system prompt: a name under role-prompts/ (implementer,
                           spec-reviewer, quality-reviewer) or a file path. Default: implementer.
  --dir <path>             Working directory for the worker (also passed via --add-dir).
                           Default: current directory. Use a git worktree for isolation.
  --allowed-tools <csv>    Tools the worker may use. Default: Read,Edit,Bash,Grep,Glob.
  --permission-mode <m>    Claude Code permission mode. Default: acceptEdits.
  --output-format <f>      stream-json | json | text. Default: stream-json.
  --timeout <secs>         Wall-clock timeout for the worker. Default: none.
  --dry-run                Print the resolved invocation (KEY REDACTED) and exit 0.
  -h, --help               This help.

Environment (set these out-of-band; the controller agent references names, not values):
  BITROUTER_BASE_URL       BitRouter endpoint, Anthropic shape, NO trailing /v1
                           (e.g. http://127.0.0.1:4356).            [required]
  BITROUTER_API_KEY        brk_* key (or "unused" for a skip_auth local daemon). [required]
  BITROUTER_MODEL_CHEAP    provider/model for --tier cheap.
  BITROUTER_MODEL_STANDARD provider/model for --tier standard.
  BITROUTER_MODEL_FLAGSHIP provider/model for --tier flagship.
  BITROUTER_CHILD_CONFIG_DIR  Lean CLAUDE_CONFIG_DIR for workers.
                           Default: $HOME/.config/cost-routed-child.
EOF
}

die() { echo "dispatch.sh: $*" >&2; exit 2; }
# Guard a two-argument flag: $1 is the flag, $# is the remaining arg count.
# Without this, `shift 2` on a trailing flag with no operand aborts under `set -e`
# with no diagnostic.
require2() { [[ $# -ge 2 ]] || die "$1 needs a value."; }

# --- parse args ---------------------------------------------------------------
TASK="" TIER="" MODEL="" ROLE="implementer" DIR="" ALLOWED_TOOLS="Read,Edit,Bash,Grep,Glob"
PERM_MODE="acceptEdits" OUTPUT_FORMAT="stream-json" TIMEOUT="" DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --task) require2 "$@"; TASK="$2"; shift 2 ;;
    --tier) require2 "$@"; TIER="$2"; shift 2 ;;
    --model) require2 "$@"; MODEL="$2"; shift 2 ;;
    --role) require2 "$@"; ROLE="$2"; shift 2 ;;
    --dir) require2 "$@"; DIR="$2"; shift 2 ;;
    --allowed-tools) require2 "$@"; ALLOWED_TOOLS="$2"; shift 2 ;;
    --permission-mode) require2 "$@"; PERM_MODE="$2"; shift 2 ;;
    --output-format) require2 "$@"; OUTPUT_FORMAT="$2"; shift 2 ;;
    --timeout) require2 "$@"; TIMEOUT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

# --- preflight: endpoint + key present (NEVER print the value) ----------------
[[ -n "${BITROUTER_BASE_URL:-}" ]] || die "BITROUTER_BASE_URL is not set (Anthropic shape, no /v1)."
[[ -n "${BITROUTER_API_KEY:-}"  ]] || die "BITROUTER_API_KEY is not set."
case "$BITROUTER_BASE_URL" in
  */v1|*/v1/) echo "dispatch.sh: warning: BITROUTER_BASE_URL ends in /v1 — the Anthropic shape omits it; Claude Code appends /v1/messages itself." >&2 ;;
esac

# --- resolve model from --model or --tier -------------------------------------
if [[ -z "$MODEL" ]]; then
  [[ -n "$TIER" ]] || die "provide --model or --tier."
  # Branch-local var names keep this portable to bash 3.2 (no ${var^^}).
  case "$TIER" in
    cheap)    MODEL="${BITROUTER_MODEL_CHEAP:-}";    TIER_VAR="BITROUTER_MODEL_CHEAP" ;;
    standard) MODEL="${BITROUTER_MODEL_STANDARD:-}"; TIER_VAR="BITROUTER_MODEL_STANDARD" ;;
    flagship) MODEL="${BITROUTER_MODEL_FLAGSHIP:-}"; TIER_VAR="BITROUTER_MODEL_FLAGSHIP" ;;
    *) die "unknown --tier '$TIER' (expected cheap|standard|flagship)." ;;
  esac
  [[ -n "$MODEL" ]] || die "tier '$TIER' has no model: set $TIER_VAR (discover ids via GET \$BITROUTER_BASE_URL/v1/models) or pass --model."
fi

# --- resolve role system prompt ----------------------------------------------
if [[ -f "$ROLE" ]]; then
  ROLE_FILE="$ROLE"
else
  ROLE_FILE="$SCRIPT_DIR/role-prompts/$ROLE.md"
fi
[[ -f "$ROLE_FILE" ]] || die "role prompt not found: $ROLE (looked for '$ROLE_FILE')."
[[ -s "$ROLE_FILE" ]] || die "role prompt is empty: $ROLE_FILE"

# --- resolve task prompt ------------------------------------------------------
[[ -n "$TASK" ]] || die "--task is required (a file path, or - for stdin)."
if [[ "$TASK" == "-" ]]; then
  TASK_TEXT="$(cat)"
else
  [[ -f "$TASK" ]] || die "task file not found: $TASK"
  TASK_TEXT="$(cat "$TASK")"
fi
[[ -n "$TASK_TEXT" ]] || die "task prompt is empty."

DIR="${DIR:-$PWD}"
[[ -d "$DIR" ]] || die "working dir not found: $DIR"

# Clean global config home for the worker (no controller settings/credentials).
CHILD_CONFIG_DIR="${BITROUTER_CHILD_CONFIG_DIR:-$HOME/.config/cost-routed-child}"

# --- assemble the claude argv (shared by dry-run and real run) ---------------
# `--bare` is the isolation lever: it skips hooks, LSP, plugin sync, auto-memory,
# keychain reads, and CLAUDE.md auto-discovery — so a cheap worker is not handed a
# large session preamble, is not pushed to invoke skills, and cannot fall back to
# the controller's keychain credentials. The controller supplies everything the
# worker needs through the role prompt and task text.
CLAUDE_ARGS=( -p "$TASK_TEXT"
  --bare
  --output-format "$OUTPUT_FORMAT"
  --permission-mode "$PERM_MODE"
  --allowed-tools "$ALLOWED_TOOLS"
  --add-dir "$DIR"
  --append-system-prompt "$(cat "$ROLE_FILE")" )

run_worker() {
  mkdir -p "$CHILD_CONFIG_DIR"
  # Run in a subshell so these overrides never leak into the controller's env.
  # ANTHROPIC_API_KEY is unset so the worker cannot fall back to the controller's
  # direct Anthropic credential — it must use the BitRouter token below.
  (
    cd "$DIR"
    unset ANTHROPIC_API_KEY
    export ANTHROPIC_BASE_URL="$BITROUTER_BASE_URL"
    export ANTHROPIC_AUTH_TOKEN="$BITROUTER_API_KEY"
    export ANTHROPIC_MODEL="$MODEL"
    export CLAUDE_CONFIG_DIR="$CHILD_CONFIG_DIR"
    if [[ -n "$TIMEOUT" ]]; then
      # `timeout` ships with GNU coreutils; macOS users may have `gtimeout` instead.
      if command -v timeout >/dev/null 2>&1; then
        exec timeout "$TIMEOUT" claude "${CLAUDE_ARGS[@]}"
      elif command -v gtimeout >/dev/null 2>&1; then
        exec gtimeout "$TIMEOUT" claude "${CLAUDE_ARGS[@]}"
      else
        echo "dispatch.sh: warning: --timeout set but no timeout/gtimeout found; running without it." >&2
        exec claude "${CLAUDE_ARGS[@]}"
      fi
    else
      exec claude "${CLAUDE_ARGS[@]}"
    fi
  )
}

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "# dispatch.sh --dry-run (no worker spawned; key redacted)"
  echo "cd $DIR"
  echo "ANTHROPIC_BASE_URL=$BITROUTER_BASE_URL \\"
  echo "ANTHROPIC_AUTH_TOKEN=***redacted*** \\"
  echo "ANTHROPIC_MODEL=$MODEL \\"
  echo "CLAUDE_CONFIG_DIR=$CHILD_CONFIG_DIR \\"
  printf 'claude -p <%d-byte task> --bare --output-format %s --permission-mode %s \\\n' \
    "${#TASK_TEXT}" "$OUTPUT_FORMAT" "$PERM_MODE"
  printf '  --allowed-tools %s --add-dir %s --append-system-prompt <%s>\n' \
    "$ALLOWED_TOOLS" "$DIR" "$ROLE_FILE"
  exit 0
fi

run_worker
