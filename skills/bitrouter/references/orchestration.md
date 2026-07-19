# Orchestrating a subagent fleet

How an orchestrating agent (you) delegates work to BitRouter-managed,
worktree-isolated ACP subagents — and when not to.

## The fabric

```
you (orchestrator, native harness)
 └─ MCP: bitrouter mcp serve --backend fleet     ← stdio subprocess
     ├─ subagent codex-acp   (.bitrouter/worktrees/codex-acp-<record16>)
     ├─ subagent claude-acp  (.bitrouter/worktrees/claude-acp-<record16>)
     └─ …
```

Wire the bridge into your harness's MCP config as a **stdio** server:

```json
{ "mcpServers": { "bitrouter-fleet": {
    "command": "bitrouter", "args": ["mcp", "serve", "--backend", "fleet"] } } }
```

It is stdio-only by design — the tools mutate (spawn processes, write the
repo), so they inherit your process identity instead of an unauthenticated
HTTP path. Run `bitrouter serve` alongside so subagents' LLM calls route
through the proxy.

The `fleet` profile is the **union**: the fleet tools below **plus** the
completion tools (`complete` / `list_models` / `status`, routed to the local
daemon) **plus** `fleet_cost`. So the one bridge lets you both delegate and run
your own completions / check spend without a second MCP server.

Under `bitrouter tui`, the bridge is injected alongside two **gateway
servers** — `bitrouter_tools` (streamable HTTP to the daemon's aggregate
`/mcp`, fanning out to every `mcp_servers` upstream with `{server}__` tool
prefixes) and `bitrouter_skills` (stdio `mcp serve --backend skills`, the
`skills_search`/`skills_get` pair) — and every spawned subagent receives the
same two in its `session/new`, so the fleet shares your tool and skill
surface. Use `skills_get` to paste a skill into a `spawn_subagent` task.

## Tools

| Tool | Effect |
|---|---|
| `spawn_subagent(agent, task, worktree?, result_schema?)` | Launch an ACP subagent on an isolated worktree + branch (`bitrouter/<agent>-<record16>`, based on the repo's HEAD at spawn) and send `task`. **Non-blocking**: returns at once with `{handle, agent, state:"working", worktree, branch, port, note}` — the turn runs in the background. **Poll `subagent_status(handle)`** for `stop_reason`, `reply`, `diff_stat` (plus `result`/`schema_ok` when `result_schema`, a JSON Schema object, was given — one repair re-prompt on invalid output, then `schema_ok:false`); `state` becomes `completed` when the turn ends. **Don't re-spawn on a slow call** — the subagent is already running; a second spawn just duplicates it. `worktree:false` opts out for read-only investigation. **Rejected at capacity** — see the cap below. |
| `prompt_subagent(handle, text)` | Follow-up prompt (e.g. review feedback) to a subagent whose previous turn has finished; same non-blocking model — returns `{handle, state:"working"}`, then poll `subagent_status`. **Refused while the subagent is still `working`** (one turn per session). |
| `subagent_status(handle?)` | One agent or the whole fleet: state (`working`/`completed`/`failed`), worktree, branch, diff stat — **and, once `completed`, the turn's `reply`, `stop_reason`, and `result`/`schema_ok`**. This is the poll surface for `spawn_subagent`/`prompt_subagent`. |
| `subagent_diff(handle)` | Full diff vs the spawn base (committed + uncommitted; untracked files listed; truncated at 64 KiB). |
| `apply_subagent(handle)` | Apply the diff onto the base working tree **uncommitted**. **Human-gated** — see below. |
| `merge_subagent(handle)` | Merge the branch, keeping history; requires the subagent to have committed (clean worktree). Serialized: one integration at a time. **Human-gated.** |
| `close_subagent(handle)` | Shut the subagent down. Its worktree is **retained** (cleanup is gated on merged-or-discarded, never automatic). |
| `fleet_cost()` | BitRouter spend snapshot from the local metering database (machine-wide, not per-session): today's spend + request count and all-time totals. When the bridge was started with `--budget-usd`, also carries `budget` (`budget_usd`, `remaining_usd`, `over_budget`) so you can self-pace. Keeps in-session model arbitrage cost-visible. |

The `fleet` profile also carries **read-only introspection** and **human
escalation** tools:

| Tool | Effect |
|---|---|
| `route_preview(model, prompt?)` | Preview how BitRouter would route `model` (with the opening `prompt`, if given): the resolved provider fallback chain and the registry's per-token rates for the top hop. Resolves through the **live daemon first** (like `bitrouter route`) so it reflects `reload`s and subscription-backed providers, falling back to static config when the daemon is unreachable — `resolved_via` (`live daemon`/`config`) records which. When config-resolved it also reports the static policy decision. Nothing is sent upstream. Use it to pick a model/tier before delegating. |
| `notify_human(message)` | Post a one-line notice in the human's TUI. Headless (no TUI), it returns `{"delivered": false}` — no error. |
| `request_attach(handle)` | Ask the human to attach to a subagent's pane and drive it. The subagent is flagged for attention in the rail. |
| `request_review(handle)` | Flag a subagent's work into the human's review queue. **Advisory when bridge-mirrored:** a subagent surfaced into the TUI over the fleet socket has no review-queue metadata there, so the queue's load-diff/merge/apply verbs no-op on it — the human drives integration from the owning process (this bridge's `merge_subagent`/`apply_subagent`, or the orchestrator). |

