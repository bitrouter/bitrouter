# Design: BitRouter as Claude Code & Codex agent plugins

> **Status:** working draft for review — not product docs (deliberately outside
> `docs/`, so no `.zh.md` sibling is required yet).
>
> **Terminology:** this doc says **agent plugin** for the Claude Code / Codex
> installable package, to avoid collision with the repo's existing Rust router
> plugins under `plugins/` (guardrails, observe, attestation), which are
> unrelated.

> **As-built shape (2026-07-10, after three trimming passes — read this first).**
> The doc below records the full exploration; the design converged well inside
> it. The shipped plugin carries exactly **two components: skills + MCP** —
> nothing else. Cut along the way, each for the same reason (a component that
> can't behave identically on every harness fragments the "one manager across
> harnesses" value prop):
> 1. **`experimental.monitors` live cost feed** — CC-only, can't generalize.
> 2. **`bitrouter events` command + per-turn `Stop` hook** — per-turn can't be
>    made uniform (CC `Stop` can't surface a line without forcing a model turn).
> 3. **All hooks** (`SessionStart` recap, `FileChanged` reload) + the
>    `status --agent` CLI surface — hooks are the *least* portable component
>    (Grok block-only; Antigravity different catalog + output schema; even
>    `SessionStart` output-surfacing varies), so anchoring on a hook fragments
>    the thing we're unifying.
>
> What remains: **skill + origin MCP server** (both port to all four harnesses),
> plus two non-plugin `bitrouter` CLI cost signals — the **MCP tool-result
> footer** and the **`spawn` exit summary**. Everything ambient/live/per-turn
> is deferred to BitRouter's own **manager surface** (`spawn --hud` → TUI/GUI
> cost-HUD), harness-agnostic by construction. Sections that still describe
> hooks/monitors/events as shipping are the exploration record — trust this
> banner, §5.4, and the manifests for the final shape.

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
| 3 | ~~Ambient hooks~~ | **Dropped** (as-built banner): hooks are the least portable component (Grok block-only, Antigravity different catalog, `SessionStart` surfacing varies), so a hook-anchored plugin fragments what BitRouter unifies. Research map kept in §5.2 for the future manager surface | Cut |
| 4 | **Cost surface** (non-hook renderers) | Spend shown on the user's own workload via the MCP footer + `spawn` exit — no ambient hook. Live/per-turn/ambient cost is deliberately *not* in the plugin (can't generalize); it's the manager surface's job. §5.4 | **P0** |
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
│   ├── plugin.json               # CC manifest (skills override + inline MCP; no hooks)
│   └── marketplace.json          # repo doubles as a CC marketplace
├── .codex-plugin/
│   ├── plugin.json               # Codex manifest (skills + MCP)
│   └── mcp.json                  # Codex MCP config (interior paths confirmed valid — R-1)
├── .agents/plugins/
│   └── marketplace.json          # Codex marketplace catalog (source: ".")
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
- **Skill-scan hygiene:** the CC `skills` manifest field normally *adds* to
  the default `skills/` scan, but for a marketplace entry whose source
  resolves to the marketplace root, declaring explicit subdirectories
  **replaces** the scan — so `"skills": ["./skills/bitrouter"]` scopes the
  install to exactly the one shippable skill. (`skills/` used to also hold a
  dev-only `verify` skill; it was removed outright in a separate PR — see the
  R-2 update below — leaving `skills/` as purely shippable payload.)

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
  "mcpServers": {
    "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
  }
}
```

(As-built: **skills + mcpServers only** — no `hooks`, no `experimental.monitors`.
The earlier drafts of this snippet carried a `SessionStart` status hook and a
`FileChanged` reload hook; both were dropped because hooks don't port across
harnesses — see the as-built banner at the top and §5.2.)

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
  "mcpServers": "./.codex-plugin/mcp.json"
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

### 5.2 Hooks — full utilization map (exploration record; the plugin ships NO hooks)

> **As-built: the plugin ships zero hooks.** This section is the research
> record — it's why we know what hooks *could* do, kept for the future manager
> surface and P2 hook ideas. The decision to drop all hooks from the plugin is
> in the top-of-doc banner: cross-harness hook research showed hooks are the
> least portable component (Grok block-only; Antigravity 5-event catalog with a
> different output schema; even `SessionStart` output-surfacing varies — CC
> injects stdout, Grok discards it, Antigravity CLI lacks the event), so a
> hook-anchored plugin fragments the thing BitRouter unifies. The `P0` verdicts
> in the table below are historical.

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
| `Stop` (per-turn cost summary) | **Rejected on both** (revised §5.4). CC's `Stop` can't surface a line without forcing an extra model turn (`additionalContext` continues the conversation; plain stdout is discarded); Codex's `Stop` *can* (clean `systemMessage`), but a Codex-only per-turn line reintroduces the cross-harness asymmetry we're removing. Per-turn/live is the manager surface's job | Killed |
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

…plus, when the metering DB has data (§5.4), the line appends the spend
recap — `Spend today $X (N requests), $Y this month` — the **universal
in-band cost anchor** (SessionStart fires on both CC and Codex). And when the
P1 `StopFailure` marker is present, one extra line noting the prior session's
API-error death and that failover would have survived it.

**Codex parity (resolved, R-7):** Codex plugin hooks expose 10 events —
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`,
`PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`,
`Stop` — with the same `hooks.json` shape as Claude Code, JSON on stdin, and
even `CLAUDE_PLUGIN_ROOT`/`CLAUDE_PLUGIN_DATA` compat env aliases. Codex ships
**SessionStart** (status line + recap) to match CC; per-turn `Stop` is *not*
used (revised §5.4 — keeping the plugin surface uniform). The
`SubagentStart/Stop` span-attribution idea (P1) belongs to the manager HUD,
not the plugin. What Codex lacks vs CC: `FileChanged` (auto-reload is
CC-only), `StopFailure`, `SessionEnd`. Codex hooks are untrusted until the
user reviews them — our read-only `bitrouter` one-liners survive that review.

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

