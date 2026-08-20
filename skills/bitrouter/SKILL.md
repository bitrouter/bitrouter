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
bitrouter status             # prints `running: no` when no daemon is reachable
bitrouter providers list     # ID  MODELS  ACTIVE  API_BASE
```

| What you see | Go to |
|---|---|
| `command not found` | 2 — install |
| Installed, no active providers | 3 — configure |
| Providers active, daemon stopped | `bitrouter start`, then 5 |
| Providers active, daemon running | 5 — wire the harness |

These emit **JSON by default** (`--human` renders the readable view) — parse it
rather than scraping prose. Do not open with a deployment question: self-hosted
is the path, and §4 picks the providers.

### 2. Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS: `brew install bitrouter/tap/bitrouter`. Node: `npm install -g bitrouter`.
Windows: `powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"`.
Verify with `bitrouter --version`; on failure read `references/diagnose.md`.

### 3. Configure — drive the headless wizard

`bitrouter init --yes` is the scriptable onboarding path: it never blocks on a
human, scaffolds a starter `bitrouter.yaml` (`skip_auth: true`,
`listen: 127.0.0.1:4356`), and **prints a JSON result envelope** — parse it
rather than guessing what happened.

```bash
bitrouter init --yes --use-detected --harness claude --after launch
```

| Envelope field | Read it for |
|---|---|
| `providers_configured` | what is routable now |
| `providers_skipped_interactive` | credentials needing a human — carry these to step 4 |
| `harnesses_installed` | which harness got native wiring |
| `after` / `snippet` | what the wizard did last, and the env snippet to persist |

Every prompt has a flag: `--provider`/`--provider-api-key`, `--cloud-login`,
`--api-key`, `--harness`, `--after`, `--model`, `--write-config`, `--reset`.
Bare `bitrouter` runs the wizard interactively when nothing is configured. Full
contract: `references/cli.md` → *Setup helpers*.

The daemon writes runtime files to `~/.bitrouter/` and merges the public
provider registry on start. For multi-account, custom endpoints, or ACP agents,
write an explicit `bitrouter.yaml` and check it with `bitrouter config validate
-c ./bitrouter.yaml` (CI-safe, no secrets needed); see `references/providers.md`.

### 4. Choose providers — subscription first

These logins are interactive, so they are what `providers_skipped_interactive`
reports. Work the order below: it buys the same tokens for less money.

**a. The subscription they already pay for.** If the user drives Claude Code or
Codex, log that in first so the harness keeps serving its own models from the
plan they have already bought instead of from metered API calls.

```bash
bitrouter providers login claude-code    # Claude Pro/Max
bitrouter providers login openai-codex   # ChatGPT/Codex subscription
```

**b. Hosted BitRouter for everything else — the recommended default.** Managed
provider routing with OAuth built in, so the user never collects a key per
provider. It is a provider, not a second deployment: signing in adds a
`bitrouter` provider to this same daemon, routable as `bitrouter:<model-id>`.

```bash
bitrouter providers login bitrouter      # same sign-in as `bitrouter cloud login`
```

**c. BYOK for anything they want to own directly.** Export the key and start —
the daemon auto-enables every provider whose key is present, and
`export ...; bitrouter reload` rotates one without a restart.

```bash
export OPENAI_API_KEY=sk-...        ANTHROPIC_API_KEY=sk-ant-...
export GEMINI_API_KEY=...           # google (NOT GOOGLE_API_KEY)
export OPENROUTER_API_KEY=sk-or-... OPENCODE_ZEN_API_KEY=...  # zen AND go
```

`providers login` also takes `--api-key` / `--key-stdin`; `github-copilot`,
`supergrok`, and `google-ai` have their own flows, and `references/cloud-setup.md`
covers the hosted account, credits, and `brk_*` keys. Net effect: the
subscription serves its native models; hosted BitRouter or BYOK supplements
everything it does not cover.

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
harness off a subscription they already pay for usually costs more, so make that
a deliberate choice rather than a default.

**The restart handoff — say it every time.** Wiring cannot reroute the session
that is already running; harnesses read their base URL at startup. End with:
"run `bitrouter launch -a claude` (or restart the harness with the env
override) to route this session."

### 6. Verify

```bash
bitrouter route claude-sonnet-4-6   # resolve a model through the routing table
bitrouter models                    # everything routable
bitrouter status --requests         # settled requests + today's spend
```

`status --requests` reads the metering store directly, so it works with no
daemon and is safe for an agent to call. A routed call showing up there with a
cost is the proof that activation worked. Canonical ids use slashes
(`openai/gpt-4o`); a provider pin uses a colon (`openrouter:openai/gpt-4o`,
`bitrouter:<model-id>`), and bare Anthropic ids from Claude Code resolve through
the fallback chain — alias one in `bitrouter.yaml` if it does not.

## References — read on demand, not upfront

| File | When to read |
|---|---|
| `references/cli.md` | Full subcommand reference — the primary reference |
| `references/cloud-setup.md` | Cloud signup, key mint, billing, wallet path |
| `references/providers.md` | Add / configure providers, multi-account, custom endpoints |
| `references/diagnose.md` | Install issues, daemon won't start, connection refused |
| `references/harness-*.md` | Durable per-harness wiring instead of `launch`: `-claude-code`, `-codex`, `-hermes-agent`, `-openclaw`, `-terminus-2` |
| `references/migrate-from-*.md` | Migrating off `-litellm`, `-openrouter`, `-openai-compatible` (Azure, Together, Groq, Ollama, LM Studio), `-anthropic-compatible` |
| `references/adaptive-routing.md` | `bitrouter/auto`, trace projections, policy locks |
| `references/workflow-optimization.md`, `references/metering.md` | Quality/cost optimization loop; cache-aware pricing, charge evidence, usage export |
| `references/sessions.md`, `references/updating.md` | ACP substrate (`acp serve\|prompt`, NDJSON, `bitrouter chat <agent>`); `bitrouter update` and channels |

## Gotchas

- **Local port is `127.0.0.1:4356`** — old docs saying 8787 are stale. Hosted:
  `https://api.bitrouter.ai/v1` for the OpenAI shape, `https://api.bitrouter.ai`
  (no `/v1`) for the Anthropic SDK — same asymmetry locally.
- **Google's env var is `GEMINI_API_KEY`** — `GOOGLE_API_KEY` is not detected.
- **Sign in with `bitrouter cloud login` or `providers login bitrouter`** (same
  flow). Everything else is `providers login <id>`; there is no top-level `login`.
- **`init --harness` only accepts `claude` and `codex`**; `launch -a` adds
  `opencode` and `pi`. `hermes`, `openclaw`, `grok`, and `agy` are no longer
  `launch`-supported — run them directly or via `spawn`; they remain providers.
- **`providers add/remove/use/test/stats` and `bitrouter doctor` do not exist.**
  Manage with `providers list|login|logout` + `bitrouter.yaml`/`reload`; diagnose
  with `status`, `route <model>`, `models`, `~/.bitrouter/bitrouter.log`.
- **`bitrouter/*` is reserved** — resolved before any provider lookup, holding
  `bitrouter/auto` and `bitrouter/fusion`. An unrecognised slug is a `400`, and
  `bitrouter:auto` is rejected in favour of the slash spelling.
