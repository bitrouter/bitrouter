---
title: 总览
description: 把模型、工具与 Agent 运行时接入 BitRouter。
sourceHash: 04eb19e177d2ad12fc475f1bfc45165f8993c065d097985acfdad627583a6c84
---

每个指南做的都是同一件事：把目标指向 BitRouter 的端点（本地代理 `http://127.0.0.1:4356`，或云端 `https://api.bitrouter.ai`），并用 `provider/model` 形式的模型 id 调用模型。这样即可获得完整的[模型注册表](/docs/concepts/models)、提供商优选与[失败回退](/docs/features/model-fallback)。初次使用？先看[快速开始](/docs/get-started/quickstart)。

集成分为三类：

## 模型（Models）

你的 token 从何而来——你已在付费的订阅、聚合器密钥，或你自己托管的模型。

<Cards>
  <Card title="模型总览" href="/docs/integrations/models" description="模型来源的三种形态" />
  <Card title="Claude 订阅" href="/docs/integrations/claude-subscription" description="走你的 Claude Pro/Max 套餐——OAuth，无需密钥" />
  <Card title="Codex 订阅" href="/docs/integrations/codex-subscription" description="经 Codex 后端走你的 ChatGPT 套餐" />
  <Card title="OpenRouter" href="/docs/integrations/openrouter" description="自带 OpenRouter 密钥" />
  <Card title="Ollama" href="/docs/integrations/ollama" description="本地运行开放模型 · :11434" />
  <Card title="vLLM" href="/docs/integrations/vllm" description="高吞吐 GPU 服务 · :8000" />
</Cards>

## 工具（Tools）

为模型提供联网检索能力的搜索服务，经 BitRouter 内置的 [web search](/docs/features/websearch) 调用。

<Cards>
  <Card title="工具总览" href="/docs/integrations/tools" description="搜索服务如何作为 web_search 的后端" />
  <Card title="Exa" href="/docs/integrations/exa" description="神经 / 语义网页搜索" />
  <Card title="Parallel" href="/docs/integrations/parallel" description="面向 Agent 的搜索 + 研究 API" />
  <Card title="Firecrawl" href="/docs/integrations/firecrawl" description="搜索 + 抓取为 LLM 友好的 markdown" />
  <Card title="Tavily" href="/docs/integrations/tavily" description="为 Agent 与 RAG 打造的搜索" />
</Cards>

## 运行时（Harnesses）

自行驱动循环的 Agent 运行时——把它指向 BitRouter，即可在任意模型上运行。

<Cards>
  <Card title="运行时总览" href="/docs/integrations/harnesses" description="运行时与模型来源的区别" />
  <Card title="Claude Code" href="/docs/integrations/claude-code" description="通过 ANTHROPIC_BASE_URL 走 Anthropic Messages" />
  <Card title="Codex" href="/docs/integrations/codex" description="在 ~/.codex/config.toml 中配置自定义 provider" />
  <Card title="OpenCode" href="/docs/integrations/opencode" description="在 opencode.json 中配置 provider" />
  <Card title="Hermes" href="/docs/integrations/hermes" description="Nous Research 的自我改进 Agent" />
  <Card title="OpenClaw" href="/docs/integrations/openclaw" description="跨多平台的消息网关" />
</Cards>

## 切换网关

<Cards>
  <Card title="从 LiteLLM 迁移" href="/docs/guides/migrate-from-litellm" description="替换网关，代码不变" />
  <Card title="从 OpenRouter 迁移" href="/docs/guides/migrate-from-openrouter" description="改 base URL 和密钥即可" />
</Cards>
