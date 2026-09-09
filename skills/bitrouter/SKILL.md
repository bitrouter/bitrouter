---
name: bitrouter
description: >
  Use when installing, configuring, running, or troubleshooting BitRouter from
  its CLI — a self-hosted LLM proxy on 127.0.0.1:4356 routing OpenAI- or
  Anthropic-shaped traffic to any provider, via a coding-agent subscription,
  hosted BitRouter, or your own keys. Covers bro init, provider
  credentials, starting the default ACP TUI with bro, routing, and spend.
  Trigger on bitrouter.yaml, port 4356, brk_ keys, "replace litellm", or
  pointing a coding agent at a proxy.
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags: [llm, proxy, routing, cli, ai-gateway, claude-code, codex]
---

# BitRouter
BitRouter is a self-hosted Rust daemon at `http://127.0.0.1:4356` that routes OpenAI- or Anthropic-shaped requests to providers selected in §4.

## Activate in one pass
Work top to bottom, probing before asking.

### 1. Probe
```bash
bro --version          # not found -> step 2
bro status             # liveness + `spend`; `running: false` when nothing is reachable
bro providers list     # ID  MODELS  ACTIVE  API_BASE
```

These emit JSON by default (`--human` is readable). Branch on the result:
missing command → §2; no active providers → §3; stopped daemon → `start`; both
ready → §5.

### 2. Install
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS: `brew install bitrouter/tap/bitrouter`. Node: `npm install -g bitrouter`.
Windows: `powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"`.
Verify with `bro --version`; on failure read `references/diagnose.md`.

### 3. Configure

A human runs `bro` to complete onboarding using searchable Up/Down lists
of registry providers (including BitRouter Cloud), ACP harnesses and actions.
After setup the same command opens the saved default ACP TUI. Credentials alone do not mark setup complete.
For scripted setup:

```bash
bro init --yes --use-detected --harness codex --after exit
```

This saves `chat.agent: codex-acp` in the resolved config, or the BitRouter home
when no config exists. `--model ID` persists the default model too. Existing
settings are preserved; `--force` resets them. Read
`providers_skipped_interactive` in the JSON report for logins needing a human.
No hand-written provider or agent entry is needed for Codex or Claude ACP.
See `references/cli.md` for flags, config precedence and first-run defaults.

### 4. Choose providers — subscription first
These logins are interactive, so they are what `providers_skipped_interactive`
reports. Work the order below: it buys the same tokens for less money.

**a. The subscription they already pay for.** If the user drives Claude Code or
Codex, log that in first so the harness keeps serving its own models from the
plan they have already bought instead of from metered API calls.

```bash
bro providers login claude-code    # adopts the live Claude Code session
bro providers login openai-codex   # ChatGPT PKCE flow in a browser
bro providers login bitrouter      # hosted; same sign-in as `cloud login`
```

Auth is catalog-derived; `references/providers.md` lists each login method.

**b. Hosted BitRouter for everything else.** Signing in adds a managed
`bitrouter` provider to this daemon; it is not a second deployment.

**c. BYOK for anything they want to own directly.** Export the key and start —
the daemon auto-enables every provider whose key is present, and
`export ...; bro reload` rotates one without a restart.

