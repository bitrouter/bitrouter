---
name: bitrouter
description: >
  Use when installing, configuring, running, or troubleshooting BitRouter from
  its CLI — a self-hosted LLM proxy on 127.0.0.1:4356 routing OpenAI- or
  Anthropic-shaped traffic to any provider, via a coding-agent subscription,
  hosted BitRouter, or your own keys. Covers bitrouter init, provider
  credentials, wiring a coding agent with bitrouter launch, routing, and spend.
  Trigger on bitrouter.yaml, port 4356, brk_ keys, "replace litellm", or
  pointing a coding agent at a proxy.
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags:
    - llm
    - proxy
    - routing
    - cli
    - ai-gateway
    - claude-code
    - codex
---

# BitRouter

BitRouter is a Rust daemon the user **self-hosts** at `http://127.0.0.1:4356`,
routing OpenAI- or Anthropic-shaped requests to any provider, driven entirely
from the `bitrouter` CLI. No BitRouter account is required to run it — where the
tokens are actually bought is a *provider* choice made in §4, not a different way
of deploying.

## Activate in one pass

Work top to bottom, probing before asking — on a machine that has used BitRouter
before, most of these steps are already done.

### 1. Probe

```bash
bitrouter --version          # not found -> step 2
bitrouter status             # liveness + `spend`; `running: false` when nothing is reachable
bitrouter providers list     # ID  MODELS  ACTIVE  API_BASE
```

These emit **JSON by default** (`--human` renders the readable view) — parse it
rather than scraping prose. Branch on what comes back: `command not found` → §2;
installed but no active providers → §3; providers active but the daemon stopped
→ `bitrouter start`, then §5; both → §5, the harness is all that is left. Do not
open with a deployment question: self-hosted is the path, and §4 picks the
providers.

### 2. Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS: `brew install bitrouter/tap/bitrouter`. Node: `npm install -g bitrouter`.
Windows: `powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"`.
Verify with `bitrouter --version`; on failure read `references/diagnose.md`.

### 3. Configure — drive the headless wizard

`bitrouter init --yes` is the scriptable onboarding path: it never blocks on a
human, scaffolds a starter `bitrouter.yaml` (`skip_auth: true`, `listen:
127.0.0.1:4356`), and **prints a JSON result envelope** — parse it rather than
guessing what happened.

```bash
bitrouter init --yes --use-detected --harness claude --after launch
```

Read `providers_skipped_interactive` off the envelope and carry it to step 4:
those are the credentials that need a human. Every prompt has a flag
(`--provider`, `--harness`, `--after`, `--model`, `--reset`, …); bare
`bitrouter` runs the wizard interactively when nothing is configured. The whole
envelope and every flag: `references/cli.md` → *Setup helpers*.

The daemon writes runtime files to `~/.bitrouter/` and merges the public provider
registry on start. For multi-account, custom endpoints, or ACP agents, write an
explicit `bitrouter.yaml` and check it with `bitrouter config validate -c
./bitrouter.yaml` (CI-safe, no secrets); see `references/providers.md`.

### 4. Choose providers — subscription first

These logins are interactive, so they are what `providers_skipped_interactive`
reports. Work the order below: it buys the same tokens for less money.

**a. The subscription they already pay for.** If the user drives Claude Code or
Codex, log that in first so the harness keeps serving its own models from the
plan they have already bought instead of from metered API calls.

```bash
bitrouter providers login claude-code    # adopts the live Claude Code session
bitrouter providers login openai-codex   # ChatGPT PKCE flow in a browser
bitrouter providers login bitrouter      # hosted; same sign-in as `cloud login`
```

Auth method is catalog-derived and differs per provider, so do not promise a
browser prompt that will not appear — `references/providers.md` → *Known
providers* lists what each login actually does.

**b. Hosted BitRouter for everything else — the recommended default.** Managed
provider routing with OAuth built in, so the user collects no per-provider key.
It is a provider, not a second deployment: signing in adds a `bitrouter`
provider to this daemon, routable as `bitrouter:<model-id>`.

**c. BYOK for anything they want to own directly.** Export the key and start —
the daemon auto-enables every provider whose key is present, and
`export ...; bitrouter reload` rotates one without a restart.

