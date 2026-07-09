---
title: Web fetch
description: A built-in web_fetch server tool — give any model routed through BitRouter a BYOK URL-content fetcher, served by an extraction backend you bring keys for.
---

`web_fetch` is a built-in [server tool](/docs/features/server-tools): BitRouter runs the fetch itself, inside the tool-calling loop, and feeds the extracted page back to the model. So **any model routed through BitRouter gains a URL reader** — hand it a link and it gets clean, normalized page content — and the extraction engine behind it is one you bring a key for.

It's the same family as Advisor, SubAgent, Fusion, and [Web search](/docs/features/websearch): off by default, enabled in config, and advertised to the model only on requests that declare it.

## How it works

A caller turns it on for a request by declaring it in the `tools` array:

```json
{
  "tools": [
    { "type": "bitrouter:web_fetch" }
  ]
}
```

From there the model calls `web_fetch` with a `url`; BitRouter fetches it through a backend and returns a stable result shape:

```json
{
  "status": "ok",
  "backend": "exa",
  "url": "https://example.com/article",
  "title": "…",
  "content": "…",
  "published": "…"
}
```

`title` and `published` are optional — no engine fills them all, so the contract is additive. The loop bounds (`max_iterations`, `tool_timeout`, …) and approval policy from [server tools](/docs/features/server-tools#how-the-loop-runs) apply unchanged.

## Enabling it

Declare the backends under `server_tools.web_fetch`. The list is a **preference and failover order** — the first backend whose key resolves is the default, and a failing (or empty) extraction falls over to the next:

```yaml
# bitrouter.yaml
server_tools:
  web_fetch:
    max_content_tokens: 4000   # optional default cap; a declaration or call may only lower it
    backends:                  # preference + failover order
      - kind: exa              # HTTP · /contents · key from api_key or EXA_API_KEY
      - kind: firecrawl        # HTTP · /v2/scrape · key from api_key or FIRECRAWL_API_KEY
      - kind: tavily           # HTTP · /extract · key from api_key or TAVILY_API_KEY
```

<Callout type="info">
**No key, no backend.** Each backend resolves its key from an explicit `api_key` (which supports `${VAR}`) or the conventional `*_API_KEY` env var; a backend with no resolvable key is silently skipped. If *nothing* resolves, BitRouter logs that `web_fetch` was configured but no backend came up — usually a missing key.
</Callout>

## Backends

| `kind` | Type | Endpoint | Key |
| --- | --- | --- | --- |
| `exa` | HTTP (BYOK) | `/contents` | `EXA_API_KEY` |
| `firecrawl` | HTTP (BYOK) | `/v2/scrape` | `FIRECRAWL_API_KEY` |
| `tavily` | HTTP (BYOK) | `/extract` | `TAVILY_API_KEY` |

Each backend calls the engine's REST API directly and takes an optional `api_key` and `api_base` override. Responses are parsed defensively, so a provider renaming a field degrades to a missing value rather than a hard error, and an empty extraction fails over to the next backend.

<Callout type="info">
**No URL is dereferenced by BitRouter.** Extraction runs on the backend provider's infrastructure — BitRouter hands the URL to Exa/Firecrawl/Tavily and never fetches the model-supplied address itself, so there's no server-side request-forgery surface to guard.
</Callout>

## Capping content

`max_content_tokens` bounds how much page text is returned. It resolves **deployment → declaration → call**, and each layer may only *lower* it — a caller can ask for less than the config allows, never more. The cap is enforced as `tokens × 4` characters (the router's `CHARS_PER_TOKEN` convention) and, where the backend supports it, pushed to the engine's own server-side limit.

```json
{ "type": "bitrouter:web_fetch", "args": { "max_content_tokens": 1500 } }
```

## Learn more

- [Server tools](/docs/features/server-tools) — the loop that runs `web_fetch`, plus Advisor, SubAgent, and Fusion.
- [Web search](/docs/features/websearch) — the sibling built-in that searches instead of fetching.
- [OpenTelemetry](/docs/features/opentelemetry) — every nested fetch call shows up in your traces like any other.
