---
title: Web search
description: A built-in web_search server tool — give any model routed through BitRouter a web search, served by a search backend you bring keys for.
sourceHash: 08f1d4d084a942367630cc5e478ee7731095f7e79683137bc78ce330e1448cc8
---

`web_search` is a built-in [server tool](/docs/features/server-tools): BitRouter runs the search itself, inside the tool-calling loop, and feeds the results back to the model. So **any model routed through BitRouter gains a web search** — even one with no native search of its own — and the engine behind it is one you bring a key for.

It's the same family as Advisor, SubAgent, and Fusion: off by default, enabled in config, and advertised to the model only on requests that declare it.

## How it works

A caller turns it on for a request by declaring it in the `tools` array:

```json
{
  "tools": [
    { "type": "bitrouter:web_search", "args": { "backend": "exa", "max_results": 3 } }
  ]
}
```

`args` is optional — drop it to use the default backend and cap. From there the model calls `web_search` with a `query`; BitRouter runs it against a backend and returns a stable result shape:

```json
{
  "backend": "exa",
  "answer": "…",
  "results": [
    { "url": "…", "title": "…", "snippet": "…", "content": "…", "published": "…", "score": 0.9 }
  ]
}
```

Every per-result field except `url` is optional — no engine fills them all, so the contract is additive. `answer` is present only for the answer-engine `native` backend; the REST engines (Parallel, Exa, Firecrawl, Tavily) return `results` and no `answer`. The loop bounds (`max_iterations`, `tool_timeout`, …) and approval policy from [server tools](/docs/features/server-tools#how-the-loop-runs) apply unchanged.

## Enabling it

Declare the backends under `server_tools.web_search`. The list is a **preference and failover order** — the first backend whose key resolves is the default, and a failing backend falls over to the next:

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    max_results: 5             # optional default cap; a caller may lower it per request
    backends:                  # preference + failover order
      - kind: parallel         # HTTP · key from api_key or PARALLEL_API_KEY
      - kind: exa              # HTTP · key from api_key or EXA_API_KEY
      - kind: firecrawl        # HTTP · key from api_key or FIRECRAWL_API_KEY
      - kind: tavily           # HTTP · key from api_key or TAVILY_API_KEY
      - kind: native           # reuse a provider's native search for every model
        name: native           # backend id a caller pins with `backend`
        model: anthropic/claude-opus-4.8
        tool: { type: "anthropic:web_search_20250305" }
```

<Callout type="info">
**No key, no backend.** Each HTTP backend resolves its key from an explicit `api_key` (which supports `${VAR}`) or the conventional `*_API_KEY` env var; a backend with no resolvable key is silently skipped. If *nothing* resolves, BitRouter logs that `web_search` was configured but no backend came up — usually a missing key.
</Callout>

## Backends

| `kind` | Type | Key | Returns |
| --- | --- | --- | --- |
| `parallel` | HTTP (BYOK) | `PARALLEL_API_KEY` | `results[]` |
| `exa` | HTTP (BYOK) | `EXA_API_KEY` | `results[]` |
| `firecrawl` | HTTP (BYOK) | `FIRECRAWL_API_KEY` | `results[]` |
| `tavily` | HTTP (BYOK) | `TAVILY_API_KEY` | `results[]` |
| `native` | Nested completion | a routable `model` + its native search `tool` | `answer` |

- **HTTP backends** (`parallel` / `exa` / `firecrawl` / `tavily`) call the engine's REST API directly. Each takes an optional `api_key` and `api_base` override; responses are parsed defensively, so a provider renaming a field degrades to a missing value rather than a hard error.
- **The native backend** runs a nested model completion, so it needs a routable model behind a provider key. It forwards a provider's *own* search tool (e.g. Anthropic's `web_search_20250305`), making one provider's native web search usable from any model — including those that don't have one.

## Pinning a backend per request

A caller overrides the default by naming a backend in the declaration's `args.backend` (matched against each backend's `kind`, or the `name` you set on `native`), and can lower the result cap with `args.max_results`:

```json
{ "type": "bitrouter:web_search", "args": { "backend": "tavily", "max_results": 8 } }
```

## Learn more

- [Server tools](/docs/features/server-tools) — the loop that runs `web_search`, plus Advisor, SubAgent, and Fusion.
- [Toolsets](/docs/features/server-tools#how-tools-are-bundled-toolsets) — how tools advertised on a request are bundled and namespaced.
- [OpenTelemetry](/docs/features/opentelemetry) — every nested search call shows up in your traces like any other.
