# Harness: Claude Code

Wire Anthropic's Claude Code CLI to route its model calls through BitRouter at `http://localhost:4356`.

> **Cloud users:** swap `http://localhost:4356` → `https://api.bitrouter.ai` (Anthropic SDK drops the `/v1`) and use a `brk_*` key instead of `"unused"`. No daemon to install. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed and running (`bitrouter status` shows green).
- The `anthropic` provider active in BitRouter (`bitrouter providers list` should show `active: yes`).
- Claude Code installed and authenticated normally at least once.

## Configuration

> **TODO:** fill in the exact env var or settings.json path that points Claude Code at a custom Anthropic-shaped base URL. Confirmed knobs to capture:
> - The env var name(s) Claude Code reads for `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` override.
> - Whether `~/.claude/settings.json` carries the override and the exact field path.
> - The auth header expectation when `bitrouter`'s `skip_auth: true` is on (any token? a `brvk_…` virtual key?).
> - Anything Claude Code does differently for stream vs. non-stream that BitRouter needs to know about.

```bash
# placeholder — replace with the verified one-liner
export ANTHROPIC_BASE_URL="http://localhost:4356"
export ANTHROPIC_API_KEY="unused"      # bitrouter handles upstream auth
```

## Model selection

> **TODO:** confirm whether Claude Code accepts `anthropic/claude-sonnet-4-5` (BitRouter's `provider/model` form) or only bare `claude-sonnet-4-5`. If only bare names, document the `models:` alias pattern to add to `bitrouter.yaml`:

```yaml
# bitrouter.yaml — alias bare Claude Code model names to BitRouter's provider/model
models:
  claude-sonnet-4-5:
    upstream_id: "anthropic/claude-sonnet-4-5"
  claude-haiku-4-5:
    upstream_id: "anthropic/claude-haiku-4-5"
```

### Override models with `bitrouter spawn`

When launching Claude Code through `bitrouter spawn`, you can point it at other
BitRouter models without editing `~/.claude`. The selection is expressed over
three generic capability tiers — `high` / `mid` / `low`, which map to Claude's
`opus` / `sonnet` / `haiku` slots (those names also work as aliases) — and
delivered as `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL`, so the whole alias
ladder (and the picker's Default) is remapped while Claude keeps its tiered
behaviour (heavy work → high, default → mid, background → low). Use the
`provider/model` ids from `bitrouter models`.

```bash
# Everything on one cheap model:
bitrouter spawn -a claude -m opencode-go/glm-5.1 -- -p "summarize"

# Per-tier: keep a strong main model, cheap background:
bitrouter spawn -a claude -m mid=anthropic/claude-sonnet-4-6 \
  -m low=opencode-go/glm-5.1-air -- -p "refactor"

# A named preset from bitrouter.yaml:
bitrouter spawn -a claude --preset cheap -- -p "tidy imports"
```

```yaml
# bitrouter.yaml — a default plan applied to every spawn, plus named presets.
spawn:
  model:                       # default tier→model plan (optional)
    low: opencode-go/glm-5.1-air
  presets:
    cheap:
      high: opencode-go/glm-5.1
      mid: opencode-go/glm-5.1
      low: opencode-go/glm-5.1-air
```

Sources merge per tier, lowest priority first: `spawn.model` →
`BITROUTER_SPAWN_PRESET` → `BITROUTER_SPAWN_MODEL` (a bare id → every tier) →
`--preset` → `--model`. With none set, Claude Code's own default models are used.
See `references/cli.md` for the full flag/env reference.

## Verify

```bash
# from the shell that exported the override:
claude --version
# run a one-shot query — the daemon log should show an /v1/messages hit
echo "say hi" | claude
tail -n 20 ~/.bitrouter/bitrouter.log
```

## Notes & gotchas

> **TODO:** capture anything specific to Claude Code that surprised you in testing — e.g., whether tool use round-trips work end-to-end through BitRouter, whether streaming buffering behaves, any auth header strictness.
