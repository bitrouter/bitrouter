---
title: Exa
description: 把 Exa 的神经网络搜索作为 BitRouter 的 web_search 后端——准备好 Exa 密钥，任何路由到的模型都能搜索网页。
sourceHash: f81429ec53dee3892ed80ab99b434242f84e4a6e69e68e0a09c6414f17aa4152
---

[Exa](https://exa.ai) 是一个基于神经网络（embedding）的搜索 API——它按语义而非关键词查找页面，并返回带高亮和相关性分数的排序结果。BitRouter 原生支持将其作为内置 [web search](/docs/features/websearch) 工具的后端，接入只需一行配置加一把密钥。

## 获取密钥

在 [Exa 控制台](https://dashboard.exa.ai)创建一个 API 密钥并导出：

```bash
export EXA_API_KEY=...
```

## 把 Exa 接入 BitRouter

在 `server_tools.web_search` 下声明一个 `exa` 后端。密钥可来自显式的 `api_key`（支持 `${VAR}` 形式），若省略则使用约定的 `EXA_API_KEY`：

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    backends:
      - kind: exa          # key from api_key or EXA_API_KEY
```

<Callout type="info">
**优先级与故障转移。** `backends` 是一个有序列表——第一个能解析出密钥的后端即为默认值，某个后端失败时会转移到下一个。可以把 Exa 和 [Parallel](/docs/integrations/parallel)、[Firecrawl](/docs/integrations/firecrawl) 或 [Tavily](/docs/integrations/tavily) 列在一起串联使用。
</Callout>

## 使用方法

请求通过声明该工具来启用它；用 `args.backend` 固定使用 Exa：

```json
{ "tools": [ { "type": "bitrouter:web_search", "args": { "backend": "exa" } } ] }
```

模型随后会带着一个 `query` 调用 `web_search`；BitRouter 会用它向 Exa 发起请求，并把结果回填进工具循环中。Exa 会为每条结果填充 `title`、`snippet`（高亮片段）、`score` 和 `published`。

## 了解更多

- [Web search](/docs/features/websearch) —— 结果 schema、每次请求的 `max_results`，以及循环边界。
- [Exa —— API 参考](https://docs.exa.ai)
