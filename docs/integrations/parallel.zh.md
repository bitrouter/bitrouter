---
title: Parallel
description: 把 Parallel 的网页搜索 API 作为 BitRouter 的 web_search 后端——准备好 Parallel 密钥，任何路由到的模型都能搜索网页。
sourceHash: 017884a14bd45f94669a4cde794eb6c2ab05620d1dc3a5c87d202039bc5eb874
---

[Parallel](https://parallel.ai) 是一个面向 AI Agent 打造的网页搜索与研究 API——你给它一个目标和若干查询，它返回带摘录的排序结果。BitRouter 原生支持将其作为内置 [web search](/docs/features/websearch) 工具的后端。

## 获取密钥

在 [Parallel 控制台](https://platform.parallel.ai)创建一个 API 密钥并导出：

```bash
export PARALLEL_API_KEY=...
```

## 把 Parallel 接入 BitRouter

在 `server_tools.web_search` 下声明一个 `parallel` 后端。密钥可来自显式的 `api_key`（支持 `${VAR}` 形式），若省略则使用约定的 `PARALLEL_API_KEY`：

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: parallel     # key from api_key or PARALLEL_API_KEY
```

<Callout type="info">
**优先级与故障转移。** `backends` 是一个有序列表——第一个能解析出密钥的后端即为默认值，某个后端失败时会转移到下一个。可以把 Parallel 和 [Exa](/docs/integrations/exa)、[Firecrawl](/docs/integrations/firecrawl) 或 [Tavily](/docs/integrations/tavily) 列在一起串联使用。
</Callout>

## 使用方法

请求通过声明该工具来启用它；用 `args.backend` 固定使用 Parallel：

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "parallel" } } ] }
```

模型随后会带着一个 `query` 调用 `web_search`；BitRouter 会把它映射为 Parallel 的目标（objective）加搜索查询，执行后将结果回填进工具循环中。Parallel 会为每条结果填充 `title`、`snippet`（摘录）和 `published`。

## 了解更多

- [Web search](/docs/features/websearch) —— 结果 schema、每次请求的 `max_results`，以及循环边界。
- [Parallel —— 文档](https://docs.parallel.ai)
