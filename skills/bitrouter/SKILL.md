---
name: bitrouter
description: >
  Use this skill when the user wants to install, configure, run, or
  troubleshoot BitRouter — an LLM proxy that runs two ways: a local Rust
  daemon at http://localhost:4356 (BYOK) or BitRouter Cloud at
  https://api.bitrouter.ai/v1 (managed, brk_* keys, Stripe credits or
  x402 wallet). Unifies OpenAI, Anthropic, Google, OpenRouter, GitHub
  Copilot, and OpenCode Zen/Go behind one endpoint. Also covers signup
  on bitrouter.ai, minting brk_ API keys, running auditable benchmarks,
  migrating off LiteLLM /
  OpenRouter / any OpenAI- or Anthropic-compatible gateway, editing
  bitrouter.yaml, and wiring coding-agent harnesses (Claude Code, Codex,
  Hermes Agent, Harbor Terminus-2, OpenClaw), plus optimizing agent workflows
  against eval quality and normalized routed cost. Trigger on "set up a local LLM proxy",
  "managed AI gateway", "replace litellm", "point claude code at a
  proxy", "bitrouter cloud", "brk_ key", anything naming bitrouter.yaml,
  port 4356, Harbor Terminus-2, or api.bitrouter.ai — even when the user does not name
  BitRouter directly.
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags:
    - llm
    - proxy
    - routing
    - openai
    - anthropic
    - google
    - gemini
    - openrouter
    - copilot
    - opencode
    - ai-gateway
    - claude-code
    - codex
---

# BitRouter

BitRouter routes OpenAI- or Anthropic-shaped requests to any LLM provider. It runs **two ways**: a local Rust daemon at `http://127.0.0.1:4356` (BYOK — your keys, your machine) or a managed cloud service at `https://api.bitrouter.ai/v1` (one bill, no per-provider keys).

## 1. Ask first: Local or Cloud?

Before touching anything, ask the user this:

> Do you want to run BitRouter **locally** (install the daemon, BYOK with your own provider API keys, you pay each upstream directly) or use **BitRouter Cloud** (managed proxy at `api.bitrouter.ai`, one bill via Stripe credits or x402 wallet payments, no per-provider keys needed)?

### If Local
Continue to §2 (Install). Skip the Cloud paths below.

### If Cloud — four entry points

Each is written up in full (dashboard URLs, credit model, key rotation) in `references/cloud-setup.md`, keyed A-D.

1. **Web playground** (A) — zero install. <https://bitrouter.ai> → sign up → top up credits → use the in-browser playground. 2. **`brk_*` API key in their SDK** (B) — the production path. Mint at Dashboard → API Keys, then point any OpenAI- or Anthropic-shaped SDK at `https://api.bitrouter.ai/v1` (drop `/v1` for the Anthropic SDK). No daemon. 3. **Permissionless wallet** (C) — Solana/EVM, no account; x402/MPP handles payment. Point the user at the docs; do not script the JWT signing yourself. 4. **Headless CLI** (D) — `bitrouter cloud login`, RFC 8628 device flow. The credential persists and auto-refreshes, and the local daemon then auto-adds a `bitrouter` provider in zero-config mode, so entitled models are routable as `bitrouter:<model-id>` against `localhost:4356` — no `brk_*` paste, no config edit. An explicit `BITROUTER_API_KEY` overrides that stored session for inference; unset it to use the login credential. `bitrouter cloud --help` drives typed management workflows (namespace / keys / usage / requests / billing / policy / budget / preset / byok); use `bitrouter cloud api <relative-endpoint>` for additional Cloud API routes.

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

**Guided onboarding.** Bare `bitrouter` is the front door: a network-free credential probe launches the wizard when nothing is configured, or prints a one-line status plus a `bitrouter launch` hint when it already is. It never re-onboards and never auto-spawns. `bitrouter init` re-runs it; `--yes` runs it headlessly and scaffolds the starter `bitrouter.yaml`. Every prompt has a flag equivalent — see `references/cli.md` → *Setup helpers*. The two commands below remain the fast path.

