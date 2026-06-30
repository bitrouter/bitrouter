---
title: Overview
description: Connect external tools — web search and retrieval providers — that the models you route through BitRouter can call.
sourceHash: f5ab6c60de45cb8fe3e87b500475ad8c71d740038da4106548ce5c779651222b
---

A **tool** is a capability a model picks up at runtime to get work done (see [concepts: tools](/docs/concepts/tools)). The integrations here are the **web-search and retrieval providers** that back BitRouter's built-in [web search](/docs/features/websearch): bring a key for any of them and *every* model you route through BitRouter gains a web search — even a model with no native search of its own.

<Cards>
  <Card title="Exa" href="/docs/integrations/exa" description="Neural / semantic web search · EXA_API_KEY" />
  <Card title="Parallel" href="/docs/integrations/parallel" description="Web search + research API for agents · PARALLEL_API_KEY" />
  <Card title="Firecrawl" href="/docs/integrations/firecrawl" description="Search + scrape to LLM-ready markdown · FIRECRAWL_API_KEY" />
  <Card title="Tavily" href="/docs/integrations/tavily" description="Search built for agents and RAG · TAVILY_API_KEY" />
</Cards>

## How it works

Each provider is a **backend** of the `web_search` server tool. You list the ones you have keys for under `server_tools.web_search` in `bitrouter.yaml`, in preference-and-failover order, and a request opts in per call by declaring `{"type":"bitrouter:web_search"}`. BitRouter runs the search itself and feeds normalized results back to the model — see [Web search](/docs/features/websearch) for the loop, the result shape, and per-request overrides.

```yaml
# bitrouter.yaml — one line per provider you bring a key for
server_tools:
  web_search:
    backends:
      - kind: exa            # EXA_API_KEY
      - kind: tavily         # TAVILY_API_KEY  (failover)
```

<Callout type="info">
**Need an arbitrary tool, not web search?** Any [MCP](https://modelcontextprotocol.io) server — a database, a file system, your own API — plugs into BitRouter's [MCP gateway](/docs/concepts/tools#the-mcp-gateway). The pages here cover the built-in search providers specifically.
</Callout>
