---
title: Server tools
description: Let BitRouter run the tool-calling loop for you, server-side — including the Advisor, SubAgent, and Fusion model-backed tools.
sourceHash: 86118153c9642705e3357ef78f6a501633c2bf90c8adea85ececbd994d4b4e06
---

Normally your agent runs the tool-calling loop: the model asks to call a tool, your harness executes it, appends the result, and calls the model again. **Server tools** move that loop into BitRouter. You declare a set of tools, BitRouter advertises them to the model, and when the model calls one, BitRouter **executes it and feeds the result back itself** — looping until the model stops calling them. To the caller it looks like a single response.

## How the loop runs

BitRouter injects the declared tools into the outbound request, intercepts the model's calls to them, runs them, appends the results, and re-calls the upstream — repeating until the model returns an answer with no tool calls, or a bound is hit. The loop is bounded so it can't run away:

| Bound | Default | Meaning |
| --- | --- | --- |
| `max_iterations` | 10 | Maximum tool rounds before the loop stops. |
| `tool_timeout` | 30s | Per-tool execution timeout. |
| `total_budget` | 120s | Wall-clock budget for the whole loop. |
| `max_consecutive_errors` | 3 | Stop after this many back-to-back tool failures. |

Before each call, an **approval policy** decides whether the tool may run. The default allows everything; a denied call returns an execution-denied result to the model instead of running.

## Enabling server tools per request

You turn server tools on by declaring them in the request's `tools` array — no config change required. BitRouter recognizes three built-in, model-backed tools and only advertises the ones you declare:

```json
{
  "tools": [
    { "type": "bitrouter:advisor",  "args": { "model": "anthropic/claude-opus-4.8", "instructions": "..." } },
    { "type": "bitrouter:subagent", "args": { "model": "openai/gpt-4o-mini", "instructions": "..." } },
    { "type": "bitrouter:fusion",   "args": { "panel": [{ "model": "..." }], "judge": { "model": "..." } } }
  ]
}
```

A fourth built-in, [**Web search**](/docs/features/websearch), works the same way — declare `bitrouter:web_search` to give the model a search backed by a key you bring.

MCP-server tools are wired through configuration instead — set `server_tools.mcp_servers` to the servers whose tools BitRouter should run inside the loop.

## Model-backed tools

Three of the built-ins wrap a **nested model call** instead of an external side effect. Each gets its own page:

- [**Advisor**](/docs/features/advisor) — the running model consults a stronger model on one hard sub-question mid-generation, without escalating the whole request.
- [**Sub-agent**](/docs/features/subagent) — the model delegates a self-contained task to a cheaper, faster worker that runs in isolation and returns only its result.
- [**Fusion**](/docs/features/fusion) — a panel of models answers in parallel, a judge compares (not merges) their answers, and the calling model writes the final reply from that analysis.

<Callout type="info">
Advisor, Sub-agent, and Fusion are each backed by model calls nested inside your request. They cost what their underlying model calls cost, and they appear in your usage history like any other call.
</Callout>

## How tools are bundled: toolsets

A **toolset** is the seam that makes router-run tools provider-agnostic. Each toolset answers two questions for a request — **which tools to advertise**, and **how to run a call** to one of them — and BitRouter composes several into the single set the model sees.

BitRouter keeps a registry of toolsets; when the model calls a tool, the registry routes the call to the toolset that **owns** that name. Because several toolsets can be active at once, tool names are **prefixed** to avoid collisions — the `search` tool from the `demo` server is advertised as `demo__search`.

Toolsets come in three kinds:

- **MCP-backed** — one per upstream MCP server; its tools are the server's tools, run through the gateway.
- **Model-backed** — Advisor, Sub-agent, and Fusion, each wrapping a nested model call.
- **In-process** — tools implemented directly in the router.

A toolset doesn't have to advertise on every request. It can check what the caller **declared** and stay silent otherwise — which is how the model-backed tools work: Advisor only appears when the request declares `bitrouter:advisor`, and likewise for Sub-agent and Fusion. The model only ever sees the tools that request actually opted into.
