---
title: Claude Code
description: 让 Anthropic 的 Claude Code 经 BitRouter 路由——使用它已经会说的 Anthropic Messages API，访问注册表中的每一个模型。
sourceHash: ba5716929b662eb242b1293fadd37f20d416a2af82f6926e0c1d4c57d5ebd953
---

Claude Code 会和一个 Anthropic Messages 端点对话。把它指向 BitRouter 而非 `api.anthropic.com`，每个请求就会流经[注册表](/docs/concepts/models)——这样同一个 agent 就能跑在 Anthropic、OpenAI、Google 或某个开放模型上，底层由供应商选择与回退机制托底。

## 前置条件

- BitRouter 正在运行——本地代理位于 `http://127.0.0.1:4356`，或使用 [BitRouter Cloud](/docs/get-started/configuration)（`https://api.bitrouter.ai`）。
- 已安装 Claude Code：

  ```bash
  npm install -g @anthropic-ai/claude-code
  # 或使用原生安装器：
  curl -fsSL https://claude.ai/install.sh | bash
  ```

## 让 Claude Code 指向 BitRouter

Claude Code 读取三个环境变量。把 base URL 设为 BitRouter 的**主机根地址**——它会自行拼接 `/v1/messages`。

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:4356
export ANTHROPIC_AUTH_TOKEN=local-placeholder        # 作为 bearer token 发送
export ANTHROPIC_MODEL=anthropic/claude-sonnet-4-6
claude
```

<Callout type="info">
**本地代理不需要密钥。** 本地代理对回环（loopback）请求无需鉴权即可接受，因此 `ANTHROPIC_AUTH_TOKEN` 可以是任意占位符。对于 **Cloud**，把 `ANTHROPIC_BASE_URL` 设为 `https://api.bitrouter.ai`，并用你的 BitRouter 密钥（来自 `bitrouter cloud login` 或控制台）作为 token。
</Callout>

更喜欢用文件配置？把同样的值放进 `.claude/settings.json`，这样就能按项目生效而无需手动 export：

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:4356",
    "ANTHROPIC_AUTH_TOKEN": "local-placeholder",
    "ANTHROPIC_MODEL": "anthropic/claude-sonnet-4-6"
  }
}
```

## 选择模型

`ANTHROPIC_MODEL` 接受 `provider/model` 形式的任意注册表 id——例如 `anthropic/claude-sonnet-4-6`、`openai/gpt-4o`、`google/gemini-2.5-pro`。加上 `:cost` 或 `:latency` 后缀可以为本次会话调整供应商选择倾向。可在会话中用 `/model <id>` 切换，或在启动时用 `claude --model <id>` 指定。完整的 id 命名方案见 [模型](/docs/concepts/models)。

## 验证

启动 `claude`，问它任意问题，并确认收到响应。想知道实际作答的是哪个供应商，可查看 BitRouter 的 `request finished` 日志行（本地安装为 `~/.bitrouter/bitrouter.log`）——其中记录了服务该请求的 `provider` 和 `model`。

## 延伸阅读

- [Claude Code — LLM 网关配置](https://code.claude.com/docs/en/llm-gateway)
- [模型回退](/docs/features/model-fallback)——传入一份有序的模型列表，失败时沿列表向下尝试。
- [OpenTelemetry](/docs/features/opentelemetry)——追踪每一次 Claude Code 请求。
