---
name: bitrouter
description: >
  Use when installing, configuring, running, or troubleshooting BitRouter from
  its CLI — an LLM proxy routing OpenAI- or Anthropic-shaped traffic to any
  provider, as a local daemon on 127.0.0.1:4356 (BYOK) or BitRouter Cloud (brk_*
  keys). Covers bitrouter init, provider credentials, wiring a coding agent with
  bitrouter launch, routing, and spend. Trigger on bitrouter.yaml, port 4356,
  brk_ keys, "replace litellm", or pointing a coding agent at a proxy.
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

BitRouter routes OpenAI- or Anthropic-shaped requests to any LLM provider —
either a local Rust daemon at `http://127.0.0.1:4356` (BYOK) or managed cloud at
`https://api.bitrouter.ai/v1` (one bill, no per-provider keys). Everything below
is driven from the `bitrouter` CLI.

## Activate in one pass

Work top to bottom, and **probe before you ask** — on a machine that has used
BitRouter before, most of these steps are already done.

### 1. Probe

```bash
bitrouter --version          # not found -> step 2
bitrouter status             # prints `stopped` and exits 0 when no daemon is up
bitrouter providers list     # ID  MODELS  ACTIVE  API_BASE
```

| What you see | Go to |
|---|---|
| `command not found` | 2 — install |
| Installed, no active providers | 3 — configure |
| Providers active, daemon stopped | `bitrouter start`, then 5 |
| Providers active, daemon running | 5 — wire the harness |

Only ask **Local or Cloud** when step 3 finds no usable credential. Local BYOK
with keys already in the environment needs no signup and no card; Cloud needs an
account and credits, so it is never faster for a user who already has keys.

### 2. Install

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS: `brew install bitrouter/tap/bitrouter`. Node environments:
`npm install -g bitrouter`. Windows:
`powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"`.
Verify with `bitrouter --version`; on failure read `references/diagnose.md`.

### 3. Configure — drive the headless wizard

`bitrouter init --yes` is the scriptable onboarding path. It never blocks on a
human, scaffolds a starter `bitrouter.yaml` (`skip_auth: true`,
`listen: 127.0.0.1:4356`), and **prints a JSON result envelope** — parse it
instead of guessing what happened.

```bash
bitrouter init --yes --use-detected --harness claude --after launch
```

| Envelope field | Read it for |
|---|---|
| `providers_configured` | what is routable now |
| `providers_skipped_interactive` | credentials needing a human — carry these to step 4 |
| `harnesses_installed` | which harness got native wiring |
| `after` / `snippet` | what the wizard did last, and the env snippet to persist |

Useful flags: `--provider <id>` + `--provider-api-key <k>` (repeatable),
`--cloud-login`, `--api-key <brk_…>`, `--harness claude|codex` (repeatable),
`--after launch|serve|exit`, `--model <id>`, `--no-install`, `--write-config`,
`--force`, `--reset`. Bare `bitrouter` runs the same wizard interactively when
nothing is configured; `bitrouter init` re-runs it. Full contract:
`references/cli.md` → *Setup helpers*.

Skip the wizard when you only need zero-config BYOK: export keys and start.

```bash
export OPENAI_API_KEY=sk-...           # openai
export ANTHROPIC_API_KEY=sk-ant-...    # anthropic
export GEMINI_API_KEY=...              # google  (NOT GOOGLE_API_KEY)
export OPENROUTER_API_KEY=sk-or-...    # openrouter
export OPENCODE_ZEN_API_KEY=...        # opencode-zen AND opencode-go (shared)
bitrouter start
```

The daemon auto-enables every provider whose key is present, merges the public
provider registry, and writes runtime files to `~/.bitrouter/`. Rotate a key with
`export ...; bitrouter reload` — no restart. For multi-account, custom endpoints,
or ACP agents write an explicit `bitrouter.yaml` and check it with
`bitrouter config validate -c ./bitrouter.yaml` (CI-safe, no secrets needed);
see `references/providers.md`.

### 4. Credentials that need a human

Subscription and OAuth providers log in locally — these are what
`providers_skipped_interactive` lists.

```bash
bitrouter providers login claude-code    # Claude Pro/Max
bitrouter providers login openai-codex   # ChatGPT/Codex subscription
bitrouter providers login github-copilot # browser device flow
bitrouter providers login supergrok      # via the Grok CLI session
bitrouter providers login google-ai      # via the `agy` keyring
```

BYOK providers accept `--api-key` or `--key-stdin` non-interactively. For Cloud,
`bitrouter cloud login` runs an RFC 8628 device flow; the credential persists,
auto-refreshes, and makes entitled models routable as `bitrouter:<model-id>`
against `localhost:4356` with no `brk_*` paste. See `references/cloud-setup.md`.

### 5. Wire the coding agent

`bitrouter launch` routes a harness's native TUI without editing its config
files — reversible, per-process, and it auto-starts the daemon if it is down.

```bash
bitrouter launch -a claude
bitrouter launch -a codex -- -p "summarize this repo"
```

`-a` accepts `claude`, `codex`, `opencode`, `pi`, `hermes`, `openclaw`, `grok`,
`agy`. Everything after `--` is forwarded verbatim, and a session spend summary
prints on exit. For durable wiring instead of a wrapper, see the harness references.

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

`status --requests` reads the metering store directly — works with no daemon,
prints once, identical piped or not, safe for an agent to call. A routed call
showing up there with a cost is the proof that activation worked.

Canonical model ids use slashes (`openai/gpt-4o`); a provider pin uses a colon
(`openrouter:openai/gpt-4o`). Bare Anthropic ids from Claude Code resolve
through the fallback chain — alias one in `bitrouter.yaml` if it does not.

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
| `references/workflow-optimization.md` | Version-controlled quality/cost optimization loop |
| `references/metering.md` | Cache-aware pricing, charge evidence, usage export |
| `references/sessions.md` | ACP substrate — `acp serve\|prompt`, NDJSON, one agent per session, and `bitrouter chat <agent>` (inline viewport, `/route` mid-session, per-turn cost) |
| `references/updating.md` | `bitrouter update`, channels, the status nudge |

## Gotchas

- **Local port is `127.0.0.1:4356`** — old docs saying 8787 are stale.
- **Cloud endpoints:** `https://api.bitrouter.ai/v1` for the OpenAI shape,
  `https://api.bitrouter.ai` (no `/v1`) for the Anthropic SDK — same locally.
- **Google's env var is `GEMINI_API_KEY`**, matching Google's own SDKs.
  `GOOGLE_API_KEY` is not auto-detected.
- **`bitrouter cloud login` is cloud sign-in.** Per-provider OAuth is
  `bitrouter providers login <provider>`. There is no top-level `login`.
- **`init --harness` only accepts `claude` and `codex`.** Route every other
  harness with `bitrouter launch -a <id>`, not the wizard.
- **`grok` and `agy` are own-auth.** They launch with their own subscription
  auth and are never routed or metered — the startup line says so — while
  remaining providers the daemon borrows sessions from.
- **`providers add/remove/use/test/stats` do not exist.** Management is
  `providers list|login|logout`; edit `bitrouter.yaml` + `bitrouter reload` for
  the rest.
- **No `bitrouter doctor`.** Diagnostics are `status`, `route <model>`,
  `models`, `providers list`, `~/.bitrouter/bitrouter.log`.
- **`bitrouter/*` is reserved** — resolved before any provider lookup, holding
  `bitrouter/auto` and `bitrouter/fusion`. An unrecognised slug is a `400`, not a
  `404`, and `bitrouter:auto` is rejected in favour of the slash spelling.
