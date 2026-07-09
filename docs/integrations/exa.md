---
title: Exa
description: Use Exa's neural web search as a BitRouter web_search backend — bring an Exa key and any routed model can search the web.
sourceHash: f81429ec53dee3892ed80ab99b434242f84e4a6e69e68e0a09c6414f17aa4152
---

[Exa](https://exa.ai) is a neural, embeddings-based search API — it finds pages by meaning rather than keywords and returns ranked results with highlights and relevance scores. BitRouter speaks it natively as a backend for the built-in [web search](/docs/features/websearch) tool, so bringing it in is one line plus a key.

## Get a key

Create an API key in the [Exa dashboard](https://dashboard.exa.ai) and export it:

```bash
export EXA_API_KEY=...
```

## Add Exa to BitRouter

Declare an `exa` backend under `server_tools.web_search`. The key resolves from an explicit `api_key` (which supports `${VAR}`) or, when omitted, the conventional `EXA_API_KEY`:

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: exa          # key from api_key or EXA_API_KEY
```

<Callout type="info">
**Preference and failover.** `backends` is an ordered list — the first whose key resolves is the default, and a failing backend falls over to the next. List Exa alongside [Parallel](/docs/integrations/parallel), [Firecrawl](/docs/integrations/firecrawl), or [Tavily](/docs/integrations/tavily) to chain them.
</Callout>

## Use it

A request turns the tool on by declaring it; pin Exa with `args.backend`:

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "exa" } } ] }
```

The model then calls `web_search` with a `query`; BitRouter runs it against Exa and feeds the results back inside the tool loop. Exa fills `title`, `snippet` (highlights), `score`, and `published` on each result.

## Learn more

- [Web search](/docs/features/websearch) — the result schema, per-request `max_results`, and loop bounds.
- [Exa — API reference](https://docs.exa.ai)
