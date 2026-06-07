# Harness: Codex CLI

Wire OpenAI's Codex CLI to route its model calls through BitRouter at `http://localhost:4356`.

> **Cloud users:** swap `http://localhost:4356/v1` → `https://api.bitrouter.ai/v1` and use a `brk_*` key instead of `"unused"`. No daemon to install. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed and running (`bitrouter status` shows green).
- The `openai` provider active in BitRouter, or whichever provider hosts the gpt-5.x-codex family you want (`github-copilot` and `opencode-zen` both serve Codex-family models via the Responses API).
- Codex CLI installed.

## Configuration

> **TODO:** fill in the exact env var or config file path that points Codex at a custom OpenAI-shaped base URL. Confirmed knobs to capture:
> - The env var Codex reads for `OPENAI_BASE_URL` / `OPENAI_API_KEY` override (one of `OPENAI_BASE_URL`, `OPENAI_API_BASE`, or a Codex-specific name).
> - Whether `~/.codex/config.toml` or similar carries the override and the exact field path.
> - Whether Codex pins itself to the Responses API or also uses Chat Completions — BitRouter routes per-model, so the harness can stay protocol-agnostic.
> - The auth header expectation when BitRouter's `skip_auth: true` is on.

```bash
# placeholder — replace with the verified one-liner
export OPENAI_BASE_URL="http://localhost:4356/v1"
export OPENAI_API_KEY="unused"
```

## Model selection

> **TODO:** confirm Codex's model identifier convention. BitRouter accepts `openai/gpt-5.5-codex`, `github-copilot/gpt-5.5-codex`, and `opencode-zen/opencode/gpt-5.5-codex` — the harness probably wants a bare name (`gpt-5.5-codex`). Add an alias:

```yaml
models:
  gpt-5.5-codex:
    upstream_id: "openai/gpt-5.5-codex"   # or github-copilot / opencode-zen, your call
```

## Verify

```bash
# in the shell with the overrides exported
codex --version
echo "fix the bug in main.py" | codex
tail -n 20 ~/.bitrouter/bitrouter.log
```

## Notes & gotchas

> **TODO:** capture anything specific to Codex that surprised you — e.g., whether the diff/apply tool calls round-trip cleanly, whether reasoning tokens are preserved, how Codex handles 401s from the upstream when BitRouter falls through accounts.