**Zero-config (BYOK).** Export any of the supported env vars and start the daemon. It auto-enables every provider whose key is present.

```bash
export OPENAI_API_KEY=sk-...           # openai
export ANTHROPIC_API_KEY=sk-ant-...    # anthropic
export GEMINI_API_KEY=...              # google  (NOT GOOGLE_API_KEY)
export OPENROUTER_API_KEY=sk-or-...    # openrouter
export OPENCODE_ZEN_API_KEY=...        # opencode-zen AND opencode-go (shared)

bitrouter start          # detached daemon, logs to ~/.bitrouter/bitrouter.log
bitrouter status         # green dot + pid / listen / model count
bitrouter status --requests # settled-request table + spend (same piped or not)
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

Config search order, lowest-priority last: `./bitrouter.yaml` → `$BITROUTER_HOME/bitrouter.yaml` → `~/.bitrouter/bitrouter.yaml` → zero-config in-memory. `config validate` runs the real parse path (structure, `derives`, the SSRF gate) and is CI-safe — unset `${VAR}`s become warnings, so no secrets are needed. A JSON Schema for editor autocomplete ships at `dist/schema/bitrouter.config.schema.json`; see `references/cli.md` → *Setup helpers*.

### Durable history (Local)

Two opt-in, restart-only subsystems record routed-request history: `trajectory` (task-neutral progress evidence, off by default) and `continuation` (Responses IDs, always on). Neither hot-reloads. Config keys, validation ranges, the privacy boundary, and `bitrouter trajectory inspect|replay|prune` are in `references/cli.md` → *Durable trajectory operations*.

**Subscription / OAuth providers.** Different — local login, not env vars:

```bash
bitrouter providers login claude-code    # Claude Pro/Max, via the Claude Code session
bitrouter providers login openai-codex   # ChatGPT/Codex subscription
bitrouter providers login github-copilot # browser device flow
bitrouter providers login supergrok      # SuperGrok, via the Grok CLI session
bitrouter providers login google-ai      # Google AI (Antigravity), via the `agy` keyring
```

BYOK providers can also be seeded non-interactively with `--api-key` or `--key-stdin`. Per-provider auth methods, env vars, and protocol notes are in `references/providers.md`.

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

BitRouter exposes its own tools (`complete`, `list_models`, `status`) over MCP. This is the **origin** server — it wraps BitRouter's routing core — and is **distinct** from the MCP gateway at `/mcp`, which proxies the upstream MCP servers declared in `bitrouter.yaml`.

```bash
bitrouter mcp serve                      # stdio → local daemon (the default)
bitrouter mcp install --client claude    # print the mcpServers config block
```

Transport picks the backend: `stdio` → local daemon at `127.0.0.1:4356`; `http` → cloud at `api.bitrouter.ai`, multi-tenant, each client sending its own `Authorization: Bearer`. `--backend skills` serves installed skills over stdio.

See `references/mcp-server.md` for every flag, the tool JSON shapes, and the transport↔backend matrix. `bitrouter skills list|init` is in `references/cli.md` → *Setup helpers*.

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
| `references/harness-hermes-agent.md` | Wiring Hermes Agent persistently (its native plugin; `bitrouter launch -a hermes` synthesizes a throwaway `HERMES_HOME` instead) |
| `references/harness-openclaw.md` | Wiring OpenClaw persistently (its native plugin; `bitrouter launch -a openclaw` synthesizes a throwaway profile instead) |
| `references/adaptive-routing.md` | Generic `bitrouter/auto` routing, trace projections, policy locks, and compatibility |
| `references/workflow-optimization.md` | Version-controlled agentic quality/cost optimization: onboarding, run/review/publish loop, evaluator defaults, and failure semantics |
| `references/harness-terminus-2.md` | Wiring Harbor Terminus-2, session identity, compaction epochs, benchmark capture |
| `references/metering.md` | Cache-aware pricing, charge evidence, usage export, strict benchmark bundles |
| `references/mcp-server.md` | Origin MCP server — all flags, tool shapes, transport/backend details, roadmap |
| `references/updating.md` | `bitrouter update`, channels, package-manager delegation, the status nudge |
| `references/sessions.md` | Per-session ACP substrate — `acp serve\|prompt`, NDJSON format, caller-prepared cwd, one-agent-per-session, turn queue, identity, v1 limits |

`bitrouter chat <agent>` is the interactive terminal client for the same
sessions — inline viewport, `/route` to switch provider mid-session, and a cost
line that always states whose spend it is. See `references/cli.md`.

## 7. Gotchas

- **Always ask Local-or-Cloud first.** The default of "just install locally" is wrong for users who want managed billing — they should never install the daemon at all.
- **Cloud sign-in is `bitrouter cloud login`.** Per-provider OAuth is `bitrouter providers login <provider>` (today: `claude-code`, `github-copilot`, `openai-codex`, `supergrok`, and `google-ai`), not a top-level `login` command.
- **Cloud management is `bitrouter cloud …`.** After `bitrouter cloud login`, run `bitrouter cloud --help` for the subcommand index: `keys`, `usage`, `requests`, `billing`, `policy`, `budget`, `preset`, `byok`. Every leaf accepts `--json`.
- **Local port: `127.0.0.1:4356`.** Old docs (and the upstream README) sometimes say 8787 — those are stale.
- **Cloud endpoints:** `https://api.bitrouter.ai/v1` for the OpenAI shape; `https://api.bitrouter.ai` (no `/v1`) for the Anthropic SDK — same asymmetry as Local.
- **Google's env var is `GEMINI_API_KEY`**, matching Google's own SDKs. `GOOGLE_API_KEY` is not auto-detected; override in `bitrouter.yaml` if you must.
- **Reload propagates env changes:** `export OPENAI_API_KEY=new...; bitrouter reload` updates the running daemon — no restart needed.
- **Trajectory settings do not hot-reload.** Changes to `enabled`, `retention_days`, or `outbox_batch_size` are rejected; restart the daemon. Any signed `progress_guard` requires trajectory to be enabled.
- **`bitrouter providers add/remove/use/test/stats` do not exist.** Provider management is `bitrouter providers list`, `bitrouter providers login <provider>`, and `bitrouter providers logout <provider>`. Edit `bitrouter.yaml` and `bitrouter reload` for config changes.
- **Model ids vs provider pins:** canonical model ids use slashes (`openai/gpt-4o`). An explicit provider pin uses a colon (`openrouter:openai/gpt-4o`, `claude-code:claude-sonnet-4-6`) and is still supported by the routing table.
- **`bitrouter/*` is reserved.** BitRouter resolves the whole namespace itself before any provider lookup, so no provider or registry model may declare an id under it. It holds `bitrouter/auto` (policy-driven routing, see `references/adaptive-routing.md`) and `bitrouter/fusion` (multi-model deliberation). An unrecognised slug is a `400`, not a `404`, and the colon form `bitrouter:auto` is rejected with a pointer to the slash spelling. Send `bitrouter/auto`, not `@auto` — the generic `@preset[:variant]` form still works for every preset, including `auto`, but the slug is the documented spelling.
- **`bitrouter launch` supports every catalog harness with an interactive binary:** `claude`, `codex`, `opencode`, `pi`, `hermes`, `openclaw`, `grok`, `agy`. `grok` and `agy` are **own-auth**: they launch with their own subscription auth and are never routed or metered (the startup line says so), and they additionally remain **providers** the daemon borrows sessions from. `launch` always hands the harness the real terminal — there is no hosted-terminal mode.
- **No `bitrouter doctor`.** Diagnostics are: `bitrouter status`, `bitrouter route <model>`, `bitrouter models`, `bitrouter providers list`, log file at `~/.bitrouter/bitrouter.log`.
