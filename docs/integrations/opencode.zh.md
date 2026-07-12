---
title: OpenCode
description: 在 opencode.json 中把 BitRouter 添加为 OpenAI 兼容供应商，从而通过 BitRouter 路由 OpenCode。
sourceHash: 94d4da78f9eb3926cce3a90d1b8f45caafb742f1b47a7715fec61618f3391bd2
---

OpenCode 通过 `opencode.json` 并借助 AI SDK 注册供应商。把 BitRouter 添加为 OpenAI 兼容供应商后，终端 Agent 就能路由到整个[注册表](/docs/concepts/models)。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/configuration)，地址为 `https://api.bitrouter.ai`。
- 已安装 OpenCode：

  ```bash
  curl -fsSL https://opencode.ai/install | bash
  # or: npm install -g opencode-ai
  ```

## 把 OpenCode 指向 BitRouter

在 `opencode.json`（项目根目录或 `~/.config/opencode/opencode.json`）中添加一个供应商区块。`baseURL` 指向 `/v1` 根路径；模型键则是你想暴露的注册表 id。

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "bitrouter": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "BitRouter",
      "options": {
        "baseURL": "http://127.0.0.1:4356/v1",
        "apiKey": "{env:BITROUTER_API_KEY}"
      },
      "models": {
        "openai/gpt-4o": { "name": "GPT-4o via BitRouter" },
        "anthropic/claude-sonnet-4-6": { "name": "Claude Sonnet 4.6 via BitRouter" }
      }
    }
  }
}
```

```bash
opencode
```

<Callout type="info">
**本地代理无需密钥**——`apiKey` 可以填任意占位值。若使用 **Cloud**，把 `baseURL` 设为 `https://api.bitrouter.ai/v1`，并提供你的 BitRouter 密钥（通过上面的 `{env:...}` 引用，或运行 `opencode auth login`）。
</Callout>

## 选择模型

`models` 下的每个键都是 `provider/model` 形式的注册表 id，可选地带上 `:cost` / `:latency` 变体。在 TUI 中选择一个，或运行 `opencode run --model bitrouter/openai/gpt-4o`。参见[模型](/docs/concepts/models)。

## 了解更多

- [OpenCode —— 供应商](https://opencode.ai/docs/providers/) · [配置](https://opencode.ai/docs/config/)
- [模型回退](/docs/features/model-fallback)
