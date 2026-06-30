---
title: 快速入门
description: 通过 Agent Skills 或 CLI/TUI，在一分钟内启动 BitRouter。
sourceHash: d13266c9cc32a83fe7b97c37ae8d00fd6ebd116673c84b9b10de0f3432b3f1db
---

本指南将在一分钟内让 BitRouter 为你的 Agent 提供路由。BitRouter 有两种部署模式，每种模式都有两种接入方式：

- **BitRouter Cloud（默认）。** 托管端点 `cloud.bitrouter.ai`。无需管理密钥；支持 Agent 原生 x402/MPP 按请求付费，也可叠加 BYOK 使用。
- **本地代理。** 单一二进制运行于本机，BYOK 使用你自己的提供商密钥。零基础设施依赖。

两种模式都可通过 **Agent Skills**（让你的 Agent 自行安装与配置）或 **BitRouter CLI / TUI**（交互式向导）完成接入。两者均默认使用 Cloud；按一个键即可切换为本地模式。

## Agent Skills

适用于支持 skills 的 Agent 运行时（Claude Code、Cursor、Codex、Copilot 等）。一次安装：

```bash
npx skills add bitrouter/bitrouter
```

然后告诉你的 Agent：*"帮我设置好 BitRouter。"*——Agent 会运行向导、默认选择 Cloud，并自动完成连接验证。

## BitRouter CLI

适用于命令行优先的接入。安装二进制：

<Tabs items={['curl', 'npm', 'Homebrew', 'cargo']}>
<Tab value="curl">

```bash
curl -fsSL https://bitrouter.ai/install.sh | sh
```

</Tab>
<Tab value="npm">

```bash
npm install -g bitrouter
```

</Tab>
<Tab value="Homebrew">

```bash
brew install bitrouter/tap/bitrouter
```

</Tab>
<Tab value="cargo">

```bash
cargo install bitrouter
```

</Tab>
</Tabs>

启动 TUI 向导：

```bash
bitrouter
```

向导会询问 **Cloud 还是本地？**（默认 Cloud），如果选择本地则提示输入密钥，随后在 `http://127.0.0.1:4356` 启动代理。

发送请求验证：

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openai/gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

将任意 OpenAI 兼容的运行时指向 `http://127.0.0.1:4356/v1` 即可通过 BitRouter 路由。

## 下一步

具体 Agent 运行时（Claude Code、OpenClaw、Codex CLI、OpenCode 等）的接入方法详见「集成」。

<Cards>
  <Card title="集成" href="/docs/integrations" description="覆盖每个 Agent 运行时的分步接入指南" />
  <Card title="对比" href="/docs/get-started/comparison" description="BitRouter 与 OpenRouter、LiteLLM 及其他网关的差异" />
  <Card title="提供商接入" href="/docs/guides/register-as-a-provider" description="将你的模型列入 BitRouter Registry" />
  <Card title="API 参考" href="/docs/reference" description="端点与 schema 文档" />
  <Card title="GitHub" href="https://github.com/bitrouter" description="源代码与贡献" external />
  <Card title="Discord" href="https://discord.gg/G3zVrZDa5C" description="社区帮助与动态" external />
</Cards>
