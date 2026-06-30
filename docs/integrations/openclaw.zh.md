---
title: OpenClaw
description: 把 OpenClaw 的消息网关接入 BitRouter，将其作为 OpenAI 兼容的模型供应商。
sourceHash: cb0f07c8db3b5bdbd0bdfd4dc724e60d0512087582f5226655f70f5be1244bd8
---

OpenClaw 是一个自托管网关，用于把消息平台——WhatsApp、Telegram、Slack、Discord 等——桥接到 LLM。把 BitRouter 配置为它的模型供应商，每个渠道就都能路由到整个[注册表](/docs/concepts/models)。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/quickstart)，地址为 `https://api.bitrouter.ai`。
- 已安装 OpenClaw。安装方式见 [OpenClaw 文档](https://docs.openclaw.ai/)。

## 把 OpenClaw 指向 BitRouter

BitRouter 是一个 OpenAI 兼容的端点，因此可以直接作为供应商写入你的 OpenClaw 配置文件——无需插件。在 `models.providers` 下添加一个 `bitrouter` 条目，并将其引用为默认模型：

```json5
{
  agents: {
    defaults: {
      model: { primary: "bitrouter/openai/gpt-4o" },
    },
  },
  models: {
    providers: {
      bitrouter: {
        baseUrl: "http://127.0.0.1:4356/v1",
        apiKey: "local-placeholder",   // any value for the local proxy
        api: "openai-completions",
        models: [{ id: "openai/gpt-4o", contextWindow: 128000, maxTokens: 8192 }],
      },
    },
  },
}
```

然后启动网关：

```bash
openclaw gateway
```

<Callout type="info">
若使用 **Cloud**，把 `baseUrl` 设为 `https://api.bitrouter.ai/v1`，并使用你的 BitRouter 密钥。本地代理会忽略 `apiKey`，所以填占位值即可。
</Callout>

## 选择模型

每个供应商模型 `id` 都是 `provider/model` 形式的注册表 id（如 `openai/gpt-4o`、`anthropic/claude-sonnet-4-6` 等），可选地带上 `:cost` / `:latency` 变体。在 OpenClaw 的 `model.primary` 中，需要加上你所选的供应商 id 作为前缀——例如 `bitrouter/openai/gpt-4o`。参见[模型](/docs/concepts/models)。

## 了解更多

- [OpenClaw —— 模型供应商](https://docs.openclaw.ai/concepts/model-providers)
- [模型回退](/docs/features/model-fallback)