Detected vars: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` (not `GOOGLE_API_KEY`), `OPENROUTER_API_KEY`, `OPENCODE_ZEN_API_KEY` (zen *and* go).

`providers login` also takes `--api-key` / `--key-stdin`, and
`references/cloud-setup.md` covers the hosted account, credits, and `brk_*`
keys. Net effect: the subscription serves its native models; hosted BitRouter
or BYOK supplements everything it does not cover.

### 5. Start the desired agent interface

BitRouter's full-screen conversation uses temporary operational inspectors:

```bash
bro code                    # conversation with Choose agent picker
bro code codex              # explicit interactive ACP session
bro run claude "summarize this repo"  # headless ACP turn
```

Both built-in adapters require Node.js 22+ and `npx`; a compatible local CLI is
selected automatically behind the pinned adapter. For the harness's own native
interface, use the reversible per-process launcher:

```bash
bro claude
bro codex -- --search
```

`launch <agent>` accepts catalog native harnesses; `claude`, `claude-code`, and
`codex` are shortcuts. Everything after `--` is forwarded verbatim, and user
configuration is not edited.

Leave the harness's own model on its subscription and let BitRouter carry the
rest — subagents, bulk work, models the plan does not include. Pinning the whole
harness off a subscription they already pay for usually costs more, so make it a
deliberate choice rather than a default.

**The restart handoff — say it every time.** Existing harness processes cannot
be rerouted. End with: "run `bro claude` (or restart the harness with the
env override) to route this session." MCP is control/introspection only;
inference goes to the daemon HTTP API.

For an ACP client, use `bro acp serve claude` or
`bro acp serve codex`. Stable ACP v1 on exact adapter pins,
initializing the harness with the client's capabilities and transparently
carrying multiple harness-native sessions on one connection. Native IDs and
history remain harness-owned; `acp_recording.enabled` optionally records the
observable ACP transcript locally; `acp checkpoints` freezes and annotates it. Route leases
(`_bitrouter/route/list|set|reset`) and session-attributed cost are
capability-gated and need a local control binding, which an explicit remote
`--base-url` does not provide. Read `references/sessions.md` — the pins and the
wire contract are there — before reasoning about this surface.

### 6. Verify
```bash
bro route claude-sonnet-4-6   # what would actually run: read `effective_model`
bro models                    # everything routable, with every provider that serves it
bro requests                  # settled requests + spend, JSON (--human for a table)
```

`requests` reads the metering store directly, so it works with no
daemon and is safe for an agent to call — a routed call appearing there, naming
the provider that actually served it, is the proof activation worked. Do **not**
use the cost as that proof: most rows carry no charge evidence and render `?`,
and the rollup reads `unreported` rather than `$0.00` when none does. Canonical
ids use slashes and a pin uses a colon (`openrouter:openai/gpt-4o`);
`references/diagnose.md` has the full spelling rules.

For administration from another computer, use a named `--context`; see
`references/remote-administration.md` for token scopes, tunnel setup, and host
boundaries. Remote errors never fall back to this machine's configuration.

## References — read on demand, not upfront

| File | When to read |
|---|---|
| `references/cli.md` | Full subcommand reference — the primary reference |
| `references/remote-administration.md` | Remote contexts, operator credentials, and host boundaries |
| `references/providers.md` | Add / configure providers, multi-account, custom endpoints, model-id spelling |
| `references/cloud-setup.md` | Cloud signup, key mint, billing, wallet path |
| `references/diagnose.md` | Install issues, daemon won't start, connection refused, model ids |
| `references/harness-*.md` | Durable per-harness wiring instead of `launch`: `-claude-code`, `-codex`, `-hermes-agent`, `-openclaw`, `-terminus-2` |
| `references/migrate-from-*.md` | Migrating off `-litellm`, `-openrouter`, `-openai-compatible` (Azure, Together, Groq, Ollama, LM Studio), `-anthropic-compatible` |
| `references/adaptive-routing.md`, `references/workflow-optimization.md`, `references/metering.md` | `bitrouter/auto`, trace projections, policy locks; history-driven quality/cost optimization; cache-aware pricing, charge evidence, usage export |
| `references/sessions.md`, `references/updating.md` | ACP controller, served vs in-process (`acp serve`, `run`, native sessions, NDJSON, `bro code <agent>`); `bro update` and channels |

## Gotchas

- **Local port is `127.0.0.1:4356`** — old docs saying 8787 are stale. Hosted:
  `https://api.bitrouter.ai/v1` for the OpenAI shape, `https://api.bitrouter.ai`
  (no `/v1`) for the Anthropic SDK — same asymmetry locally.
- **Hosted sign-in is `cloud login` or `providers login bitrouter`** (same flow),
  everything else `providers login <id>`; there is no top-level `login`.
- **Remote control is separate from inference and ACP.** `control.enabled: true`
  starts a control API on `127.0.0.1:4358` with dedicated operator credentials
  (legacy `BITROUTER_CONTROL_TOKEN` grants reads only). Keep it loopback-only behind a
  private tunnel or TLS reverse proxy. `server.skip_auth` never disables this
  authentication, changes under `control:` require a daemon restart, and the
  control API does not run remote ACP sessions.
- **`init --harness` only accepts `claude` and `codex`**; `launch <agent>`
  accepts the native facets listed by `launch --help`.
- **`providers add/remove/use/test/stats` and `bro doctor` do not exist.**
  Manage with `providers list|login|logout` + `bitrouter.yaml`/`reload`; diagnose
  with `status`, `route <model>`, `models`, `~/.bitrouter/bitrouter.log`.
- **`bitrouter/*` is reserved** — resolved before any provider lookup, holding
  `bitrouter/auto` and `bitrouter/fusion`. An unrecognised slug is a `400`, and
  `bitrouter:auto` is rejected in favour of the slash spelling.
