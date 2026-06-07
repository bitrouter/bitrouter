# Harness: Hermes Agent

Wire Hermes Agent to route its model calls through BitRouter at `http://localhost:4356`.

> **Cloud users:** swap `http://localhost:4356/v1` → `https://api.bitrouter.ai/v1` (or drop `/v1` for the Anthropic shape) and use a `brk_*` key instead of `"unused"`. No daemon to install. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed and running (`bitrouter status` shows green).
- Whichever provider Hermes targets is `active` in `bitrouter providers list`.
- Hermes Agent installed.

## Configuration

> **TODO:** fill in everything below — this stub exists so the routing exists in the skill index, but it has not been validated.
>
> Specifics to capture:
> - Hermes' canonical project home / install command (link to repo).
> - The env var or config field that overrides the model endpoint base URL.
> - Whether Hermes speaks OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages on the wire — BitRouter supports all three but the harness has to send something it expects.
> - Auth header convention when `skip_auth: true` is on.
> - Any model-id normalization Hermes does before sending the request.

```bash
# placeholder — replace with the verified env vars / config edits
# export HERMES_LLM_BASE_URL="http://localhost:4356/v1"
# export HERMES_LLM_API_KEY="unused"
```

## Model selection

> **TODO:** add the `models:` aliases that match Hermes' expected model identifiers, e.g.

```yaml
models:
  # hermes-default:
  #   upstream_id: "anthropic/claude-sonnet-4-5"
```

## Verify

> **TODO:** one-line smoke test that triggers a single LLM call and lets the user confirm via `tail -n 20 ~/.bitrouter/bitrouter.log` that it landed.

## Notes & gotchas

> **TODO:** anything Hermes-specific you learn while wiring it.
