# Design: BitRouter as Claude Code & Codex agent plugins

> **Status:** working draft for review — not product docs (deliberately outside
> `docs/`, so no `.zh.md` sibling is required yet).
>
> **Terminology:** this doc says **agent plugin** for the Claude Code / Codex
> installable package, to avoid collision with the repo's existing Rust router
> plugins under `plugins/` (guardrails, observe, attestation), which are
> unrelated.

## 1. Problem & goals

Onboarding BitRouter from a coding agent today takes three separately-discovered
steps: install the skill (`npx skills add bitrouter/bitrouter`), install the
MCP server (`bitrouter mcp install --client claude`), and wire the harness
(`bitrouter spawn` / env vars / `~/.codex/config.toml`). Each lives in a
different doc. None is discoverable from inside the agent itself.

**Goal:** one marketplace install (`/plugin install bitrouter`) that bundles
skill + MCP + hooks into a versioned package for Claude Code and Codex, so a
user who has never heard of BitRouter can go from "found it in the marketplace"
to "traffic routing through the daemon" inside a single agent session.

**Non-goals (v1):**

- Antigravity / Grok Build shims — deferred until each is tested end-to-end
  (Antigravity piggybacks the Claude format, so it may come nearly free later).
- Rerouting the *current* session's own inference. Structurally impossible from
  inside a plugin — see §2. We do not pretend otherwise.
- Silently editing user config (`~/.claude/settings.json`,
  `~/.codex/config.toml`). Trust-destroying; everything durable is
  show-diff-and-confirm, everything else goes through `bitrouter spawn`.

## 2. What a plugin can and cannot do (grounding)

Both plugin systems extend the harness *sideways* — skills, tools, hooks. They
cannot repoint the harness's own inference backend:

- Claude Code reads its backend from `ANTHROPIC_BASE_URL`/auth env or user
  settings; a plugin's `settings.json` honors only `agent` and
  `subagentStatusLine`.
- Codex reads `model_provider` from `~/.codex/config.toml` / `-c` overrides;
  plugins ship skills/hooks/MCP, none of which set it.

So the plugin is **not** how BitRouter routes. It is how BitRouter becomes
**discoverable, self-installing, and observable** inside the agent. The actual
rewiring is done by the thing that already does it deterministically:
`bitrouter spawn -a claude|codex` (env/`-c` overrides, daemon auto-start,
never touches config files — [CLI.md](CLI.md) §`bitrouter spawn`).

**The bootstrapping honesty rule:** when the skill finishes onboarding, the
*current* session is still running on its original backend. The skill must end
with an explicit handoff — "run `bitrouter spawn -a claude` (or restart with
the env override) to route this harness" — never imply the running session got
cheaper. One exception softens this: the bundled **origin MCP server** gives
in-session value immediately (§5.3), no restart needed.

## 3. Value analysis — what's actually new, ranked

Marginal value over the status quo (standalone skill + docs), most→least:

| # | Capability | Why it's uniquely a plugin win | MVP? |
|---|---|---|---|
| 1 | **Marketplace distribution** | Discovery inside the agent; one install; versioned updates. The entire funnel improvement. | P0 |
| 2 | **Composition** | skill + MCP + hooks arrive together and stay in version lockstep, vs. three manual installs that drift | P0 |
| 3 | **Ambient hooks** (SessionStart status, auto-reload on config edit) | A bare skill is inert until invoked. Hooks give BitRouter a heartbeat inside the session: daemon health, "not routed" warnings, zero-friction `bitrouter reload`. Full utilization map in §5.2 | P0 |
| 4 | **Cost feed** (one events core, N renderers) | The onboarding aha-moment: savings shown on the user's own workload. Session-boundary + per-tool-call renderers are **cross-harness** (CC + Codex); live in-context streaming is a CC-monitor enhancement. §5.4 | **P0** |
| 5 | **In-session model arbitrage** (origin MCP: `complete`, `list_models`, `status`) | Offload bulk/mechanical subtasks to a cheap model *right now*, without restart — the only piece that dodges the bootstrapping paradox | P0 |
| 6 | **Loop-optimizer subagent** (`bitrouter:loop-optimizer`) | Translates the user's observed agentic loop into BitRouter policy config — the "act" arm of observe→evaluate→act, running inside the harness. §5.6 | P1 |
| 7 | **Enable-time config prompt** (`userConfig`, Claude Code) | Local-vs-Cloud choice + `brk_` key straight into the OS keychain at install time — beats hand-editing settings | P1 |
| 8 | **Statusline spend HUD** | Plugin `settings.json` can only set `subagentStatusLine`; main statusline needs user-consented wiring by the skill | P1 |

