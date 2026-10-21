---
name: bitrouter
description: >
  Use when installing, configuring, running, or troubleshooting BitRouter from
  its CLI — a self-hosted LLM proxy on 127.0.0.1:4356 routing OpenAI- or
  Anthropic-shaped traffic to any provider, via a coding-agent subscription,
  hosted BitRouter, or your own keys. Covers bro init, provider
  credentials, MCP configuration, versioned skills, native BRO tasks, ACP harnesses, routing, and spend.
  Trigger on bitrouter.yaml, port 4356, brk_ keys, "replace litellm", or
  pointing a coding agent at a proxy.
license: Apache-2.0
metadata:
  author: BitRouterAI
  tags: [llm, proxy, routing, cli, ai-gateway, claude-code, codex]
---

# BitRouter
BitRouter is a self-hosted Rust daemon at `http://127.0.0.1:4356` that routes OpenAI- or Anthropic-shaped requests to providers selected in §4. Native CLI tasks use bounded read workers and database execution records; see `references/cli.md` for permissions and restart limitations. Metered `google` uses Chat Completions with `GEMINI_API_KEY`; native Gemini routes, `google-ai` and Vertex Express are retired.

`bro task managed` submits to the local daemon, which owns native tools/Core and the existing durable journal. Use `--request-id` for exact retries, `managed-status` for passive inspection, `managed-cancel` with the original boot/input key for cancellation, and `managed-answer` for explicit MCP skill consent; continuation requires confirmed release. Custom durable harness clients use the negotiated managed-core HTTP/WebSocket profile; see `references/managed-core.md` for binding, ACKs, results and current limits.

## Activate in one pass
### 1. Probe
```bash
bro --version          # not found -> step 2
bro status             # liveness + `spend`; `running: false` when nothing is reachable
bro providers list     # ID  MODELS  ACTIVE  API_BASE
```

These emit JSON by default (`--human` is readable). Branch on the result:
missing command → §2; no active providers → §3; stopped daemon → `start`; both ready → §5.

### 2. Install
```bash
curl --proto '=https' --tlsv1.2 -LsSf https://bitrouter.ai/install.sh | sh
```

macOS: `brew install bitrouter/tap/bitrouter`. Node: `npm install -g bitrouter`.
Windows: `powershell -ExecutionPolicy Bypass -c "irm https://bitrouter.ai/install.ps1 | iex"`.
Verify with `bro --version`; on failure read `references/diagnose.md`.

### 3. Configure
Bare `bro` ensures a ready background daemon and returns to the shell. Router config
or detected credentials suffice; a verified running target needs neither current client credentials nor YAML. No chat agent is required. Missing setup needs
stdin/stderr terminals. Menus use number/name or keyword + Enter; Enter accepts the
shown default. Secrets are hidden; EOF/Ctrl-C cancels. Scripted setup:
```bash
bro init --yes --use-detected --after exit
bro                         # ensure readiness; idempotent on repeat
```
Optional `--harness codex|claude` saves `chat.agent`; `--model` saves the model. Optional MCP/skill setup reuses existing transactions: `--mcp-name NAME --mcp-from FILE` and `--install-skill SOURCE [--skill NAME] [--skill-ref REF] [--workspace PATH|--global]`. Router progress survives resource failures; inspect `resources` outcomes and retry explicitly. `--yes` never prompts for bundle ambiguity. Settings survive; `--force` resets them.
`init --yes` saves/exits; `--after serve` waits for readiness; `--after launch` invokes
the selected native tool. Read `providers_skipped_interactive`; `agents_selected`
is selection evidence, while deprecated `harnesses_installed` stays empty. Saves are
`saved_only`; daemon `config_state` reports reload/restart needs. Missing non-TTY
setup and readiness timeouts fail. See `references/cli.md` for output/cancellation.

### 4. Choose providers — subscription first
These logins are interactive, so they are what `providers_skipped_interactive`
reports. Work the order below: it buys the same tokens for less money.

