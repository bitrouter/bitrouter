---
title: Overview
description: 接入外部工具——通过 BitRouter 路由的模型可以调用的网页搜索与检索供应商。
sourceHash: f5ab6c60de45cb8fe3e87b500475ad8c71d740038da4106548ce5c779651222b
---

**工具**是模型在运行时调用以完成工作的能力（参见[概念：工具](/docs/concepts/tools)）。这里列出的集成是为 BitRouter 内置[网页搜索](/docs/features/websearch)提供支持的**网页搜索与检索供应商**：只要带上其中任意一个的密钥，你通过 BitRouter 路由的*每个*模型都能获得网页搜索能力——即便该模型本身并不具备原生搜索功能。

<Cards>
  <Card title="Exa" href="/docs/integrations/exa" description="神经 / 语义网页搜索 · EXA_API_KEY" />
  <Card title="Parallel" href="/docs/integrations/parallel" description="面向 Agent 的网页搜索 + 研究 API · PARALLEL_API_KEY" />
  <Card title="Firecrawl" href="/docs/integrations/firecrawl" description="搜索 + 抓取为 LLM 友好的 markdown · FIRECRAWL_API_KEY" />
  <Card title="Tavily" href="/docs/integrations/tavily" description="为 Agent 与 RAG 打造的搜索 · TAVILY_API_KEY" />
</Cards>

## 工作原理

每个供应商都是 `web_search` 服务端工具的一个**后端**。你在 `bitrouter.yaml` 的 `server_tools.web_search` 下列出已持有密钥的供应商，按优先级与故障转移顺序排列；某次请求则通过声明 `{"type":"bitrouter:web_search"}` 按需启用。BitRouter 会自行执行搜索，并把归一化后的结果回填给模型——关于循环流程、结果结构与单次请求的覆盖项，参见 [Web search](/docs/features/websearch)。

```yaml
# bitrouter.yaml —— 每个持有密钥的供应商占一行
server_tools:
  web_search:
    backends:
      - kind: exa            # EXA_API_KEY
      - kind: tavily         # TAVILY_API_KEY（故障转移）
```

<Callout type="info">
**需要任意工具而非网页搜索？** 任何 [MCP](https://modelcontextprotocol.io) 服务器——数据库、文件系统、你自己的 API——都可以接入 BitRouter 的 [MCP 网关](/docs/concepts/tools#the-mcp-gateway)。本页内容专门覆盖内置的搜索供应商。
</Callout>
