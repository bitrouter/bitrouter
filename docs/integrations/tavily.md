---
title: Tavily
description: Use Tavily's search API as a BitRouter web_search backend — bring a Tavily key and any routed model can search the web.
sourceHash: 0937cc028632af1aa505d332aad2f3a693e328e99359f7e8d72fd768043c8757
---

[Tavily](https://tavily.com) is a search API built for agents and RAG — it returns ranked results with a relevant content snippet and a score per hit. BitRouter speaks it natively as a backend for the built-in [web search](/docs/features/websearch) tool.

## Get a key

Create an API key in the [Tavily dashboard](https://app.tavily.com) and export it:

```bash
export TAVILY_API_KEY=...
```

## Add Tavily to BitRouter

Declare a `tavily` backend under `server_tools.web_search`. The key resolves from an explicit `api_key` (which supports `${VAR}`) or, when omitted, the conventional `TAVILY_API_KEY`:

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: tavily       # key from api_key or TAVILY_API_KEY
```

<Callout type="info">
**Preference and failover.** `backends` is an ordered list — the first whose key resolves is the default, and a failing backend falls over to the next. List Tavily alongside [Exa](/docs/integrations/exa), [Parallel](/docs/integrations/parallel), or [Firecrawl](/docs/integrations/firecrawl) to chain them.
</Callout>

## Use it

A request turns the tool on by declaring it; pin Tavily with `args.backend`:

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "tavily" } } ] }
```

The model then calls `web_search` with a `query`; BitRouter runs it against Tavily and feeds the results back inside the tool loop. Tavily fills `title`, `snippet` (content), `score`, and `published` on each result.

## Learn more

- [Web search](/docs/features/websearch) — the result schema, per-request `max_results`, and loop bounds.
- [Tavily — documentation](https://docs.tavily.com)