## Rules of engagement

- **Delegate, don't relay.** Spawn for tasks that are isolated and
  non-overlapping (a refactor in one crate, a test-fix sweep, a doc pass).
  Do small or tightly-coupled edits yourself — coding parallelizes worse
  than research, and a disciplined review gate beats a fancy scheduler.
- **Phrase tasks with boundaries and an output contract.** Name the files or
  subsystem in scope, say what "done" means, and — when you need structured
  data back — pass `result_schema` instead of parsing prose.
- **Depth 1.** Subagents do not spawn subagents.
- **Mind the cap.** At most **6** subagents run concurrently per bridge. A
  `spawn_subagent` past the cap is **rejected** with an actionable message —
  `merge_subagent`/`apply_subagent`/`close_subagent` one before spawning more.
  A healthy fleet is ~2–6; a disciplined review gate beats fanning out wide.
- **Mind the budget, when set.** If the bridge was started with `--budget-usd`,
  `spawn_subagent`/`prompt_subagent` are **refused** once today's machine-wide
  spend reaches the ceiling (the message names the ceiling and current spend).
  Watch `fleet_cost().budget.remaining_usd` and pace yourself — pick cheaper
  models via `route_preview`, or ask the human to raise `--budget-usd` / wait
  for a fresh window (it resets on a new UTC day). The ceiling is machine-wide,
  not per-session, so other spend today counts against it.
- **Review before integrating.** Read `subagent_diff` (or have the human
  review in `bitrouter tui`). Rejection loop: `prompt_subagent` with your
  feedback — the subagent addresses it in the same worktree.
- **Writes are human-gated by default.** `apply_subagent`/`merge_subagent`
  refuse unless the human started the bridge with `--allow-writes`. Without
  the grant, *request* integration: tell the human which handle is ready and
  let them merge from the `bitrouter tui` review queue (or rerun the bridge
  with the grant). Never ask for the grant on the human's behalf.
- **Permissions are auto-policied, escalating to the human when possible.**
  A subagent's reversible, in-worktree actions (reads, searches, edits under
  the repo) auto-allow. Higher-risk actions (deletes, command execution,
  network access, out-of-tree writes) **escalate to the human's decision
  queue** when the bridge runs under `bitrouter tui` — the tool call waits
  for their y/a/n — and are **denied** when the bridge is headless (no human
  in the loop). Every decision is logged to stderr. Under the TUI your
  subagents also appear in its rail as monitor panes.
  *(Forward-compat: when the connecting client declares the MCP Tasks /
  elicitation capability, a gated permission can instead route back to the
  orchestrator conversation as an `elicitation/create`. No shipping harness
  declares it yet, so this branch is off by default and the human-queue / deny
  fallback above is the guaranteed path.)*
- **Worktree hygiene.** Each subagent gets a `PORT` from `worktrees.ports`
  (default 3100–3199; leases are shared with the TUI's fleet, so ports never
  collide across the two). The `worktrees.bootstrap` hook (config-declared;
  it executes shell) runs in each fresh worktree — under `bitrouter tui`
  only after the human's first-use approval there (a skipped hook is noted
  in the spawn summary as `bootstrap`); headless, wiring the bridge is the
  standing grant. Closed subagents leave their worktrees behind — tell the
  human what is merged and what can be discarded (`git worktree remove`).

## Headless one-shots (no bridge)

For a fire-and-forget subagent without the MCP bridge:
`bitrouter spawn <agent> -p "<task>" [--worktree NAME] [--result-schema JSON|@path]`
streams NDJSON and exits — see `references/cli.md` (NDJSON format + result
contract).