### 5.4 Cost surface — uniform in the plugin, live in the manager

> **Revision (2026-07-10, supersedes the earlier "one events core, N
> renderers / CC live monitor" design).** The prior version made a live
> in-context cost stream a P0 renderer, delivered on Claude Code via
> `experimental.monitors`. Two facts killed that as a *plugin* feature:
> (1) it can **never** generalize — Codex has no monitor mechanism and none
> is planned (verified: manifest has no monitor field, hooks are strictly
> one-shot, exhaustive issue/discussion sweep); (2) Claude Code's `Stop`
> hook can't surface a per-turn line either — plain stdout is discarded and
> the only injection path (`additionalContext`) *forces an extra model
> turn*. A permanently one-harness live feed contradicts BitRouter's core
> value prop — **one manager across harnesses**. So live/per-turn cost moves
> out of the plugin entirely and into BitRouter's own manager surface (below,
> §8 P1/P2); the plugin keeps only renderers that are **identical on every
> harness**.

Routing is invisible when it works, so the cost surface exists to *show* the
win on the user's own workload. But it must show it the same way everywhere —
a feature that's rich on one harness and absent on another teaches exactly the
fragmentation BitRouter exists to erase. So the plugin surfaces cost at
**boundaries and on-demand** (uniform), and the *manager* owns **live**
(where "single manager across harnesses" is actually expressed).

**Plugin/CLI cost renderers — as-built (two, both non-hook):**

> **Superseded again (2026-07-10, hooks-drop pass):** the earlier version of
> this table listed a **SessionStart spend recap** as a third renderer. Hooks
> were then dropped entirely (top banner), so the recap is gone. What remains
> are two renderers that need no hook — the MCP footer (part of the MCP
> component) and the `spawn` exit summary (a `spawn` CLI feature, not a plugin
> component). The `status --agent` CLI surface that backed the recap was
> removed with the hooks (no consumer, no-dead-code).

