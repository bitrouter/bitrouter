---
title: Parallel
description: Use Parallel's web search API as a BitRouter web_search backend — bring a Parallel key and any routed model can search the web.
sourceHash: 017884a14bd45f94669a4cde794eb6c2ab05620d1dc3a5c87d202039bc5eb874
---

[Parallel](https://parallel.ai) is a web search and research API built for AI agents — you give it an objective and queries, and it returns ranked results with excerpts. BitRouter speaks it natively as a backend for the built-in [web search](/docs/features/websearch) tool.

## Get a key

Create an API key in the [Parallel dashboard](https://platform.parallel.ai) and export it:

```bash
export PARALLEL_API_KEY=...
```

## Add Parallel to BitRouter

Declare a `parallel` backend under `server_tools.web_search`. The key resolves from an explicit `api_key` (which supports `${VAR}`) or, when omitted, the conventional `PARALLEL_API_KEY`:

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: parallel     # key from api_key or PARALLEL_API_KEY
```

<Callout type="info">
**Preference and failover.** `backends` is an ordered list — the first whose key resolves is the default, and a failing backend falls over to the next. List Parallel alongside [Exa](/docs/integrations/exa), [Firecrawl](/docs/integrations/firecrawl), or [Tavily](/docs/integrations/tavily) to chain them.
</Callout>

## Use it

A request turns the tool on by declaring it; pin Parallel with `args.backend`:

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "parallel" } } ] }
```

The model then calls `web_search` with a `query`; BitRouter mirrors it into Parallel's objective plus search query, runs it, and feeds the results back inside the tool loop. Parallel fills `title`, `snippet` (excerpts), and `published` on each result.

## Learn more

- [Web search](/docs/features/websearch) — the result schema, per-request `max_results`, and loop bounds.
- [Parallel — documentation](https://docs.parallel.ai)