Detected vars: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY` (not
`GOOGLE_API_KEY`), `OPENROUTER_API_KEY`, `OPENCODE_ZEN_API_KEY` (zen *and* go).

`providers login` also takes `--api-key` / `--key-stdin`, and
`references/cloud-setup.md` covers the hosted account, credits, and `brk_*`
keys. Net effect: the subscription serves its native models; hosted BitRouter
or BYOK supplements everything it does not cover.

### 5. Wire the coding agent

`bitrouter launch` routes a harness's native TUI without editing its config
files — reversible, per-process, and it auto-starts the daemon if it is down.

```bash
bitrouter launch -a claude
bitrouter launch -a codex -- -p "summarize this repo"
```

`-a` accepts `claude`, `codex`, `opencode`, and `pi` (catalog ids `claude-acp`,
`codex-acp`, `pi-acp` also resolve). Everything after `--` is forwarded verbatim,
and a session spend summary prints on exit. For durable wiring instead of a
wrapper, see the harness references.

Leave the harness's own model on its subscription and let BitRouter carry the
rest — subagents, bulk work, models the plan does not include. Pinning the whole
harness off a subscription they already pay for usually costs more, so make it a
deliberate choice rather than a default.

**The restart handoff — say it every time.** Wiring cannot reroute the session
already running; harnesses read their base URL at startup. End with: "run
`bitrouter launch -a claude` (or restart the harness with the env override) to
route this session." One exception: the origin MCP server (`bitrouter mcp
install --client claude|cursor`) exposes a `complete` tool that offloads a
subtask *inside* the running session — if they cannot relaunch, say it exists
and send them to `bitrouter mcp --help`.

For a programmatic ACP manager, use `bitrouter spawn claude-acp --serve` or
`bitrouter spawn codex-acp --serve`. Stable ACP v1 on exact adapter pins,
initializing the harness with the manager's capabilities and transparently
carrying multiple harness-native sessions on one connection; BitRouter owns
none of their IDs, transcripts, or storage. Route leases
(`_bitrouter/route/list|set|reset`) and session-attributed cost are
capability-gated and need a local control binding, which an explicit remote
`--base-url` does not provide. Read `references/sessions.md` — the pins and the
wire contract are there — before reasoning about this surface.

### 6. Verify

```bash
bitrouter route claude-sonnet-4-6   # what would actually run: read `effective_model`
bitrouter models                    # everything routable
bitrouter status --requests         # settled requests + spend, JSON (--human for a table)
```

`status --requests` reads the metering store directly, so it works with no
daemon and is safe for an agent to call — a routed call appearing there, naming
the provider that actually served it, is the proof activation worked. Do **not**
use the cost as that proof: most rows carry no charge evidence and render `?`,
and the rollup reads `unreported` rather than `$0.00` when none does. Canonical
ids use slashes and a pin uses a colon (`openrouter:openai/gpt-4o`);
`references/diagnose.md` has the full spelling rules.

## References — read on demand, not upfront

| File | When to read |
|---|---|
| `references/cli.md` | Full subcommand reference — the primary reference |
| `references/providers.md` | Add / configure providers, multi-account, custom endpoints, model-id spelling |
| `references/cloud-setup.md` | Cloud signup, key mint, billing, wallet path |
| `references/diagnose.md` | Install issues, daemon won't start, connection refused, model ids |
| `references/harness-*.md` | Durable per-harness wiring instead of `launch`: `-claude-code`, `-codex`, `-hermes-agent`, `-openclaw`, `-terminus-2` |
| `references/migrate-from-*.md` | Migrating off `-litellm`, `-openrouter`, `-openai-compatible` (Azure, Together, Groq, Ollama, LM Studio), `-anthropic-compatible` |
| `references/adaptive-routing.md`, `references/workflow-optimization.md`, `references/metering.md` | `bitrouter/auto`, trace projections, policy locks; history-driven quality/cost optimization; cache-aware pricing, charge evidence, usage export |
| `references/sessions.md`, `references/updating.md` | ACP controller, served vs in-process (`acp serve\|prompt`, native sessions, NDJSON, `bitrouter chat <agent>`); `bitrouter update` and channels |

## Gotchas

- **Local port is `127.0.0.1:4356`** — old docs saying 8787 are stale. Hosted:
  `https://api.bitrouter.ai/v1` for the OpenAI shape, `https://api.bitrouter.ai`
  (no `/v1`) for the Anthropic SDK — same asymmetry locally.
- **Hosted sign-in is `cloud login` or `providers login bitrouter`** (same flow),
  everything else `providers login <id>`; there is no top-level `login`.
- **`init --harness` only accepts `claude` and `codex`**; `launch -a` adds
  `opencode` and `pi`. `hermes`, `openclaw`, `grok`, and `agy` are no longer
  `launch`-supported — run them directly or via `spawn`; they remain providers.
- **`providers add/remove/use/test/stats` and `bitrouter doctor` do not exist.**
  Manage with `providers list|login|logout` + `bitrouter.yaml`/`reload`; diagnose
  with `status`, `route <model>`, `models`, `~/.bitrouter/bitrouter.log`.
- **`bitrouter/*` is reserved** — resolved before any provider lookup, holding
  `bitrouter/auto` and `bitrouter/fusion`. An unrecognised slug is a `400`, and
  `bitrouter:auto` is rejected in favour of the slash spelling.
