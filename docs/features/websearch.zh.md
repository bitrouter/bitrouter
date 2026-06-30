---
title: Web search
description: 一个内置的 web_search 服务端工具——让任何经 BitRouter 路由的模型都能联网搜索，搜索后端由你自带密钥。
sourceHash: 08f1d4d084a942367630cc5e478ee7731095f7e79683137bc78ce330e1448cc8
---

`web_search` 是一个内置的[服务端工具](/docs/features/server-tools)：BitRouter 在工具调用循环内部自行执行搜索，并把结果喂回给模型。因此**任何经 BitRouter 路由的模型都能获得网络搜索能力**——即便它本身没有原生搜索——而背后的搜索引擎是你自带密钥接入的。

它与 Advisor、SubAgent、Fusion 同属一个家族：默认关闭，在配置中启用，并且只在请求声明了它时才会公布给模型。

## 工作原理

调用方通过在 `tools` 数组中声明它来为某次请求开启该功能：

```json
{
  "tools": [
    { "type": "bitrouter:web_search", "args": { "backend": "exa", "max_results": 3 } }
  ]
}
```

`args` 是可选的——省略它即可使用默认后端与默认上限。之后模型会带着一个 `query` 调用 `web_search`；BitRouter 把它转发给某个后端执行，并返回一个稳定的结果结构：

```json
{
  "backend": "exa",
  "answer": "…",
  "results": [
    { "url": "…", "title": "…", "snippet": "…", "content": "…", "published": "…", "score": 0.9 }
  ]
}
```

每条结果中除 `url` 外的所有字段都是可选的——没有哪个引擎能把它们全部填齐，因此这个契约是可叠加式的。`answer` 只在使用「问答型」`native` 后端时才会出现；REST 类引擎（Parallel、Exa、Firecrawl、Tavily）只返回 `results`，不返回 `answer`。[服务端工具](/docs/features/server-tools#how-the-loop-runs)中的循环上限（`max_iterations`、`tool_timeout` 等）与审批策略原样适用。

## 启用方式

在 `server_tools.web_search` 下声明后端列表。这份列表是一个**优先与故障转移顺序**——第一个能解析出密钥的后端即为默认值，某个后端失败时会转移到下一个：

```yaml
# bitrouter.yaml
server_tools:
  web_search:
    max_results: 5             # 可选的默认结果上限；调用方可在单次请求中调低它
    backends:                  # 优先 + 故障转移顺序
      - kind: parallel         # HTTP · 密钥来自 api_key 或 PARALLEL_API_KEY
      - kind: exa              # HTTP · 密钥来自 api_key 或 EXA_API_KEY
      - kind: firecrawl        # HTTP · 密钥来自 api_key 或 FIRECRAWL_API_KEY
      - kind: tavily            # HTTP · 密钥来自 api_key 或 TAVILY_API_KEY
      - kind: native            # 为每个模型复用某个供应商的原生搜索
        name: native            # 调用方用 `backend` 固定指向的后端 id
        model: anthropic/claude-opus-4.8
        tool: { type: "anthropic:web_search_20250305" }
```

<Callout type="info">
**没有密钥，就没有后端。** 每个 HTTP 后端会从显式的 `api_key`（支持 `${VAR}`）或约定俗成的 `*_API_KEY` 环境变量中解析密钥；解析不出密钥的后端会被静默跳过。如果**全部**都解析不出，BitRouter 会记录「`web_search` 已配置但没有任何后端可用」——这通常意味着缺了某个密钥。
</Callout>

## 后端

| `kind` | 类型 | 密钥 | 返回内容 |
| --- | --- | --- | --- |
| `parallel` | HTTP（自带密钥） | `PARALLEL_API_KEY` | `results[]` |
| `exa` | HTTP（自带密钥） | `EXA_API_KEY` | `results[]` |
| `firecrawl` | HTTP（自带密钥） | `FIRECRAWL_API_KEY` | `results[]` |
| `tavily` | HTTP（自带密钥） | `TAVILY_API_KEY` | `results[]` |
| `native` | 嵌套补全 | 一个可路由的 `model` + 其原生搜索 `tool` | `answer` |

- **HTTP 后端**（`parallel` / `exa` / `firecrawl` / `tavily`）直接调用各引擎的 REST API。每个都接受可选的 `api_key` 与 `api_base` 覆盖；响应解析是防御式的，因此即便供应商重命名了某个字段，也只会退化为该值缺失，而不会硬性报错。
- **native 后端**运行一次嵌套的模型补全，因此需要一个挂在某个供应商密钥之后、可路由的模型。它会转发某个供应商**自己的**搜索工具（例如 Anthropic 的 `web_search_20250305`），从而让某一家供应商的原生网络搜索可以被任何模型使用——包括那些本身没有搜索能力的模型。

## 按请求固定后端

调用方可以在声明的 `args.backend` 中指定一个后端名称（与各后端的 `kind`，或你在 `native` 上设置的 `name` 匹配）来覆盖默认值，并可用 `args.max_results` 调低结果上限：

```json
{ "type": "bitrouter:web_search", "args": { "backend": "tavily", "max_results": 8 } }
```

## 延伸阅读

- [服务端工具](/docs/features/server-tools)——运行 `web_search` 的那个循环，以及 Advisor、SubAgent 与 Fusion。
- [Toolset](/docs/features/server-tools#how-tools-are-bundled-toolsets)——某次请求中公布的工具是如何被打包与命名空间化的。
- [OpenTelemetry](/docs/features/opentelemetry)——每一次嵌套的搜索调用都会像其他调用一样出现在你的追踪记录中。
