---
title: Codex
description: 把 OpenAI 的 Codex CLI 注册为自定义模型供应商，让它经 BitRouter 路由。
sourceHash: c6042bc984b397d5a4214b910fabd2a09a71e8f1afb02672a747d1db8939a399
---

Codex CLI 允许你在 `~/.codex/config.toml` 中注册一个自定义的、兼容 OpenAI 的供应商。把 BitRouter 添加为该供应商，Codex 就会经由整个[注册表](/docs/concepts/models)路由，而不再只能用 OpenAI。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/quickstart)（`https://api.bitrouter.ai`）。
- 已安装 Codex：

  ```bash
  npm install -g @openai/codex
  ```

## 让 Codex 指向 BitRouter

在 `~/.codex/config.toml` 中添加一个供应商配置块。`base_url` 包含 `/v1`——Codex 会自行追加路由（`/chat/completions`）。

```toml
model = "openai/gpt-4o"
model_provider = "bitrouter"

[model_providers.bitrouter]
name = "BitRouter"
base_url = "http://127.0.0.1:4356/v1"
wire_api = "chat"          # Chat Completions；若要走 /v1/responses 则用 "responses"
```

```bash
codex
```

<Callout type="info">
**本地代理不需要密钥**——省略 `env_key`，Codex 就不会发送鉴权信息，而回环代理会接受这样的请求。对于 **Cloud**，把 `base_url` 设为 `"https://api.bitrouter.ai/v1"`，在配置块中加上 `env_key = "BITROUTER_API_KEY"`，并 export 该变量为你的 BitRouter 密钥。
</Callout>

<Callout type="warn">
`model_provider` / `model_providers` 只在**用户级**的 `~/.codex/config.toml` 中生效，对项目本地的 `.codex/config.toml` 无效。
</Callout>

## 选择模型

`model` 字段接受 `provider/model` 形式的任意注册表 id——例如 `openai/gpt-4o`、`anthropic/claude-sonnet-4-6`、`google/gemini-2.5-pro`——也可以选用 `:cost` 或 `:latency` 变体。用 `codex --model <id>` 可以按次运行覆盖。详见 [模型](/docs/concepts/models)。

## 验证

运行 `codex` 并发出一次提问；查看 `bitrouter-served-by` 响应头，确认是哪个供应商作答的。

## 延伸阅读

- [Codex — 配置参考](https://developers.openai.com/codex/config-reference)
- [模型回退](/docs/features/model-fallback) · [供应商选择](/docs/features/provider-selection)