| Renderer | Reaches | Works on | Notes |
|---|---|---|---|
| **MCP tool-result footer** — every origin-server `complete`/`status` result carries a one-line spend footer | agent + user, in-band | **CC + Codex** (any MCP client) | makes in-session arbitrage self-demonstrating |
| **`spawn` exit summary** — session spend printed after the wrapped harness exits | user | **any** harness `spawn` wraps (BitRouter-owned) | trivial, zero risk; not a plugin component |

Both read the metering `requests` table directly (per R-9 — persisted
unconditionally) via a read-only opener; no daemon protocol change, no
streaming, no `events` command, no hook. Each degrades to **silence** on a
missing DB / unroutable session — a cost surface must never break a session.

**Manager renderers — live, NOT in the plugin (deferred):**

| Renderer | Reaches | Works on | Tier |
|---|---|---|---|
| **`spawn --hud` live bar** — PTY scroll-region / terminal-title HUD, updated per request | user, live | **universal** (spawn wraps any harness) | P1 — needs a PTY-interposition spike |
| **`bitrouter top` / TUI-GUI cost-HUD** — the #604 manager's live pane | user, live | **universal**, even non-spawn setups | P1/P2 — the real home for live cross-harness cost |
| Desktop notifications (threshold/failover) | user | universal | P2, opt-in |

These consume a future **`bitrouter events` stream** (a throttled `requests`
DB tail — the design from R-9 stands), (re)introduced *with* its first manager
consumer rather than shipped consumer-less now (repo no-dead-code rule). The
per-turn `Stop`-hook idea and the `experimental.monitors` entry are **removed**
— the former can't be made uniform, the latter can't generalize.

**Rejected (unchanged):** injecting cost text into the LLM response stream
(mutating model output breaks tool-call parsing and trust); Codex `notify`
(only `agent-turn-complete`, no usage fields).

**v1 reports spend, not savings.** The counterfactual "vs frontier list price"
line needs the P1 `bitrouter usage` pricing plumbing, and a savings line on a
single-provider BYOK config would read "$0 saved" — worse than absent. Recap
says *spend today / this month* (per-session attribution isn't in the metering
rows — R-9); the counterfactual-savings headline is the P1 target. All figures
are estimates (`estimated_charge_micro_usd`) — HUD-grade, not invoice-grade.

**What we do not promise:** live in-context cost inside a *natively-launched*
harness session (env-wired, not via `spawn`/TUI). That path bypassed the
manager; live cost is the manager's surface. Boundary recap + MCP footer still
apply there.

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

  --- routed session ends, next one opens ---
  [spawn exit]: "spawn: session spend $0.42 (18 requests) · today $1.10"
  [next SessionStart]: "BitRouter: routing active — daemon :4356, 5 models routable;
           this session is routed. Spend today $1.10 (26 requests), $8.40 this month."
```

The same recap lands identically on Codex (SessionStart + `spawn` exit both
fire there). Live mid-session cost is the manager surface's job (`spawn --hud`
/ TUI), not the plugin — see §5.4.

**Persona B — already wired, daily use:**

```text
  [SessionStart]: "BitRouter: routing active — daemon :4356, 5 models routable;
           this session is routed. Spend today $1.10 (26 requests), $8.40 this month."
› generate fixtures for all 30 endpoint schemas
  [Claude] calls mcp: bitrouter.complete (model: deepseek/deepseek-v4) for the
           bulk generation, reviews output on the main model
  [mcp footer]: "bitrouter: spend today $1.34 (31 requests)"
› this loop feels expensive — optimize it
  [Claude] launches bitrouter:loop-optimizer → proposes bitrouter.yaml diff
           ("alias summarization hops to deepseek, cap Task spend at $2")
› looks right, apply it
  [Claude] writes bitrouter.yaml → [FileChanged hook] bitrouter reload
