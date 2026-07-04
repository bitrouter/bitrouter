# Harness: OpenClaw

Wire OpenClaw to route its model calls through BitRouter at `http://localhost:4356`. OpenClaw has a native BitRouter plugin — prefer that over generic base-URL overrides.

> **Cloud users:** the native plugin should support cloud endpoints out of the box once configured to point at `https://api.bitrouter.ai` with a `brk_*` key — verify against the plugin's docs. For the generic base-URL fallback, swap `http://localhost:4356` → `https://api.bitrouter.ai` and supply the `brk_*` key. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed and running (`bitrouter status` shows green).
- An active provider that serves the models OpenClaw drives (Claude family → `anthropic` or `github-copilot`; OpenAI family → `openai`).
- OpenClaw installed (`github.com/openclaw/openclaw`).

## Native plugin path (preferred)

The BitRouter <> OpenClaw native plugin lives at `github.com/bitrouter/bitrouter-openclaw`.

> **TODO:** fill in the exact install + configure steps for the native plugin. Specifics to capture:
> - The plugin install one-liner (npm? a binary drop? a config block in OpenClaw's settings?).
> - The OpenClaw config field that activates the BitRouter route (e.g. provider id, base URL, account selector).
> - Whether the plugin uses the control socket (`bitrouter.sock`) for richer info or just HTTP.
> - Any feature OpenClaw gets from the native plugin that the generic base-URL path does NOT (account failover, routing prefs, tool fan-out, etc.).

## Generic base-URL fallback

If you can't use the native plugin (or want to verify connectivity first), override OpenClaw's LLM endpoint:

> **TODO:** identify the env var / config field. OpenClaw is OpenAI-compatible at minimum, so:

```bash
# placeholder — replace with the verified one-liner
export OPENCLAW_BASE_URL="http://localhost:4356/v1"
export OPENCLAW_API_KEY="unused"
```

## Model selection

OpenClaw drives multiple model families. Add `models:` aliases for whatever it expects:

```yaml
models:
  # openclaw-driver:
  #   upstream_id: "anthropic/claude-sonnet-4-5"
  # openclaw-sub:
  #   upstream_id: "github-copilot/gpt-5.5-codex"
```

> **TODO:** confirm OpenClaw's actual model-id convention and replace the examples above.

## Verify

> **TODO:** capture the smoke test — e.g., a one-prompt OpenClaw invocation that produces a single LLM call you can verify in `~/.bitrouter/bitrouter.log`.

## Notes & gotchas

> **TODO:** anything OpenClaw-specific learned in testing — especially around ACP if OpenClaw also speaks it.
