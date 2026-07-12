# Harness: Claude Code

Wire Anthropic's Claude Code CLI to route its model calls through BitRouter at `http://localhost:4356`.

> **Cloud users:** swap `http://localhost:4356` → `https://api.bitrouter.ai` (Anthropic SDK drops the `/v1`) and use a `brk_*` key instead of the placeholder. No daemon to install. See `references/cloud-setup.md`.

## Prerequisites

- BitRouter installed (`bitrouter --version`). The daemon does **not** need to be pre-started for the `spawn` path — it auto-starts.
- Claude Code installed and authenticated normally at least once.

## Preferred launch path: `bitrouter spawn`

```bash
bitrouter spawn -a claude
bitrouter spawn -a claude -- -p "summarize this repo"
```

Reversible, per-process, and config-file-free: `spawn` launches Claude Code as a child process with two environment overrides and never touches `~/.claude/settings.json`. When the local daemon is down, `spawn` auto-starts it and waits for readiness first. Everything after `--` is forwarded to `claude` verbatim. After the session exits, `spawn` prints a one-line spend summary for the wrapped run.

## What the wiring actually is

Two environment variables — these are what `spawn` injects, and what a durable setup exports:

```bash
export ANTHROPIC_BASE_URL="http://localhost:4356"
export ANTHROPIC_AUTH_TOKEN="bitrouter-local"   # placeholder; fine under skip_auth: true
```

Facts that matter (verified against `apps/bitrouter/src/spawn.rs`):

- **`ANTHROPIC_AUTH_TOKEN`, not `ANTHROPIC_API_KEY`.** Claude Code sends `ANTHROPIC_AUTH_TOKEN` as `Authorization: Bearer …` — the credential BitRouter validates. `ANTHROPIC_API_KEY` would be sent as `x-api-key` instead, and in a BYOK setup that variable typically holds your *upstream* Anthropic provider key, which is not a valid BitRouter inbound credential. `spawn` always sets both variables explicitly (never inherit-only) for exactly this reason.
- **Token precedence** (`spawn`): an `ANTHROPIC_AUTH_TOKEN` you already exported → `BITROUTER_API_KEY` → the `bitrouter-local` placeholder. The placeholder works with the `skip_auth: true` default from `bitrouter init`; flip `skip_auth: false` and mint a `brvk_*` key (`bitrouter key sign --user <id>`) for multi-tenant setups.
- **Durable setup:** put the two exports in your shell profile, or in the `env` block of `~/.claude/settings.json`. Show the user the diff before writing settings files — never edit them silently.

## Model selection

Claude Code sends bare Anthropic model ids (`claude-sonnet-4-6`, `claude-haiku-4-5`). BitRouter's routing table resolves bare ids through its fallback chain — confirm with:

```bash
bitrouter route claude-sonnet-4-6
```

If a bare id doesn't resolve in your config, alias it in `bitrouter.yaml`:

```yaml
models:
  claude-sonnet-4-6:
    upstream_id: "anthropic/claude-sonnet-4-6"
```

## Verify

```bash
bitrouter spawn -a claude -- --version     # binary + wiring sanity
echo "say hi" | bitrouter spawn -a claude  # one-shot through the router
tail -n 20 ~/.bitrouter/bitrouter.log      # daemon log should show /v1/messages traffic
```

## Agent plugin

The BitRouter agent plugin (repo root `.claude-plugin/`) layers onto this wiring for Claude Code users: the `/bitrouter` skill and the origin MCP server (with a cost footer on tool results) for in-session model arbitrage. Install via `/plugin marketplace add bitrouter/bitrouter` → `/plugin install bitrouter@bitrouter`. A session spend summary is printed by `bitrouter spawn` on exit (a spawn feature, independent of the plugin).

## Notes & gotchas

- A plugin or env change cannot reroute a session that is already running — Claude Code reads `ANTHROPIC_BASE_URL` at startup. Wire first, then (re)launch.
- Streaming, tool use, and subagents ride the same `/v1/messages` surface — no extra wiring beyond the two variables.
