---
title: Pi
description: 把 Pi（这款极简终端编码工具）通过 BitRouter 接入，作为 OpenAI 兼容的供应商使用。
sourceHash: a5270f935b7221c1a9ea6a9b6d2ebf4686ac0901c38a7afb85f27ae62fe9463b
---

Pi 是一款极简终端编码工具，它在 `~/.pi/agent/models.json` 中定义模型供应商。把 BitRouter 添加为 OpenAI 兼容供应商，Pi 即可路由到整个[注册表](/docs/concepts/models)中的模型。

## 前置条件

- BitRouter 已在运行——本地代理地址为 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/configuration)，地址为 `https://api.bitrouter.ai`。
- 已安装 Pi：

  ```bash
  npm install -g --ignore-scripts @earendil-works/pi-coding-agent
  ```

## 将 Pi 指向 BitRouter

在 `~/.pi/agent/models.json` 中添加一个供应商。使用 `api: "openai-completions"`，并把 `baseUrl` 指向 `/v1` 根路径。

```json
{
  "providers": {
    "bitrouter": {
      "baseUrl": "http://127.0.0.1:4356/v1",
      "api": "openai-completions",
      "apiKey": "local-placeholder",
      "models": [
        { "id": "openai/gpt-4o", "name": "GPT-4o via BitRouter" },
        { "id": "anthropic/claude-sonnet-4-6", "name": "Claude Sonnet 4.6 via BitRouter" }
      ]
    }
  }
}
```

然后在 TUI 中用 `/model` 选择模型。

<Callout type="info">
**本地代理无需密钥**——`apiKey` 可以是任意占位符。若使用 **Cloud**，请将 `baseUrl` 设为 `https://api.bitrouter.ai/v1`，并使用你的 BitRouter 密钥（该字段也支持 `$ENV_VAR` 引用）。
</Callout>

## 选择模型

每个 `models[].id` 都是 `provider/model` 形式的注册表 id，可选地带上 `:cost` / `:latency` 变体后缀。详见[模型](/docs/concepts/models)。

## 了解更多

- [Pi —— 自定义供应商](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/models.md)
- [模型回退](/docs/features/model-fallback)