**Kill list** (considered, rejected — with reasons, so we don't re-litigate):

- **LSP servers** — irrelevant to a router.
- **Generic "diagnostician" subagent** — the skill already covers install/
  diagnose flows; a subagent adds no capability there, only drift surface.
  (Distinct from the **loop-optimizer** subagent, §5.6, which owns a real
  workload the skill can't: multi-file loop analysis → policy-spec synthesis.)
- **`bin/` binary shim** — the CLI installs via brew/npm/installer; a
  plugin-PATH copy creates version-skew confusion.
- **`settings.json` `agent` override** (replacing the main-thread agent) —
  wrong product; BitRouter is infrastructure, not a persona.
- **Per-prompt cost injection via `UserPromptSubmit`** — noise; monitors do
  this better with aggregation.
- **Silent auto-rewire on install** — the single fastest way to get the plugin
  flagged as hostile. Never.
- **Themes / channels** — no.

The two-persona framing that drives the MVP cut:

- **Persona A (prospective user):** wants the funnel — discover → install →
  guided setup → routed. Served by #1–#3.
- **Persona B (existing BitRouter user):** daemon already wired; wants ambient
  observability and in-session control. Served by #3, #4, #6, #7. Likely the
  *stickier* audience — the plugin becomes BitRouter's cockpit inside the
  agent.

## 4. Repo layout

Repo root doubles as plugin root **and** (for Claude Code) marketplace root.
Manifests are additive dotted dirs; the payload is referenced in place — no
vendoring, no second copy of the skill:

```text
bitrouter/                        # repo root == plugin root == CC marketplace root
├── .claude-plugin/
│   ├── plugin.json               # CC manifest (skills override, inline hooks + MCP)
│   └── marketplace.json          # repo doubles as a CC marketplace
├── .codex-plugin/
│   ├── plugin.json               # Codex manifest
│   ├── mcp.json                  # Codex MCP config (interior paths confirmed valid — R-1)
│   └── hooks.json                # Codex lifecycle hooks (SessionStart + Stop — R-7)
├── skills/
│   └── bitrouter/                # existing skill — single source of truth, unchanged
├── plugins/                      # existing Rust router plugins — UNRELATED
└── mcp/                          # existing origin MCP server crate — UNRELATED
```

Why this works without restructuring:

- The Claude Code manifest is optional-with-overrides: `name` is the only
  required field, and component paths (`skills`, `hooks`, `mcpServers`) may
  point anywhere under the plugin root as `./…` paths — or be **inline
  objects**, which is how we avoid dropping a `hooks/` dir or `.mcp.json` at
  repo root (a root `.mcp.json` would double as *contributor* project MCP
  config for everyone opening this repo in Claude Code — avoid).
- `skills/bitrouter/SKILL.md` already matches the plugin skill layout
  (`skills/<name>/SKILL.md`) byte-for-byte. The plugin ships the same files
  the standalone `bitrouter skills add` / `npx skills add` paths serve.
- **Skill-scan hygiene:** `skills/` also contains the dev-only `verify` skill
  (ACP substrate verification). The CC `skills` manifest field normally *adds*
  to the default `skills/` scan, but for a marketplace entry whose source
  resolves to the marketplace root, declaring explicit subdirectories
  **replaces** the scan — so `"skills": ["./skills/bitrouter"]` keeps `verify`
  out of marketplace installs. It still leaks under `--plugin-dir .` dev
  testing — resolved by relocating `verify` to `.claude/skills/` (R-2, a P0
  action item), after which `skills/` is purely shippable payload on every
  rail.

Trade-off accepted: a marketplace git install clones the full monorepo
(~46 MiB pack today). Tolerable for v1; mitigations (slim npm package for
Codex, CI-built mirror repo) are P2 (§8).

## 5. Component design

### 5.1 Manifests

`.claude-plugin/plugin.json`:

```json
{
  "name": "bitrouter",
  "displayName": "BitRouter",
  "version": "0.1.0",
  "description": "Cost-optimize your agentic loops: route every model call through the cheapest viable path. Bundles the /bitrouter setup skill, live daemon status, and in-session model arbitrage tools.",
  "author": { "name": "BitRouterAI" },
  "homepage": "https://bitrouter.ai",
  "repository": "https://github.com/bitrouter/bitrouter",
  "license": "Apache-2.0",
  "keywords": ["llm", "router", "gateway", "cost", "openai", "anthropic", "mcp"],
  "skills": ["./skills/bitrouter"],
  "hooks": {
    "hooks": {
      "SessionStart": [
        {
          "hooks": [
            {
              "type": "command",
              "command": "command -v bitrouter >/dev/null 2>&1 && bitrouter status --agent || echo 'BitRouter CLI not installed — the bitrouter skill can set it up.'"
            }
          ]
        }
      ],
      "FileChanged": [
        {
          "matcher": "bitrouter.yaml",
          "hooks": [{ "type": "command", "command": "bitrouter reload" }]
        }
      ]
    }
  },
  "mcpServers": {
    "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
  },
  "experimental": {
    "monitors": [
      {
        "name": "cost-feed",
        "command": "bitrouter events --follow --format agent",
        "description": "Session spend, savings vs. frontier, failovers"
      }
    ]
  }
}
```

(Hooks and monitors are declared inline rather than as `hooks/` /
`monitors/` dirs precisely so the repo root stays clean — see §4.)

`.codex-plugin/plugin.json`:

```json
{
  "name": "bitrouter",
  "displayName": "BitRouter",
  "version": "0.1.0",
  "description": "Cost-optimize your agentic loops through one local gateway.",
  "homepage": "https://bitrouter.ai",
  "repository": "https://github.com/bitrouter/bitrouter",
  "license": "Apache-2.0",
  "skills": "./skills/bitrouter",
  "mcpServers": "./.codex-plugin/mcp.json",
  "hooks": "./.codex-plugin/hooks.json"
}
```

`.claude-plugin/marketplace.json` (repo as its own marketplace):

```json
{
  "name": "bitrouter",
  "owner": { "name": "BitRouterAI" },
  "plugins": [{ "name": "bitrouter", "source": "./", "description": "…" }]
}
```

Install paths: `/plugin marketplace add bitrouter/bitrouter` →
`/plugin install bitrouter@bitrouter`; later, community-marketplace submission
makes it `/plugin install bitrouter@claude-community` with SHA auto-bumping.

### 5.2 Hooks — full utilization map

Claude Code exposes ~30 lifecycle events. Sweeping all of them against
BitRouter's thesis (route/observe/govern the loop) yields four keepers, three
exploratory bets, and a pile of rejects. The map, so we never re-sweep:

| Event | Use for BitRouter | Verdict |
|---|---|---|
| `SessionStart` | One-line routing status + context injection (below) | **P0** |
| `FileChanged` (matcher: `bitrouter.yaml`) | Auto-run `bitrouter reload` when the user or an agent edits the config — routing edits take effect mid-session with zero friction | **P0** (trivial, delightful) |
| `StopFailure` | Fires when a turn dies on an API error (rate limit, outage). Hook output is *ignored* by the harness, so no direct message — but the hook can drop a marker file that the next `SessionStart` reads: "your last turn died on a rate-limit; routed through BitRouter, failover would have absorbed it." The single sharpest onboarding trigger we have — it fires at the exact moment of pain | P1 |
| `SubagentStart` / `SubagentStop` | Post span markers to the daemon so the cost feed attributes spend per subagent ("code-reviewer: $0.31"). Honest caveat: harness gives us no per-subagent request tagging, so time-window attribution is **approximate** and degrades under parallel subagents — fine for a HUD, never for billing | P1 (pairs with §5.4) |
| `SessionEnd` | Opt-in: ship turn/outcome metadata (hook receives the transcript path) into the observe→evaluate→act eval loop — the "observe" arm of §5.6. Privacy-sensitive; needs explicit consent design, off by default | P2 |
| `PreToolUse` (matcher: `Task`) | `updatedInput` is confirmed on both platforms (R-8): BitRouter policy could downgrade subagent `model` selection at spawn time — actual routing *inside* the harness. Feasible; gated on a consent story, not capability | P2 spike |
| `Setup` | Headless daemon install for CI images (`claude --init-only`) | P2 |
| `Stop` (per-turn cost summary) | On CC: rejected — the monitor (§5.4) delivers the same signal with aggregation. On **Codex**: adopted — no monitor exists there, and `Stop` fires at every turn end (R-7), making it the Codex per-turn cost renderer (`bitrouter events --turn`) | **P0 on Codex**; killed on CC |
| `UserPromptSubmit`, `PostToolUse*`, `PreCompact`, `Notification`, `Permission*`, `InstructionsLoaded`, `CwdChanged`, `Worktree*`, `Elicitation*`, `TeammateIdle`, themes of that ilk | No routing/observability angle that survives the noise-budget test | Killed |

Two design rules bind every hook we ship:

1. **All logic lives in the Rust CLI, not shell scripts.** Hooks are
   one-liners invoking purpose-built subcommands (`bitrouter status --agent`,
   `bitrouter reload`). Keeps drift inside the codebase where the CLAUDE.md
   lockstep rule operates, keeps hooks auditable at trust-prompt time (a
   reviewer sees `bitrouter reload`, not 40 lines of bash), and keeps them
   unit-testable.
2. **Read-only or user-initiated-write only.** `reload` re-reads a config the
   *user* edited; no hook ever mutates harness or router config itself.

**SessionStart contract (P0)** — constraints in priority order:

1. **Graceful when the binary is missing** — the hook fires before BitRouter
   is installed (that's the point of the plugin). One-line pointer, exit 0.
2. **Noise budget** — fires *every* session. Healthy + routed ⇒ at most one
   short line. Never multi-line dumps.
3. **Latency budget** — session start is on the critical path. Local
   socket/pidfile check only; hard sub-100 ms target; never hit the network.

**`bitrouter status --agent`** (name settled per R-4) emits exactly one of:

- `BitRouter: routing active — daemon :4356 up, N providers, this session IS
  routed through it` (detected via `ANTHROPIC_BASE_URL` pointing at the
  daemon)
- `BitRouter: daemon up, but this session is NOT routed through it (run
  'bitrouter spawn -a claude' or ask the bitrouter skill to wire it)`
- `BitRouter: installed but daemon not running — 'bitrouter start' brings it
  up`

…plus, when a savings ledger exists (§5.4), the routed line appends the
session-boundary recap — `last session $0.42 (saved 86%) · lifetime saved
$214` — which is the **universal in-band cost-feed anchor** (SessionStart
fires on both CC and Codex). And when the P1 `StopFailure` marker is present,
one extra line noting the prior session's API-error death and that failover
would have survived it.

**Codex parity (resolved, R-7):** Codex plugin hooks expose 10 events —
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`,
`PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`,
`Stop` — with the same `hooks.json` shape as Claude Code, JSON on stdin, and
even `CLAUDE_PLUGIN_ROOT`/`CLAUDE_PLUGIN_DATA` compat env aliases. So Codex
MVP ships **SessionStart + Stop** (status line + per-turn cost); the
`SubagentStart/Stop` span-attribution idea (P1) works on both platforms.
What Codex lacks: `FileChanged` (auto-reload is CC-only), `StopFailure`, and
`SessionEnd`. Codex hooks are untrusted until the user reviews them — our
read-only `bitrouter` one-liners survive that review easily.

### 5.3 Origin MCP server (P0)

Bundled config launches the *existing* `bitrouter mcp serve` (stdio → local
daemon). Zero new code. What it buys:

- **`complete`** — in-session model arbitrage: "draft these 40 test stubs via
  `complete` on `deepseek/…`" while the main loop stays on the frontier model.
  This is the only plugin feature that delivers value *before* the user
  restarts into a routed session.
- **`status` / `list_models`** — the agent can self-diagnose routing and
  browse the catalog without shelling out.

Failure mode: binary not installed ⇒ server shows as errored in `/plugin`
until setup completes. Acceptable; the skill and the SessionStart line both
explain it. On Codex, bundled MCP servers **don't auto-enable** — the skill
must walk the user through enabling (platform rule, not ours).

Honest limitation: tool-based arbitrage is *opt-in* routing — it depends on
the model choosing the tool. The skill should nudge ("prefer `complete` for
bulk mechanical subtasks"), but transparent routing via `spawn` remains the
real product. Don't oversell this in marketing copy.

### 5.4 Cost feed (P0) — one events core, N renderers

Routing is invisible when it works. The cost feed makes the first routed
session *show* its own wins — that's why it belongs in onboarding, not in a
later "power user" tier: a user who installs, wires, restarts, and then sees

```text
cost-feed: session $0.42 · would be $3.10 at frontier list price · saved 86%
cost-feed: openai rate-limited at request 141 → failed over to bedrock, 0 lost
```

has the product's entire pitch demonstrated on their own workload within
minutes. The **counterfactual-savings line** (actual spend vs. what the same
calls would have cost at frontier list price) is the headline metric; the
daemon already attributes per-request cost, so the counterfactual is a
pricing-table lookup away.

**Framing rule — two audiences, two products.** The onboarding aha targets
the **user's eyes**; only reactive behaviors (arbitrage nudges, spend-cap
warnings) need the **agent's context**. Defining the feature as
"context-injected stream" would wrongly gate it on the one mechanism only
Claude Code has (monitors). Instead: a single harness-agnostic **events
core** in the CLI/daemon, consumed by thin per-surface **renderers**.

**Events core** — `bitrouter events` (name settled per R-4), the P0 CLI work:

- **Aggregate, never stream.** In-band lines cost tokens; a cost feed that
  streams per-request lines is self-satire. Emission rules: (a) failover /
  rate-limit-reroute events, immediately; (b) spend-threshold crossings
  (default every $1); (c) rolling session summary with the counterfactual
  line, at most every 10 min and only when spend changed. Steady-state
  budget: **≤ 6 lines/hour**.
- **Graceful daemon absence.** Wait quietly, poll at low frequency
  (`--wait-daemon`); never spin, never emit error lines into context.
- **Strictly read-only.** Implementation anchor (per R-9): every settled
  request is already persisted unconditionally in the `requests` table
  (`bitrouter.db`) with model, provider, tokens, `estimated_charge_micro_usd`,
  latency, and error — so `events` v1 is a **throttled DB tail** on a
  `created_at` cursor, no daemon protocol change. (`bitrouter-observe` is
  push-only OTLP — not usable here. Live push over the control socket is a
  later upgrade.) A `--turn`/`--since` one-shot mode serves per-turn hook
  queries.
- **Savings ledger** — cumulative per-session/lifetime totals persisted in
  the existing optional `bitrouter.db`, so session-boundary renderers can say
  "lifetime saved $214".
- **Honesty asterisk:** the counterfactual is approximate (same token volumes
  × frontier reference price) — HUD-grade, never invoice-grade. Docs and
  skill say so.

> **v1 as-built (P0 implementation):** the shipped renderers report **spend,
> not savings** — the counterfactual line needs pricing/routing plumbing that
> belongs with the P1 `bitrouter usage` work, and a savings line computed
> against a single-provider BYOK config would read "$0 saved", which is worse
> than absent. Concretely: the SessionStart recap says *"Spend today $X (N
> requests), $Y this month"* (per-session attribution doesn't exist in the
> metering rows — R-9 — so "last session/lifetime saved" wording waits for
> it); the MCP footer reports spend-today (per-call cost needs request-id
> correlation); `--turn` and `--follow` report turn/session spend + failures.
> The counterfactual-savings headline remains the design target for P1.

**Renderers**, by reach:

| Renderer | Reaches | Works on | Tier |
|---|---|---|---|
| **SessionStart savings recap** — `status --agent` (§5.2) appends a ledger line: *"last session $0.42 (saved 86%) · lifetime saved $214"* | agent + user, in-band | **CC + Codex** (SessionStart on both); likely Antigravity/Grok later | **P0** — the universal in-band anchor; once per session, zero noise |
| **`spawn` exit summary** — savings printed after the child harness exits | user | any harness `spawn` wraps | **P0** — trivial, zero risk |
| **MCP tool-result piggyback** — every origin-server result (`complete`, `status`) carries a one-line cost footer: *"this call $0.003 vs $0.09 frontier · session $1.87"* | agent + user, in-band | **CC + Codex** (any MCP client) | **P0** — cheap; makes arbitrage self-demonstrating |
| **Codex `Stop`-hook turn summary** — Codex's `Stop` hook fires at every turn end (R-7); it runs `bitrouter events --turn` against the local DB and returns the spend line via `systemMessage`/`additionalContext` | agent + user, per turn | **Codex** (and available on CC, where the monitor supersedes it) | **P0** — upgrades Codex from session-boundary to per-turn |
| **CC monitor stream** — live in-context feed, inline under `experimental.monitors` (§5.1) | agent + user, live | CC only (≥ 2.1.105, experimental, interactive sessions only; skipped for project-scope skills-dir plugins — marketplace installs fine) | **P0** — honestly labeled a *CC enhancement*, not the feature itself |
| **`spawn --hud` live bar** — PTY scroll-region or terminal-title HUD | user, live | universal | **Stretch** — interposing a PTY under a full-TUI child is real multiplexer work; title writes can interleave with the child's escape stream. Spike first |
| Desktop notifications (threshold/failover) | user | universal | P1, opt-in |
| `bitrouter top` TUI / local dashboard page | user, live | universal, even non-spawn setups | P1/P2 — the "second pane" answer; can reuse #604 TUI components |
| ~~Codex `notify` wiring~~ | — | — | Dropped (R-7): `notify` emits only `agent-turn-complete` with no usage fields — the `Stop` hook supersedes it |
| ~~Injecting cost text into the LLM response stream~~ | — | — | **Rejected hard**: mutating model output breaks tool-call parsing and trust |

**Why P0 stays defensible:** the P0 bundle (core + first four renderers) is
four-fifths cross-harness; the CC monitor rides along at ~10 manifest lines.
Codex users get a real aha too — routed session ends → exit summary; next
session opens → ledger recap in context. Session boundaries are exactly when
onboarding attention peaks. The renderer split is also what makes future
surfaces (Antigravity, Grok, native Hermes/OpenClaw integrations) a renderer
each, not a redesign.

**What we do not promise:** *mid-turn* live streaming on Codex. The `Stop`
hook (R-7) delivers per-turn granularity, but nothing fires while a turn is
in flight — Codex has no monitor equivalent. Stakeholder answer: "per-turn +
per-tool-call + session-boundary on Codex; fully live on Claude Code." If
Codex grows a push channel, the same `bitrouter events --format agent` slots
in unchanged.

### 5.5 userConfig (P1, Claude Code)

Enable-time prompt replaces the skill's "ask Local or Cloud first" for plugin
users, and stores the `brk_` key in the OS keychain (`sensitive: true`)
instead of a settings file:

```json
"userConfig": {
  "mode": {
    "type": "string", "title": "Local daemon or Cloud",
    "description": "local = daemon at 127.0.0.1:4356 (BYOK). cloud = api.bitrouter.ai with a brk_ key.",
    "default": "local"
  },
  "cloud_api_key": {
    "type": "string", "title": "BitRouter Cloud key (brk_…)",
    "description": "Only needed for cloud mode", "sensitive": true
  }
}
```

`${user_config.cloud_api_key}` then feeds the MCP server env
(`BITROUTER_TOKEN`) for the stdio→cloud path. Caveat: keychain storage is
shared with OAuth tokens under a ~2 KB cap — keys are small, fine.

### 5.6 Loop-optimizer subagent (P1)

The one subagent that earns its place: **`bitrouter:loop-optimizer`**
translates the user's *observed* agentic loop into BitRouter policy
configuration — the "act" arm of the README's observe→evaluate→act cycle,
running inside the harness where the loop actually lives.

**Workload** (why a subagent and not the skill): multi-file analysis across
the harness config, `CLAUDE.md`/workflow definitions, CI scripts, the
existing `bitrouter.yaml`, and the daemon's observed traffic stats — then
synthesizing a policy diff. That's a long-context, tool-heavy task worth
isolating from the main thread; the skill stays the thin front door that
launches it.

**Inputs → output:**

- Reads: `bitrouter.yaml` (or absence thereof), `bitrouter models` /
  `bitrouter route` resolutions, and per-model/per-hop usage stats. The
  stats surface is the hard dependency: local usage attribution exists in the
  daemon but has **no CLI query surface today** (`providers stats` was
  explicitly removed; cloud has `bitrouter cloud usage`) — a `bitrouter
  usage` local query command is the P1 gating work — scoped by R-9: a pure
  local read of the `requests` table via new group-by aggregates on
  `MeteringStore`, no daemon or schema change.
- Produces: a proposed `bitrouter.yaml` diff + rationale + projected savings
  ("80% of your Task-subagent calls are file summarization billed at frontier
  prices; alias them to `deepseek/…` — projected −$41/week").
- **Never auto-applies.** Diff → user confirms → file write → the
  `FileChanged` hook (§5.2) reloads the daemon → the cost feed (§5.4) shows
  the delta. That chain *is* the self-improving loop, rendered entirely in
  plugin primitives — optimizer acts, hook applies, monitor verifies.

**Frontmatter sketch** (`agents/loop-optimizer.md`):

```yaml
---
name: loop-optimizer
description: Analyze this project's agentic loop and observed BitRouter
  traffic, then propose bitrouter.yaml routing/policy changes that cut cost
  without dropping capability. Use when the user asks to optimize costs,
  tune routing, or generate a policy spec from their workload.
tools: Read, Grep, Glob, Bash
memory: project
---
```

Platform notes: plugin agents may not declare `hooks`/`mcpServers` (platform
security rule) — it drives the CLI via Bash instead, which is what we want
anyway. Claude Code-only at first; Codex has no plugin-subagent concept in
current docs.

## 6. UX flows (concrete)

**Persona A — cold install, Claude Code:**

```text
› /plugin marketplace add bitrouter/bitrouter
› /plugin install bitrouter
  [SessionStart next session]: "BitRouter CLI not installed — the bitrouter skill can set it up."
› set up bitrouter
  [skill] Local or Cloud? … → local
  [skill] runs: brew install bitrouter/tap/bitrouter && bitrouter start
  [skill] verifies: bitrouter status → green, 3 providers detected from env
  [skill] "Routing is live at :4356. This session is still on its original
          backend — exit and relaunch with `bitrouter spawn -a claude`, or I
          can show you the durable env override (diff shown, you confirm)."

  --- first routed session (the aha-moment §5.4 exists for) ---
  [SessionStart]: "BitRouter: routing active — daemon :4356, 5 providers, session routed."
  … normal work …
  [cost-feed]: "session $0.42 · would be $3.10 at frontier list price · saved 86%"
```

On Codex the same aha lands **per turn** instead of live: the `Stop` hook
appends the spend line as each turn completes (R-7), `spawn` prints the
savings summary when the session exits, and the next session's SessionStart
line opens with `last session $0.42 (saved 86%) · lifetime saved $214`.

**Persona B — already wired, daily use:**

```text
  [SessionStart]: "BitRouter: routing active — daemon :4356, 5 providers, session routed."
› generate fixtures for all 30 endpoint schemas
  [Claude] calls mcp: bitrouter.complete (model: deepseek/deepseek-v4) for the
           bulk generation, reviews output on the main model
  [cost-feed]: "openai rate-limited at request 141 → failed over to bedrock,
           0 lost. Session spend: $1.87."
› this loop feels expensive — optimize it
  [Claude] launches bitrouter:loop-optimizer → proposes bitrouter.yaml diff
           ("alias summarization hops to deepseek, cap Task spend at $2")
› looks right, apply it
  [Claude] writes bitrouter.yaml → [FileChanged hook] bitrouter reload
  [cost-feed] (next summary): savings delta visible
```

## 7. Security & trust posture

- **No silent config mutation.** Durable rewiring is always
  show-diff-and-confirm; per-process wiring goes through `spawn`, which by
  design never touches the agent's config files.
- **Hooks & monitors are read-only** (status/events). Both platforms prompt
  users to trust hooks — a read-only status line survives that review;
  anything that installs software inside a hook would not.
- **Install actions run in the skill conversation**, where the user sees and
  approves each command (brew/npm/installer), not hidden in lifecycle hooks.
- **MCP server is local-loopback** to the user's own daemon; cloud mode uses
  the user's own `brk_` key from keychain.
- Marketplace distribution is SHA-pinned on both platforms; releases bump
  `version` explicitly (no per-commit churn for installed users).

## 8. Distribution & phasing

**P0 — MVP (Claude Code + Codex):**

1. Prerequisite: finish
   [skills/bitrouter/references/harness-claude-code.md](skills/bitrouter/references/harness-claude-code.md)
   — its TODOs are answerable today from `spawn -a claude`'s implementation
   (`ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN`).
2. `bitrouter status --agent` (hook-grade status line; name per R-4).
3. **Cost-feed events core + P0 renderers** (§5.4): `bitrouter events`
   (`--follow` DB tail per R-9, `--turn` one-shot, aggregation,
   counterfactual line, savings ledger, `--wait-daemon`), the SessionStart
   ledger recap in `status --agent`, the Codex `Stop`-hook turn summary, the
   `spawn` exit summary, and the MCP tool-result cost footer. The largest P0
   work item; everything else is manifests and wording. The CC monitor entry
   rides along at ~10 manifest lines.
4. `.claude-plugin/{plugin,marketplace}.json` (incl. SessionStart +
   FileChanged hooks and the inline cost-feed monitor), `.codex-plugin/*`
   (SessionStart + Stop hooks) as in §5.1.
5. Relocate `skills/verify` → `.claude/skills/verify` and update
   `skills/README.md` (R-2).
6. Skill addendum: plugin-context behavior (MCP enable walk-through on Codex,
   the restart handoff wording, arbitrage nudge, cost-feed interpretation —
   including the "counterfactual is approximate" caveat).
7. CI: `claude plugin validate . --strict`, plus the Codex loader-exercise
   check per R-6 (`codex plugin marketplace add` + `add` + `list --json`
   with component presence-assertions, since Codex load errors are silent).
8. CLAUDE.md: extend the Agent Skill lockstep rule to cover
   `.claude-plugin/` + `.codex-plugin/` (manifests must never describe a CLI
   that doesn't exist).
9. Docs: user-facing install page under `docs/` (with `.zh.md` sibling, per
   contract) — can trail the code by one release.
10. *P0-stretch:* `bitrouter spawn --hud` spike — the universal *live* HUD
    renderer (§5.4), pending the PTY/terminal-title UX validation.

**P1:** loop-optimizer subagent (§5.6) + its gating `bitrouter usage` local
stats surface (scoped by R-9); `userConfig` local/cloud; `StopFailure` marker →
SessionStart pain-point nudge; `SubagentStart/Stop` span attribution feeding
the cost feed; statusline offer; community-marketplace submission (Claude) +
plugin-portal submission (Codex); granular sub-skills if
`/bitrouter:bitrouter` proves awkward.

**P2:** slim distribution (npm `@bitrouter/plugin` for Codex's npm source;
CI-built mirror repo if the 46 MiB clone hurts adoption — note: live testing
confirmed `codex plugin add` **copies the entire plugin root** into its cache,
which on a dev checkout with `target/` was 7.9 GB; fresh clones are fine, but
this raises the priority of a slim source); Antigravity + Grok
Build shims (Antigravity reads the Claude format — likely near-free);
`Setup`-hook CI/headless story; `SessionEnd` opt-in eval-loop feed;
`PreToolUse(Task)` subagent-model-rewrite spike (feasible on both platforms
per R-8; gated on a consent story, not capability).

## 9. Resolutions (formerly open questions)

All nine OQs resolved 2026-07-10 — Codex answers from the `openai/codex`
source (`codex-rs/`), official docs, and a live `codex-cli 0.144.0` install;
Claude Code answers from official docs; BitRouter answers from this repo.

- **R-1 (Codex layout) — resolved: `.codex-plugin/` interior paths work.**
  The single path validator (`resolve_manifest_path`,
  `codex-rs/core-plugins/src/manifest.rs`) enforces only: starts with `./`,
  no `..`, not absolute, resolves inside plugin root. No rule excludes
  `.codex-plugin/` — `"mcpServers": "./.codex-plugin/mcp.json"` and
  `"hooks": "./.codex-plugin/hooks.json"` load fine (docs *recommend*
  root-level layout, but it's convention, not validation). So the §4 layout
  stands and the repo root stays clean — no top-level `.mcp.json` collision.
  Two bonuses: (a) Codex's manifest discovery falls back to
  `.claude-plugin/plugin.json` (`DISCOVERABLE_PLUGIN_MANIFEST_PATHS` in
  `codex-rs/utils/plugins/src/plugin_namespace.rs`) — the ecosystems
  interop deliberately; we still ship a dedicated `.codex-plugin/` manifest
  for explicit control, but the fallback is insurance. (b) Codex plugin hooks
  export `CLAUDE_PLUGIN_ROOT`/`CLAUDE_PLUGIN_DATA` compat aliases. One
  caveat: invalid manifest paths are **warn-and-ignore**, not load failures —
  a typo silently drops a component (see R-6).
- **R-2 (skill hygiene) — resolved: relocate `skills/verify` →
  `.claude/skills/verify`.** Referenced only by `skills/README.md`; `.claude/`
  has no skills dir yet. Wins: (a) plugin payload dir becomes purely
  shippable — no leakage under `--plugin-dir .` (Claude) or a `./skills/`
  scan (Codex); (b) the `npx skills add bitrouter/bitrouter` rail stops
  surfacing a dev-only skill; (c) contributors get `/verify` auto-loaded in
  project scope, which bare `skills/verify` never did. Matches the
  convention `bitrouter skills add` itself uses (`.claude/skills/` install
  target). Action item in P0; update `skills/README.md` in the same change.
- **R-3 (Codex skill scan) — resolved: point at the skill dir, get exactly
  one skill.** Each `skills` entry is a root recursively scanned for files
  literally named `SKILL.md` (depth ≤ 6, hidden dirs pruned —
  `codex-rs/core-skills/src/loader/discovery.rs`). `"skills":
  "./skills/bitrouter"` → exactly our skill; `"./skills/"` would include
  `verify` too (moot after R-2). Standing rule for skill authors:
  **never name any file under `skills/bitrouter/references/` `SKILL.md`** —
  it would load as a second skill on Codex.
- **R-4 (CLI naming) — resolved:** `bitrouter status --agent` (third output
  mode next to `--json`/`--human`: one agent-context line, sub-100 ms, always
  exit 0 — fits the "agent-native first" output contract in CLI.md);
  `bitrouter usage` (local mirror of the existing `bitrouter cloud usage` —
  perfect symmetry); `bitrouter events` (new verb; `--follow` for the
  monitor, `--turn`/`--since` for per-turn hook queries). All three land in
  the skill's lockstep scope on day one.
- **R-5 (skill naming) — resolved: accept `/bitrouter:bitrouter` for MVP.**
  The skill is primarily model-invoked via its description; explicit slash
  invocation is rare. Split into `/bitrouter:setup` / `:diagnose` wrappers
  only if P1 usage shows confusion — each wrapper is another lockstep
  surface, so don't pre-pay.
- **R-6 (Codex validation) — resolved: no validator exists** (`codex plugin`
  has only `add`/`list`/`marketplace`/`remove` as of 0.144.0), and manifest
  errors are warn-and-ignore. CI substitute: exercise the real loader —
  `codex plugin marketplace add <repo> && codex plugin add bitrouter &&
  codex plugin list --json`, then **assert the skill/MCP/hooks components
  actually appear** (presence-assertion, because load errors are silent).
  Claude side keeps `claude plugin validate . --strict`.
- **R-7 (Codex hook catalog) — resolved: 10 events, near-Claude-parity
  format.** `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
  `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`,
  `SubagentStart`, `SubagentStop`, `Stop` (schemas in
  `codex-rs/hooks/schema/generated/`). Same `hooks.json` shape as Claude,
  JSON on stdin, outputs include `additionalContext`, `systemMessage`, and
  `updatedInput` (PreToolUse). **Design consequence — Codex gets a per-turn
  cost renderer:** `Stop` fires at every turn end (no usage data in its
  payload, but the hook can run `bitrouter events --turn` against the local
  DB and return the spend line). §5.4 updated. No `SessionEnd`/`FileChanged`/
  monitor equivalents; `SubagentStart/Stop` parity means span attribution
  (P1) works on both. Codex's `notify` config exists but emits only
  `agent-turn-complete` with no usage fields — superseded by the `Stop` hook
  for our purposes.
- **R-8 (PreToolUse input rewrite) — resolved: supported on BOTH platforms.**
  Claude Code: `hookSpecificOutput.updatedInput` replaces tool arguments
  before execution (official hooks doc). Codex: `updatedInput` in the
  PreToolUse output schema. The P2 subagent-model-downgrade spike is
  *possible* on both; remains P2 on product grounds (silently rewriting
  spawns needs a consent story), not feasibility.
- **R-9 (local usage stats) — resolved: the data already exists,
  unconditionally.** Every settled request — success or failure — is written
  to the `requests` table (SQLite `bitrouter.db` by default, sea-orm, any
  backend) with `model_id`, `provider_id`, token counts,
  `estimated_charge_micro_usd`, `latency_ms`, `error`, `created_at`
  (`apps/bitrouter/src/metering/{recorder,store,db}.rs`; not gated by any
  config). Consequences: **`bitrouter usage` is a pure local DB read** (new
  group-by aggregates on `MeteringStore`; no daemon, no schema change), and
  **`bitrouter events` v1 is a throttled DB tail** (`created_at` cursor) —
  no daemon protocol change; the control socket is strictly one-shot today,
  so live push (broadcast channel + streaming socket command) is a later
  upgrade, not a v1 requirement. Counterfactual pricing's natural home:
  a second lookup table beside `PricingTable` in
  `apps/bitrouter/src/metering/pricing.rs`, sourced from `registry/` list
  prices. Attribution limits: rows carry `user_id`/`api_key_id` only — no
  session/agent id — so per-agent grouping today means one `brvk_` key per
  agent; threading ACP session ids into settlement is a cross-crate P2.
  `plugins/bitrouter-observe` is push-only (OTLP/Prometheus, no cost metric)
  — not a query surface; ignore it for this feature.
