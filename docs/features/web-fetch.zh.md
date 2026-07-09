---
title: Web fetch
description: 一个内置的 web_fetch 服务端工具——让任何经 BitRouter 路由的模型都能读取网页内容，抓取后端由你自带密钥。
---

`web_fetch` 是一个内置的[服务端工具](/docs/features/server-tools)：BitRouter 在工具调用循环内部自行执行抓取，并把提取出的网页内容喂回给模型。因此**任何经 BitRouter 路由的模型都能获得读取 URL 的能力**——把一个链接交给它，它就会返回干净、规整的页面内容——而背后的提取引擎是你自带密钥接入的。

它与 Advisor、SubAgent、Fusion 以及 [Web search](/docs/features/websearch) 同属一个家族：默认关闭，在配置中启用，并且只在请求声明了它时才会公布给模型。

## 工作原理

调用方通过在 `tools` 数组中声明它来为某次请求开启该功能：

```json
{
  "tools": [
    { "type": "bitrouter:web_fetch" }
  ]
}
```

之后模型会带着一个 `url` 调用 `web_fetch`；BitRouter 通过某个后端抓取它，并返回一个稳定的结果结构：

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

`title` 与 `published` 是可选的——没有哪个引擎能把它们全部填齐，因此这个契约是可叠加式的。[服务端工具](/docs/features/server-tools#how-the-loop-runs)中的循环上限（`max_iterations`、`tool_timeout` 等）与审批策略原样适用。

## 启用方式

在 `server_tools.web_fetch` 下声明后端列表。这份列表是一个**优先与故障转移顺序**——第一个能解析出密钥的后端即为默认值，某个后端失败（或提取为空）时会转移到下一个：

```yaml
# bitrouter.yaml
server_tools:
  web_fetch:
    max_content_tokens: 4000   # 可选的默认上限；声明或调用只能把它调低
    backends:                  # 优先 + 故障转移顺序
      - kind: exa              # HTTP · /contents · 密钥来自 api_key 或 EXA_API_KEY
      - kind: firecrawl        # HTTP · /v2/scrape · 密钥来自 api_key 或 FIRECRAWL_API_KEY
      - kind: tavily           # HTTP · /extract · 密钥来自 api_key 或 TAVILY_API_KEY
```

<Callout type="info">
**没有密钥，就没有后端。** 每个后端会从显式的 `api_key`（支持 `${VAR}`）或约定俗成的 `*_API_KEY` 环境变量中解析密钥；解析不出密钥的后端会被静默跳过。如果**全部**都解析不出，BitRouter 会记录「`web_fetch` 已配置但没有任何后端可用」——这通常意味着缺了某个密钥。
</Callout>

## 后端

| `kind` | 类型 | 端点 | 密钥 |
| --- | --- | --- | --- |
| `exa` | HTTP（自带密钥） | `/contents` | `EXA_API_KEY` |
| `firecrawl` | HTTP（自带密钥） | `/v2/scrape` | `FIRECRAWL_API_KEY` |
| `tavily` | HTTP（自带密钥） | `/extract` | `TAVILY_API_KEY` |

每个后端都直接调用各引擎的 REST API，并接受可选的 `api_key` 与 `api_base` 覆盖。响应解析是防御式的，因此即便供应商重命名了某个字段，也只会退化为该值缺失，而不会硬性报错；提取为空时则会故障转移到下一个后端。

<Callout type="info">
**BitRouter 不会解引用任何 URL。** 提取工作运行在后端供应商的基础设施上——BitRouter 把 URL 交给 Exa/Firecrawl/Tavily，自己从不去抓取模型给出的地址，因此不存在需要防范的服务端请求伪造（SSRF）面。
</Callout>

## 限制内容长度

`max_content_tokens` 用来限制返回的页面文本量。它按 **部署 → 声明 → 调用** 逐层解析，且每一层只能把它*调低*——调用方可以请求比配置更少的内容，但永远不能更多。该上限按 `tokens × 4` 个字符执行（路由器的 `CHARS_PER_TOKEN` 约定），并在后端支持时下推到引擎自身的服务端限制。

```json
{ "type": "bitrouter:web_fetch", "args": { "max_content_tokens": 1500 } }
```

## 延伸阅读

- [服务端工具](/docs/features/server-tools)——运行 `web_fetch` 的那个循环，以及 Advisor、SubAgent 与 Fusion。
- [Web search](/docs/features/websearch)——负责搜索（而非抓取）的姊妹内置工具。
- [OpenTelemetry](/docs/features/opentelemetry)——每一次嵌套的抓取调用都会像其他调用一样出现在你的追踪记录中。