**a. The subscription they already pay for.** If the user drives Claude Code or Codex,
log that in first so the harness keeps serving its models from the plan they already bought.

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

`providers login` also takes `--api-key` / `--key-stdin`; `references/cloud-setup.md`
covers hosted accounts, credits, and `brk_*` keys. Hosted BitRouter or BYOK supplements subscriptions.

### 5. Start the desired agent interface

Use single-operation CLI controls or the harness's own native interface.
Observing state never submits, approves, resumes or stops work:

```bash
bro threads list           # authorized durable native Threads
bro run codex "inspect tests" # one headless ACP turn
bro run claude "summarize this repo"  # headless ACP turn
bro task run "fix the failing test" --check "cargo test"  # BRO native task
bro task run "inspect this project" --read-only  # no shell or writes
bro run codex "audit this repo" --background  # supervised; returns run id
bro agents sessions --json  # scriptable supervised-run inventory
bro agents                 # read-only live instances; no daemon/agent start
```

Background runs survive client exit. Inspect with `bro agents show/events <run-id>`; stop and
remove are separate. Foreground `run` denies unmatched permissions; background asks.
Same-worktree runs require the warned override. Read `references/sessions.md` first.

`bro task run` joins `bro serve`, creates a Thread and starts one Turn, streams NDJSON,
defaults to the bundled `bitrouter/auto` policy, and approves its own tools. `--check` adds verification; otherwise
it is `not_requested`. `bro threads` provides explicit start/enqueue/steer,
identified approvals, cancellation, queue resume and checkpoint recovery.
`code/tui/chat/agents attach` return migration guidance; no first-party UI or REPL remains.
`--read-only` permits `read`, `glob`, `grep`; coding adds `write`, unique-span `edit`, and `shell`.
Native Threads load global and ancestor `AGENTS.md` into durable user context, using overrides and model-read nested rules; see [wiring](references/cli.md#native-project-instructions-mcp-and-skills-wiring). Coding exposes configured MCP tools through the same approvals; read-only connects only to explicitly granted skill sources and exposes no MCP tools. Skills discovery publishes metadata without activation. `read` also paginates directories. Shell/verification share the declared server interpreter; no launch-time retry. `run <agent>` remains ACP. Local protocol is v19; opt-in HTTP uses `/agent/v2`. Durable history/keys
survive hot unload. Lost instances/unknown effects never trigger automatic resubmission.
Use `bro threads` for native controls and `bro agents pending/answer/show/events` for supervised ACP; read [CLI controls](references/cli.md#single-operation-native-and-acp-controls) for approvals, retries and recovery.
Resident BRO daemons defer automatic replacement; finish work and use explicit `bro restart` after binary updates.
Built-in ACP adapters require Node.js 22+ and `npx`. For the harness's native
interface, use the reversible per-process launcher:

```bash
bro claude
bro codex -- --search
```

`launch <agent>` accepts catalog native harnesses; `claude`, `claude-code`, and
`codex` are shortcuts. Everything after `--` is forwarded verbatim, and user
configuration is not edited. Keep the harness's model on its subscription by default; use BitRouter for
subagents, bulk work, and models the plan lacks.

**The restart handoff — say it every time.** Existing harness processes need a
restart; tell the user to run `bro claude` to route the new session.

For a native BRO ACP client, use `bro acp serve --model openai/gpt-5` (v1 or draft v2).
EOF detaches; cancel pauses inputs; close keeps history. For external harnesses, use `bro acp serve claude` or `bro acp serve codex`.
Their IDs/history stay harness-owned; see `references/sessions.md`.

### 6. Verify
```bash
bro route claude-sonnet-4-6   # preview; check `policy_decision_executed`
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

## References — read on demand
| File | When to read |
|---|---|
| `references/cli.md` | Full subcommand reference — the primary reference |
| `references/remote-administration.md` | Remote contexts, operator credentials, and host boundaries |
| `references/guardrails.md` | Independent input checker, router binding, and blocked legacy guardrails migration |
| `references/providers.md` | Add / configure providers, multi-account, custom endpoints, model-id spelling |
| `references/decisions.md` | Native Decisions requests, API-key setup, operation routing and tariff limits |
| `references/cloud-setup.md` | Cloud signup, key mint, billing, wallet path |
| `references/diagnose.md` | Install issues, daemon won't start, connection refused, model ids |
| `references/harness-*.md` | Durable per-harness wiring instead of `launch`: `-claude-code`, `-codex`, `-hermes-agent`, `-openclaw`, `-terminus-2` |
| `references/migrate-from-*.md` | Migrating off `-litellm`, `-openrouter`, `-openai-compatible` (Azure, Together, Groq, Ollama, LM Studio), `-anthropic-compatible` |
| `references/adaptive-routing.md`, `references/workflow-optimization.md`, `references/metering.md` | `bitrouter/auto`, trace projections, policy locks; history-driven quality/cost optimization; cache-aware pricing, charge evidence, usage export |
| `references/sessions.md`, `references/updating.md` | ACP controller/supervisor (`acp serve`, foreground/background `run`, `agents`, native sessions, NDJSON, Code); `bro update` and channels |
| `references/decision-native.md` | TypeSafe decision configuration, native BRO context views, evidence recall and decision accounting |

## Gotchas

- Request-check extensions use SDK ExtensionApi and the shared foreground host. Restart the same custom binary, not official `bro restart`. No HTTP service or probe; legacy `plugins.bitrouter-guardrails` blocks startup. See `references/guardrails.md`.
- **A Skill does not grant command execution.** If the host cannot run local
  processes, explain that this local integration is unavailable; do not suggest
  the removed OSS origin MCP server as a fallback.
- Use `bro mcp list/show/add/edit/remove` for local upstream config; writes are saved_only and require explicit restart for startup bindings. Only `mcp check` connects. See [config editing](references/cli.md#mcp-configuration).
- Use `bro skills list/show/install/update/uninstall` for BitRouter-owned local/Git bundles, with `--workspace PATH` or `--global`. Native tasks accept repeatable `--skill`/`--exclude-skill` with `project:ID`, `global:ID` or `mcp:SERVER:URI`; clients require daemon support. MCP roots need per-skill consent, enter source-tagged user data and retain a verified per-Turn cache; approval never grants tools. Read original facts with `skills explain --thread ID --turn ID`, or `--managed-session NAME --request-id KEY`. Native `skill_read` delivers verified bound UTF-8 files as Turn-scoped data; frozen Turn/model budgets and original cache/permission checks apply; lost owners stay recovery-blocked. Supervised Codex/Claude `run --background` supports local prompt instructions with the same scoped flags; MCP-origin external delivery is unsupported. Vendor history/token admission and real sessions remain unverified. See [skills](references/cli.md#versioned-skills).
- MCP upstreams default to modern `server/discover` with rmcp's classified
  legacy fallback. Set `mcp.upstream_protocol: latest` only when an upstream
  must use the legacy `initialize` lifecycle.
- `server.skip_auth: false` protects both model and MCP routes with the same
  `brvk_` virtual-key validation. Static credentials under `mcp_servers` are
  for BitRouter's upstream hop; they do not authenticate a downstream caller.
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
- **Native task HTTP is opt-in and separately privileged.** `agent_api.enabled:
  true` requires a loopback `listen`, `token_env`, and exact `workspaces`
  allowlist. `server.skip_auth` never admits an HTTP task caller.
- **`init --harness` only accepts `claude` and `codex`**; `launch <agent>`
  accepts the native facets listed by `launch --help`.
- **`providers add/remove/use/test/stats` and `bro doctor` do not exist.** Manage
  with `providers list|login|logout` + config/reload; diagnose via `status`, `route`, `models`, and logs.
- **`bitrouter/<id>` selects a named router** before provider lookup; `auto` and
  `fusion` retain compatibility behavior. See `references/cli.md` for details.
