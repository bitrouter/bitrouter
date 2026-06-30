---
title: Tavily
description: 把 Tavily 的搜索 API 用作 BitRouter 的 web_search 后端——只需一个 Tavily 密钥，任何被路由的模型都能联网搜索。
sourceHash: 0937cc028632af1aa505d332aad2f3a693e328e99359f7e8d72fd768043c8757
---

[Tavily](https://tavily.com) 是一款为 Agent 与 RAG 打造的搜索 API——每条结果都附带相关内容摘要和分数。BitRouter 原生支持将其作为内置 [web search](/docs/features/websearch) 工具的后端。

## 获取密钥

在 [Tavily 控制台](https://app.tavily.com)中创建一个 API 密钥并导出：

```bash
export TAVILY_API_KEY=...
```

## 将 Tavily 添加到 BitRouter

在 `server_tools.web_search` 下声明一个 `tavily` 后端。密钥会从显式的 `api_key`（支持 `${VAR}`）解析；若省略，则按惯例使用 `TAVILY_API_KEY`：

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: tavily       # 密钥来自 api_key 或 TAVILY_API_KEY
```

<Callout type="info">
**优先级与故障转移。** `backends` 是一个有序列表——第一个能解析出密钥的即为默认项，某个后端失败时会自动转移到下一个。可以把 Tavily 与 [Exa](/docs/integrations/exa)、[Parallel](/docs/integrations/parallel) 或 [Firecrawl](/docs/integrations/firecrawl) 列在一起串联使用。
</Callout>

## 使用方式

请求通过声明该工具来启用它；用 `args.backend` 固定使用 Tavily：

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "tavily" } } ] }
```

模型随后会以 `query` 调用 `web_search`；BitRouter 会针对 Tavily 执行搜索，并把结果回填进工具循环中。Tavily 会为每条结果填充 `title`、`snippet`（内容）、`score` 和 `published`。

## 了解更多

- [Web search](/docs/features/websearch) —— 结果 schema、单次请求的 `max_results`，以及循环边界。
- [Tavily —— 文档](https://docs.tavily.com)
