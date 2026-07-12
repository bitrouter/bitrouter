---
title: Firecrawl
description: Use Firecrawl's search API as a BitRouter web_search backend — bring a Firecrawl key and any routed model can search the web.
sourceHash: 8351f71765f9a1b1c5e0e8516f413f39066e00813eb51411461fbc92e145b5e0
---

[Firecrawl](https://firecrawl.dev) turns the web into LLM-ready data — its search endpoint returns ranked results with a description and, when asked, the page as markdown. BitRouter speaks it natively as a backend for the built-in [web search](/docs/features/websearch) tool.

## Get a key

Create an API key in the [Firecrawl dashboard](https://www.firecrawl.dev/app) and export it:

```bash
export FIRECRAWL_API_KEY=...
```

## Add Firecrawl to BitRouter

Declare a `firecrawl` backend under `server_tools.web_search`. The key resolves from an explicit `api_key` (which supports `${VAR}`) or, when omitted, the conventional `FIRECRAWL_API_KEY`:

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: firecrawl    # key from api_key or FIRECRAWL_API_KEY
```

<Callout type="info">
**Preference and failover.** `backends` is an ordered list — the first whose key resolves is the default, and a failing backend falls over to the next. List Firecrawl alongside [Exa](/docs/integrations/exa), [Parallel](/docs/integrations/parallel), or [Tavily](/docs/integrations/tavily) to chain them.
</Callout>

## Use it

A request turns the tool on by declaring it; pin Firecrawl with `args.backend`:

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "firecrawl" } } ] }
```

The model then calls `web_search` with a `query`; BitRouter runs it against Firecrawl and feeds the results back inside the tool loop. Firecrawl fills `title`, `snippet` (description), and — uniquely among the search backends — `content` (markdown) on each result.

## Learn more

- [Web search](/docs/features/websearch) — the result schema, per-request `max_results`, and loop bounds.
- [Firecrawl — documentation](https://docs.firecrawl.dev)
