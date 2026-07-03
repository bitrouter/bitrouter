---
title: Codex
description: 把 OpenAI 的 Codex CLI 注册为自定义模型供应商，让它经 BitRouter 路由。
sourceHash: c6042bc984b397d5a4214b910fabd2a09a71e8f1afb02672a747d1db8939a399
---

Codex CLI 可以通过 BitRouter 运行，而不接管你的 Codex 配置。最快的方式是 `bitrouter spawn --agent codex`，它只给这一次子进程注入 Codex 配置覆盖；如果你希望永久配置，也可以在 `~/.codex/config.toml` 中把 BitRouter 注册为自定义供应商。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/quickstart)（`https://api.bitrouter.ai`）。
- 已安装 Codex：

  ```bash
  curl -fsSL https://chatgpt.com/codex/install.sh | sh
  ```

## 使用 spawn 启动

```bash
bitrouter spawn --agent codex
```

`--` 之后的所有参数都会原样转发给 Codex：

```bash
bitrouter spawn --agent codex -- --model openai/gpt-5-codex
```

`spawn` 不会编辑 `~/.codex/config.toml`。它会保留你已有的 Codex 模型选择，然后把 Codex 指向一个临时的 `bitrouter` provider：`base_url = "<BitRouter>/v1"`，`wire_api = "responses"`。如果导出了 `BITROUTER_API_KEY`，Codex 会通过 `env_key` 使用它；否则 BitRouter 会注入一个本地占位凭据，适用于 `bitrouter init` 默认写入的 `skip_auth: true`。

## 永久配置

在 `~/.codex/config.toml` 中添加一个供应商配置块。`base_url` 包含 `/v1`——Codex 会自行追加路由（`/responses`）。

```toml
model_provider = "bitrouter"

[model_providers.bitrouter]
name = "BitRouter"
base_url = "http://127.0.0.1:4356/v1"
wire_api = "responses"
# env_key = "BITROUTER_API_KEY"  # 云端或开启鉴权的本地守护进程
```

```bash
codex
```

<Callout type="info">
**本地代理不需要密钥**——省略 `env_key`，Codex 就不会发送鉴权信息，而回环代理在 `skip_auth: true` 下会接受这样的请求。对于 **Cloud**，把 `base_url` 设为 `"https://api.bitrouter.ai/v1"`，在配置块中加上 `env_key = "BITROUTER_API_KEY"`，并 export 该变量为你的 BitRouter 密钥。
</Callout>

<Callout type="warn">
`model_provider` / `model_providers` 只在**用户级**的 `~/.codex/config.toml` 中生效，对项目本地的 `.codex/config.toml` 无效。
</Callout>

## 选择模型

Codex 的 `model` 设置，或按次运行的 `codex --model <id>`，都可以使用 `provider/model` 形式的任意注册表 id——例如 `openai/gpt-5-codex`、`anthropic/claude-sonnet-4-6`、`google/gemini-2.5-pro`——也可以选用 `:cost` 或 `:latency` 变体。详见 [模型](/docs/concepts/models)。

## 验证

运行 `codex` 并发出一次提问；查看 `bitrouter-served-by` 响应头，确认是哪个供应商作答的。

## 延伸阅读

- [Codex — 配置参考](https://developers.openai.com/codex/config-reference)
- [模型回退](/docs/features/model-fallback) · [供应商选择](/docs/features/provider-selection)
