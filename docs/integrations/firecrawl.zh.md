---
title: Firecrawl
description: 把 Firecrawl 的搜索 API 作为 BitRouter 的 web_search 后端——准备好 Firecrawl 密钥，任何路由到的模型都能搜索网页。
sourceHash: 8351f71765f9a1b1c5e0e8516f413f39066e00813eb51411461fbc92e145b5e0
---

[Firecrawl](https://firecrawl.dev) 把网页转换成 LLM 可直接使用的数据——它的搜索端点会返回带描述的排序结果，如有需要还能把页面转成 markdown。BitRouter 原生支持将其作为内置 [web search](/docs/features/websearch) 工具的后端。

## 获取密钥

在 [Firecrawl 控制台](https://www.firecrawl.dev/app)创建一个 API 密钥并导出：

```bash
export FIRECRAWL_API_KEY=...
```

## 把 Firecrawl 接入 BitRouter

在 `server_tools.web_search` 下声明一个 `firecrawl` 后端。密钥可来自显式的 `api_key`（支持 `${VAR}` 形式），若省略则使用约定的 `FIRECRAWL_API_KEY`：

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: firecrawl    # key from api_key or FIRECRAWL_API_KEY
```

<Callout type="info">
**优先级与故障转移。** `backends` 是一个有序列表——第一个能解析出密钥的后端即为默认值，某个后端失败时会转移到下一个。可以把 Firecrawl 和 [Exa](/docs/integrations/exa)、[Parallel](/docs/integrations/parallel) 或 [Tavily](/docs/integrations/tavily) 列在一起串联使用。
</Callout>

## 使用方法

请求通过声明该工具来启用它；用 `args.backend` 固定使用 Firecrawl：

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "firecrawl" } } ] }
```

模型随后会带着一个 `query` 调用 `web_search`；BitRouter 会用它向 Firecrawl 发起请求，并把结果回填进工具循环中。Firecrawl 会为每条结果填充 `title`、`snippet`（描述），以及——在所有搜索后端中独有的——`content`（markdown）。

## 了解更多

- [Web search](/docs/features/websearch) —— 结果 schema、每次请求的 `max_results`，以及循环边界。
- [Firecrawl —— 文档](https://docs.firecrawl.dev)
