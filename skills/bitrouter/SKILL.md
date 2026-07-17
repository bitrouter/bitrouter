---
name: bitrouter
description: >
  Use this skill when the user wants to install, configure, run, or
  troubleshoot BitRouter — an LLM proxy that runs two ways: a local Rust
  daemon at http://localhost:4356 (BYOK) or BitRouter Cloud at
  https://api.bitrouter.ai/v1 (managed, brk_* keys, Stripe credits or
  x402 wallet). Unifies OpenAI, Anthropic, Google, OpenRouter, GitHub
  Copilot, and OpenCode Zen/Go behind one endpoint. Also covers signup
  on bitrouter.ai, minting brk_ API keys, migrating off LiteLLM /
  OpenRouter / any OpenAI- or Anthropic-compatible gateway, editing
  bitrouter.yaml, and wiring coding-agent harnesses (Claude Code, Codex,
  Hermes Agent, OpenClaw). Trigger on "set up a local LLM proxy",
  "managed AI gateway", "replace litellm", "point claude code at a
  proxy", "bitrouter cloud", "brk_ key", anything naming bitrouter.yaml,
  port 4356, or api.bitrouter.ai — even when the user does not name
  BitRouter directly.
version: 1.0.0
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags: [llm, proxy, routing, openai, anthropic, google, gemini, openrouter, copilot, opencode, ai-gateway, claude-code, codex]
---

# BitRouter

BitRouter routes OpenAI- or Anthropic-shaped requests to any LLM provider. It runs **two ways**: a local Rust daemon at `http://127.0.0.1:4356` (BYOK — your keys, your machine) or a managed cloud service at `https://api.bitrouter.ai/v1` (one bill, no per-provider keys).

## 1. Ask first: Local or Cloud?

Before touching anything, ask the user this:

> Do you want to run BitRouter **locally** (install the daemon, BYOK with your own provider API keys, you pay each upstream directly) or use **BitRouter Cloud** (managed proxy at `api.bitrouter.ai`, one bill via Stripe credits or x402 wallet payments, no per-provider keys needed)?

### If Local
Continue to §2 (Install). Skip the Cloud paths below.

### If Cloud — four entry points

1. **Web playground (zero install, fastest demo).** Send the user to <https://bitrouter.ai> → sign up → top up credits via Stripe → use the in-browser playground. No code changes needed.

2. **API key in their SDK (most common production path).** On <https://bitrouter.ai> → Dashboard → API Keys → mint a `brk_*` key. Then in their code:

   ```python
   from openai import OpenAI
   client = OpenAI(
       base_url="https://api.bitrouter.ai/v1",
       api_key="brk_...",
   )
   client.chat.completions.create(
       model="openai/gpt-4o",
       messages=[{"role": "user", "content": "hi"}],
   )
   ```

   No local daemon installed. Works with the Anthropic SDK too — drop `/v1` from the base URL.

3. **Permissionless wallet (Solana / EVM, no account).** Sign an SOL_EDDSA JWT with the user's wallet, hit `api.bitrouter.ai` directly, x402/MPP handles payment. Crypto-native flow; point the user at <https://bitrouter.ai> docs for details — don't try to script the JWT signing yourself.

