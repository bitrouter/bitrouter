---
title: Overview
description: 把编码 Agent 或运行时指向 BitRouter——Claude Code、Codex、OpenCode、Hermes、OpenClaw、Pi——即可在 registry 中的任意模型上运行。
sourceHash: 85f397c71e014e6dec7bcdb24119abf335a484362d4dccf2cb2e10df212a475d
---

**运行时（harness）**指的是驱动循环的 Agent 运行环境——读取你的 prompt、调用工具、编辑文件的 CLI 或服务。每一种运行时本来就会说某种模型 API；要做的事情始终一样：把它指向 BitRouter 的端点（本地代理 `http://127.0.0.1:4356`，或云端 `https://api.bitrouter.ai`），而不是厂商自己的端点，并用 `provider/model` 形式的 id 来寻址模型。这样一来，同一个运行时就能跑在 Anthropic、OpenAI、Google 或任意开放模型上——底层由[供应商选择](/docs/features/provider-selection)与[失败回退](/docs/features/model-fallback)支撑。

<Cards>
  <Card title="Claude Code" href="/docs/integrations/claude-code" description="通过 ANTHROPIC_BASE_URL 走 Anthropic Messages" />
  <Card title="Codex" href="/docs/integrations/codex" description="spawn 包装器或自定义 provider" />
  <Card title="OpenCode" href="/docs/integrations/opencode" description="在 opencode.json 中配置 provider" />
  <Card title="Hermes" href="/docs/integrations/hermes" description="Nous Research 的自我改进 Agent" />
  <Card title="OpenClaw" href="/docs/integrations/openclaw" description="跨多平台的消息网关" />
  <Card title="Pi" href="/docs/integrations/pi" description="通过 models.json 配置的极简终端编码运行时" />
</Cards>

## 运行时 vs. 模型来源

其中两项和某个[模型来源](/docs/integrations/models)同名，值得把它们分清楚：

| 你想要… | 使用 |
| --- | --- |
| 在任意模型上运行 **Claude Code CLI** | [Claude Code](/docs/integrations/claude-code)（运行时） |
| 把 **Claude 套餐**作为 token 消费 | [Claude subscription](/docs/integrations/claude-subscription)（模型） |
| 在任意模型上运行 **Codex CLI** | [Codex](/docs/integrations/codex)（运行时） |
| 把 **ChatGPT 套餐**作为 token 消费 | [Codex subscription](/docs/integrations/codex-subscription)（模型） |

运行时是*跑什么*；模型来源是 *token 从哪来*。两者可以自由组合——例如让 Claude Code（运行时）跑在你的 ChatGPT 套餐（Codex subscription）上，或者跑在本地的 Ollama 模型上。

## 第一次用 BitRouter？

先看[快速开始](/docs/get-started/configuration)把代理跑起来，再回来把你的运行时指向它。
