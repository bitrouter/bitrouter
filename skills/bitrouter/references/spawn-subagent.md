# Agent-driven subagent budgets — `spawn_subagent`

A top-level agent (e.g. opencode), routed through the local daemon, can spawn a
**budget-capped subagent** at runtime by calling the router-owned
**`spawn_subagent`** tool. BitRouter executes it server-side (via the
server-tool loop): it mints a scoped `brvk_` bound to a fresh `Budget` policy,
spawns a headless `opencode acp` worker pinned to that key + chosen model,
meters every worker inference, and **fails the worker closed at the cap**. The
parent gets a structured result back.

This needs **no MCP servers** and no pre-start delegation — the agent chooses the
model and budget mid-conversation.

## Enable it (daemon config)

```yaml
server:
  skip_auth: false          # brvk_ keys + Budget policies must be enforced

server_tools:
  spawn_subagent:
    base_url: "http://127.0.0.1:4356/v1"      # THIS daemon — so worker calls are metered here
    harnesses: ["opencode", "claude-acp"]     # operator allowlist; first entry is the default
    models:                                    # model allowlist; a call naming another model is rejected
      - "bitrouter/z-ai/glm-5.1"
```

- **Harness selection is operator-controlled** — the model does NOT choose the binary. The first entry in `harnesses` is always used (the model picks `model` + `budget`, not `harness`).
- **`claude-acp`** is env-pinned (`ANTHROPIC_BASE_URL` = daemon base without `/v1`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL`). It reaches the daemon over the Anthropic wire (`/v1/messages`), so `base_url` must NOT have a trailing `/v1` — `ClaudeAcpHarness` strips it automatically.

The parent agent must also route through this daemon (point its provider
`baseURL` at `http://127.0.0.1:4356/v1`) so the loop can inject the tool.

## The tool the agent calls

```
spawn_subagent({
  model: "bitrouter/z-ai/glm-5.1",   // must be in the configured allowlist
  budget_micro_usd: 500000,          // hard cap (µ$); worker denied once spend >= cap
  task: "…use ABSOLUTE paths…",      // the worker prompt
  allowed_tools: ["…"]               // optional: scope the worker's tools (Policy.allowed_tools)
})
```

Returns JSON: `{ final_message, files_touched, spend_micro_usd, budget_micro_usd,
stop_reason, capped }`.

## How it works (one machine, no upstream key to the child)

1. Validate the model (allowlist) and budget (> 0).
2. Register a runtime `Budget` policy (`max_spend_micro_usd = budget`, model +
   tools pinned) — in-memory, no restart.
3. Mint one `brvk_` bound to that policy.
4. Generate a temp `opencode.json` (`OPENCODE_CONFIG`) pinning the model +
   provider `baseURL` (this daemon) + `apiKey` (the `brvk_`), with a wide-open
   permission ruleset so the worker runs non-interactively. The worker runs in
   an isolated temp `--cwd` (removed on completion).
5. Drive `opencode acp` over ACP (`initialize → session/new → session/prompt`),
   collecting the final message + file edits + stop reason.
6. Read spend via metering; return the structured result. Mid-run, each worker
   inference hits `/v1/messages` under the `brvk_`; `PolicyHook` denies
   (`Forbidden`) at the cap — fail-closed with no extra code.

## Gotchas

- **Metering needs STREAMING usage from the upstream.** The cap only accrues if
  the chosen model's upstream reports streaming token usage. On a `usage{0,0}`
  stream the charge stays 0 and the cap never trips. Pick a provider/model that
  reports streaming usage, and give it a **pricing block** in config (no pricing
  → `estimated_charge` 0 → cap never trips).
- **The cap window is monthly** (`TimeWindow::ThisMonth`) — it still fires
  correctly for a freshly minted key (zero prior spend).
- **opencode honors `--cwd` + absolute paths;** the task prompt should address
  files by absolute path under the worker's cwd.
- **Demo:** `examples/subagent-demo/` (`bitrouter.demo.yaml`,
  `opencode.parent.json`, `run_demo.sh`). Budget-deny verification procedure:
  `apps/bitrouter/tests/subagent_budget_e2e.rs` (run with `-- --ignored`).
