---
title: Codex
description: 把 OpenAI 的 Codex CLI 注册为自定义模型供应商，让它经 BitRouter 路由。
sourceHash: c6042bc984b397d5a4214b910fabd2a09a71e8f1afb02672a747d1db8939a399
---

Codex CLI 可以通过 BitRouter 运行，而不接管你的 Codex 配置。最快的方式是 `bitrouter launch --agent codex`，它只给这一次子进程注入 Codex 配置覆盖；如果你希望永久配置，也可以在 `~/.codex/config.toml` 中把 BitRouter 注册为自定义供应商。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/configuration)（`https://api.bitrouter.ai`）。
- 已安装 Codex：

  ```bash
  curl -fsSL https://chatgpt.com/codex/install.sh | sh
  ```

## 通过 BitRouter 启动

```bash
bitrouter launch --agent codex
```

`--` 之后的所有参数都会原样转发给 Codex：

```bash
bitrouter launch --agent codex -- --model openai/gpt-5-codex
```

开始长时间运行前，可以让 BitRouter 检查它将使用的路由：

```bash
bitrouter launch --agent codex --check -- --model openai/gpt-5-codex
```

该检查会确认已安装 `codex`、BitRouter base URL 可访问，并且转发的模型至少有一个兼容 Responses 的端点。它**不要求**供应商必须是 `openai-codex`；任何通过 Responses 协议路由的模型来源都可以供 Codex harness 使用。

`launch` 不会编辑 `~/.codex/config.toml`。它会保留你已有的 Codex 模型选择，然后把 Codex 指向一个临时的 `bitrouter` provider：`base_url = "<BitRouter>/v1"`，`wire_api = "responses"`。如果导出了 `BITROUTER_API_KEY`，Codex 会通过 `env_key` 使用它；否则 BitRouter 会注入一个本地占位凭据，适用于 `bitrouter init` 默认写入的 `skip_auth: true`。

<Callout type="warn">
避免通过 `bitrouter launch --agent codex` 转发 Codex 的 `-c` / `--config` 参数。当前 Codex 版本可能会让这些转发的配置参数覆盖临时 provider 注入，导致本次运行静默停止使用 BitRouter。`launch` 会拒绝这种形态，并要求你把选项移到 Codex 配置里，或用 `--check` 检查路由。
</Callout>

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

Codex 的 `model` 设置，或按次运行的 `codex --model <id>`，都可以使用 `provider/model` 形式的任意注册表 id——例如 `openai/gpt-5-codex`、`anthropic/claude-sonnet-4-6`、`google/gemini-2.5-pro`。在 BitRouter Cloud 上，你还可以添加 `:cost` 或 `:latency` 这类托管路由 profile。对于本地 OSS 守护进程，请先在 `bitrouter.yaml` 中定义对应的 `variants`，再使用后缀。详见 [模型](/docs/concepts/models)。

## 验证

运行 `codex` 并发出一次提问；查看 BitRouter 的 `request finished` 日志行（本地安装为 `~/.bitrouter/bitrouter.log`），确认是哪个 `provider` 和 `model` 作答的。

## 延伸阅读

- [Codex — 配置参考](https://developers.openai.com/codex/config-reference)
- [模型回退](/docs/features/model-fallback) · [供应商选择](/docs/features/provider-selection)