```

## 7. Security & trust posture

- **No silent config mutation.** Durable rewiring is always
  show-diff-and-confirm; per-process wiring goes through `spawn`, which by
  design never touches the agent's config files.
- **No hooks ship** (as-built), so there's no lifecycle-hook trust surface to
  review at all — one less thing for a security-conscious user to vet.
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
2. **Non-hook cost renderers** (§5.4), reading the metering `requests` table
   via a read-only opener (R-9): the MCP tool-result cost footer and the
   `spawn` exit summary. No `events` command, no monitor, no hook.
3. `.claude-plugin/{plugin,marketplace}.json` and `.codex-plugin/*` —
   **skills + mcpServers only** (as in §5.1). No hooks.
4. Remove the dev-only `skills/verify` skill outright (R-2) — done in a
   separate PR off `main`, not bundled here.
5. Skill addendum: plugin-context behavior (MCP enable walk-through on Codex,
   the restart handoff wording, arbitrage nudge, cost-surface interpretation —
   including the "spend, not savings" and "estimate, not invoice" caveats).
6. CI: `claude plugin validate . --strict`, plus the Codex loader-exercise
   check per R-6 (`codex plugin marketplace add` + `add` + `list --json`
   with component presence-assertions, since Codex load errors are silent).
7. CLAUDE.md: extend the Agent Skill lockstep rule to cover
   `.claude-plugin/` + `.codex-plugin/` (manifests must never describe a CLI
   that doesn't exist).
8. Docs: user-facing install page under `docs/` (with `.zh.md` sibling, per
   contract) — can trail the code by one release.

**P1:** the **manager live cost surface** (§5.4) — `bitrouter events` stream
(the deferred throttled DB tail) + its first consumer, `bitrouter spawn --hud`
(needs the PTY/terminal-title spike); loop-optimizer subagent (§5.6) + its
gating `bitrouter usage` local stats surface (scoped by R-9); the
counterfactual-savings line (needs `usage` pricing); `userConfig` local/cloud;
statusline offer; community-marketplace submission (Claude) + plugin-portal
submission (Codex); granular sub-skills if `/bitrouter:bitrouter` proves
awkward. (Hook-based ideas — `StopFailure` nudge, `FileChanged` reload,
`PreToolUse` model-downgrade — are viable only where each harness supports
them; treat as harness-specific enhancements, not uniform plugin features.)

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

> **Superseded in part by the §5.4 revision (same day):** R-4, R-7, and R-9
> below reference a `bitrouter events` command / Codex `Stop`-hook per-turn
> feed / CC `experimental.monitors`. Those were the design at resolution time;
> the §5.4 revision then pulled live/per-turn cost out of the plugin into the
> manager surface. The `bitrouter events` DB-tail design (R-9) still stands —
> it's just deferred to ship with its first manager consumer, not as a plugin
> renderer. Treat the mechanism findings in R-7/R-9 as accurate; treat the
> "we will ship X in the plugin" framing as revised.

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
- **R-2 (skill hygiene) — resolved: REMOVE the dev-only `verify` skill
  outright** (superseded an earlier "relocate to `.claude/skills/`" plan).
  `skills/` is the source-of-truth tree served verbatim by every install rail
  (`bitrouter skills add`, `npx skills add`) and both plugin manifests, so a
  dev-only skill there blurs the shipped-vs-internal boundary (and its name
  collides with the `bitrouter verify` CLI command). Relocating to
  `.claude/skills/` was considered but rejected — it just moved the clutter;
  the substrate-verification steps belong in `DEVELOPMENT.md` / the code's own
  tests. Done in a **separate PR** (off `main`, not bundled with the plugin
  work); this PR drops its earlier relocation. Net: `skills/` holds exactly
  the shippable `/bitrouter` skill.
- **R-3 (Codex skill scan) — resolved: point at the skill dir, get exactly
  one skill.** Each `skills` entry is a root recursively scanned for files
  literally named `SKILL.md` (depth ≤ 6, hidden dirs pruned —
  `codex-rs/core-skills/src/loader/discovery.rs`). `"skills":
  "./skills/bitrouter"` → exactly our skill (and after R-2 removes `verify`,
  `skills/` holds only the one skill anyway). Standing rule for skill authors:
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