4. **Headless CLI (`bitrouter cloud login`).** RFC 8628 device-flow OAuth, persists the credential to `$XDG_DATA_HOME/bitrouter/account-credentials.json` (auto-refreshed). When the credentials file is present, the local daemon auto-adds the `bitrouter` provider in zero-config mode, so every entitled model is routable as `bitrouter:<model-id>` against `localhost:4356` — no manual `brk_*` paste, no `bitrouter.yaml` changes. `bitrouter cloud --help` then drives the /v1/* management surface (keys / usage / billing / policy / budget / preset / byok). See `references/cloud-setup.md` path D.

See `references/cloud-setup.md` for deeper detail (dashboard URLs, credit model, key rotation, wallet flow, CLI sign-in).

## 2. Install (Local only)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS users may prefer `brew install bitrouter/tap/bitrouter`; environments that already manage global npm tools can use `npm install -g bitrouter`. Windows:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"
```

`https://bitrouter.ai/install.{sh,ps1}` is the canonical entry point — it proxies the latest GitHub release's cargo-dist installer and survives transient asset-publishing gaps by falling back to the most recent release that actually has the installer attached.

Verify: `bitrouter --version`. If `command not found`, see `references/diagnose.md`.

## 3. Run (Local only)

**Guided onboarding.** Bare `bitrouter` (no subcommand) is the front door: a network-free credential probe launches a scripted wizard (credentials → harness → launch/serve/exit) when nothing is configured, or prints a one-line status + `bitrouter launch` hint when it already is (never re-onboards, never auto-spawns). `bitrouter init` re-runs it; every prompt has a flag, so `bitrouter init --yes [flags]` runs headlessly, emits the JSON envelope, never blocks, and scaffolds the starter `bitrouter.yaml` (`--force` overwrites, `--reset` clears credentials first). `--yes` reports-and-skips anything needing interactive OAuth (bare `--cloud-login`, provider PKCE/device) in `providers_skipped_interactive`. The two commands below remain the fast path.

**Zero-config (BYOK).** Export any of the supported env vars and start the daemon. It auto-enables every provider whose key is present.

```bash
export OPENAI_API_KEY=sk-...           # openai
export ANTHROPIC_API_KEY=sk-ant-...    # anthropic
export GEMINI_API_KEY=...              # google  (NOT GOOGLE_API_KEY)
export OPENROUTER_API_KEY=sk-or-...    # openrouter
export OPENCODE_ZEN_API_KEY=...        # opencode-zen AND opencode-go (shared)

bitrouter start          # detached daemon, logs to ~/.bitrouter/bitrouter.log
bitrouter status         # green dot + pid / listen / model count
bitrouter update         # self-update the binary (prereleases by default); --check to dry-run
```

The daemon writes its runtime files (`bitrouter.sock`, `bitrouter.pid`, `bitrouter.log`, optional `bitrouter.db`) into `~/.bitrouter/`.

Beyond the local-login providers, the daemon merges the public provider registry on startup: any registry **BYOK** provider whose key is present (convention `${NAME}_API_KEY`) becomes routable for the models it serves — the curated catalog plus any BYOK / BYO-subscription extras the provider lists beyond it — and providers are ranked by a configurable priority ladder. See `references/providers.md` → *Provider registry*.

**With a config file.** When you want explicit control (multi-account, MCP servers, ACP agents, custom providers):

```bash
bitrouter init                    # writes ./bitrouter.yaml (skip_auth: true)
$EDITOR bitrouter.yaml
bitrouter config validate -c ./bitrouter.yaml   # CI-safe: exits non-zero if invalid
bitrouter start --config ./bitrouter.yaml
```

Config search order, lowest-priority last: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory.

`bitrouter config validate` runs the real parse path — deserialization, `derives` resolution, and the upstream-URL (SSRF) gate — and exits non-zero on an invalid config. It does *not* load a JSON Schema; structural checking is what the parser enforces. Unset `${VAR}` references are substituted with a placeholder and reported as warnings, so it is safe to run in CI without secrets present.

Separately, a JSON Schema for the config is committed at `dist/schema/bitrouter.config.schema.json` (regenerated with `cargo run -p dist-helper -- generate-schema`). Add a `# yaml-language-server: $schema=…` header to a YAML config to get IDE autocomplete + inline validation against it.

**Subscription / OAuth providers.** Different — local login, not env vars:

```bash
bitrouter providers login claude-code       # Claude Pro/Max via Claude Code session
bitrouter providers login openai-codex      # ChatGPT/Codex subscription
bitrouter providers login github-copilot    # browser device flow, token stored on disk
bitrouter providers login supergrok         # SuperGrok via the Grok CLI session (~/.grok)
bitrouter providers login google-ai         # Google AI (Antigravity) via the `agy` CLI keyring session
```

Seed a BYOK provider (any that accepts a pasted key — `openai`, `anthropic`, `google`, `openrouter`, `opencode-*`) non-interactively with `--api-key sk-…` or `--key-stdin` (`printf %s "$KEY" | bitrouter providers login anthropic --key-stdin`). Both skip the method menu, conflict with the OAuth-only `--import-existing` / `--no-browser`, and error if the provider has no API-key method.

## 4. Connect your SDK

Point any OpenAI- or Anthropic-shaped SDK at the daemon. The credential the daemon validates is set by `server.skip_auth` (true in the starter config — credential-less local requests admitted; flip to `false` and mint a virtual key with `bitrouter key sign --user <id>` for multi-tenant).

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:4356/v1", api_key="unused")
client.chat.completions.create(
    model="openai/gpt-4o",                # provider/model
    messages=[{"role": "user", "content": "hi"}],
)
```

Resolve a model name without making a request: `bitrouter route openai/gpt-4o`.

> For Cloud, swap `http://localhost:4356/v1` → `https://api.bitrouter.ai/v1` and `api_key="unused"` → `api_key="brk_..."`. Everything else stays identical.

## 5. Origin MCP server

BitRouter exposes its own tools (`complete`, `list_models`, `status`) over MCP. This is the **origin** server — it wraps BitRouter's routing core — and is **distinct** from the MCP gateway at `/mcp` (which proxies upstream MCP servers declared in `bitrouter.yaml`).

```bash
# stdio (default): talks to the local daemon at 127.0.0.1:4356
bitrouter mcp serve

# streamable HTTP: cloud backend, multi-tenant — clients supply their own Bearer header
# no --token on the server side; each MCP client sets "Authorization: Bearer brk_..." in its remote config
bitrouter mcp serve --transport http

# stdio: origin AgentSkills server (skills_search/skills_get over installed skills)
bitrouter mcp serve --backend skills

# print the Claude/Cursor mcpServers config block
bitrouter mcp install --client claude

# merge non-destructively into an existing config file
bitrouter mcp install --client claude --config ~/Library/Application\ Support/Claude/claude_desktop_config.json
```

Transport↔backend defaults: `stdio` → local daemon at `127.0.0.1:4356`; `http` → cloud at `api.bitrouter.ai` (multi-tenant: each client sends its own `Authorization: Bearer` header; no server-side `--token` needed). HTTP server binds at `127.0.0.1:4357` and mounts at `/mcp-control`. For stdio→cloud, pass `--token brk_...` or `BITROUTER_TOKEN` (single-tenant).

See `references/mcp-server.md` for all flags, tool JSON shapes, and deferred roadmap items.

## 6. References

Read these on demand — don't load them all upfront.

| File | When to read |
|---|---|
| `references/cloud-setup.md` | User chose Cloud — signup walkthrough, key mint, billing, wallet path |
| `references/cli.md` | Full subcommand reference + what each one does |
| `references/providers.md` | Add / configure providers, multi-account, MCP servers, custom OpenAI-compatible endpoints |
| `references/diagnose.md` | Install issues, daemon won't start, connection refused, provider errors, log locations |
| `references/migrate-from-litellm.md` | Migrating off LiteLLM |
| `references/migrate-from-openrouter.md` | Migrating off OpenRouter (or keeping it as a fallback) |
| `references/migrate-from-openai-compatible.md` | Migrating from raw OpenAI keys, Azure, Together, Groq, Ollama, LM Studio, or any other OpenAI-compatible endpoint |
| `references/migrate-from-anthropic-compatible.md` | Migrating from raw Anthropic keys or any Anthropic-Messages-shaped gateway |
| `references/harness-claude-code.md` | Wiring Claude Code at `localhost:4356` |
| `references/harness-codex.md` | Wiring Codex CLI |
| `references/agent-plugin.md` | The installable Claude Code / Codex agent plugin — hooks, cost feed, MCP enable steps, restart handoff |
| `references/harness-hermes-agent.md` | Wiring Hermes Agent |
| `references/harness-openclaw.md` | Wiring OpenClaw |
| `references/mcp-server.md` | Origin MCP server — all flags, tool shapes, transport/backend details, roadmap |
| `references/updating.md` | `bitrouter update`, channels, package-manager delegation, the status nudge |
| `references/orchestration.md` | Fleet orchestration — the `mcp serve --backend fleet` stdio bridge, spawn/prompt/status/diff/apply/merge/close tools, human-gated writes, task-phrasing rules |
| `references/sessions.md` | Per-session ACP substrate — `acp serve\|prompt\|sessions`, NDJSON format, worktree retention, session records, one-agent-per-session, turn queue, identity, v1 limits |

## 7. Gotchas

- **Always ask Local-or-Cloud first.** The default of "just install locally" is wrong for users who want managed billing — they should never install the daemon at all.
- **Cloud sign-in is `bitrouter cloud login`.** Per-provider OAuth is `bitrouter providers login <provider>` (today: `claude-code`, `github-copilot`, `openai-codex`, `supergrok`, and `google-ai`), not a top-level `login` command.
- **Cloud management is `bitrouter cloud …`.** After `bitrouter cloud login`, run `bitrouter cloud --help` for the subcommand index: `keys`, `usage`, `requests`, `billing`, `policy`, `budget`, `preset`, `byok`. Every leaf accepts `--json`.
- **Local port: `127.0.0.1:4356`.** Old docs (and the upstream README) sometimes say 8787 — those are stale.
- **Cloud endpoints:** `https://api.bitrouter.ai/v1` for the OpenAI shape; `https://api.bitrouter.ai` (no `/v1`) for the Anthropic SDK — same asymmetry as Local.
- **Google's env var is `GEMINI_API_KEY`**, matching Google's own SDKs. `GOOGLE_API_KEY` is not auto-detected; override in `bitrouter.yaml` if you must.
- **Reload propagates env changes:** `export OPENAI_API_KEY=new...; bitrouter reload` updates the running daemon — no restart needed.
- **`bitrouter providers add/remove/use/test/stats` do not exist.** Provider management is `bitrouter providers list`, `bitrouter providers login <provider>`, and `bitrouter providers logout <provider>`. Edit `bitrouter.yaml` and `bitrouter reload` for config changes.
- **Model ids vs provider pins:** canonical model ids use slashes (`openai/gpt-4o`). An explicit provider pin uses a colon (`openrouter:openai/gpt-4o`, `claude-code:claude-sonnet-4-6`) and is still supported by the routing table.
- **No `bitrouter doctor`.** Diagnostics are: `bitrouter status`, `bitrouter route <model>`, `bitrouter models`, `bitrouter providers list`, log file at `~/.bitrouter/bitrouter.log`.
