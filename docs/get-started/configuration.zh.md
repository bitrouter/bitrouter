---
title: Configuration
description: 在一分钟内安装 BitRouter 并接入你的 Agent——通过 Agent Skills 或 CLI/TUI，用你自己的密钥自托管，或附加到 BitRouter Cloud。
---

本页将在一分钟内让 BitRouter 为你的 Agent 提供路由，随后介绍所有配置方式。BitRouter 有两种部署模式，每种模式都有两种接入方式：

- **BitRouter Cloud（默认）。** 托管端点 `api.bitrouter.ai`。无需管理上游密钥；支持 Agent 原生 x402/MPP 按请求付费，也可叠加 BYOK 使用。
- **本地代理。** 单一二进制运行于本机，BYOK 使用你自己的提供商密钥。零基础设施依赖。

两种模式都运行**同一套开源核心**（Apache 2.0）——路由引擎完全相同，因此你可以先用一种模式，之后无需改动 harness 即可切换。不确定选哪个？参见 FAQ 中的[自托管还是 Cloud？](/docs/get-started/faqs#self-host-or-cloud)。

## 安装二进制

<Tabs items={['macOS / Linux', 'Homebrew', 'npm', 'cargo']}>
<Tab value="macOS / Linux">

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bitrouter/bitrouter/releases/latest/download/bitrouter-installer.sh | sh
```

</Tab>
<Tab value="Homebrew">

```bash
brew install bitrouter/tap/bitrouter
```

</Tab>
<Tab value="npm">

```bash
npm install -g bitrouter
```

</Tab>
<Tab value="cargo">

```bash
cargo install bitrouter
```

</Tab>
</Tabs>

## 通过 Agent Skills 接入

适用于支持 skills 的 Agent 运行时（Claude Code、Cursor、Codex、Copilot 等）。一次安装：

```bash
npx skills add bitrouter/bitrouter
```

然后告诉你的 Agent：*"帮我设置好 BitRouter。"*——Agent 会运行向导、默认选择 Cloud，并自动完成连接验证。

## 通过 CLI / TUI 接入

如需命令行优先的接入，启动向导：

```bash
bitrouter
```

向导分为三步——**凭据**（默认登录 BitRouter Cloud，或登录某个提供方，或粘贴 BYOK 密钥）、**Harness**（Claude Code 或 Codex）、**收尾**（立即启动 agent、在 `http://127.0.0.1:4356` 启动代理，或退出）。随时用 `bitrouter init` 重新运行；加上 `--yes` 即可非交互式驱动（它会输出 JSON 结果信封且从不阻塞）。

## 自托管运行

把供应商密钥设到环境里，然后启动代理：

```bash
export OPENAI_API_KEY=sk-...    # ANTHROPIC_API_KEY / GEMINI_API_KEY also work
bitrouter start
# Proxy running at http://127.0.0.1:4356
```

BitRouter 在启动时自动检测环境中的任意密钥——无需配置文件。任何已设置密钥的供应商立即可用。完整的识别变量列表见 [BYOK](/docs/features/byok)，或参阅 [本地与私有模型](/docs/integrations/models) 把 BitRouter 指向 Ollama、vLLM 或 LM Studio，完全免费。

如需高级路由规则、护栏或多账号故障转移，可生成一份配置文件：

```bash
bitrouter init          # writes ./bitrouter.yaml (override with `-c <path>`)
bitrouter start
```

## 使用 BitRouter Cloud

从终端登录 BitRouter Cloud 账号——一个账号即可覆盖托管网络提供的所有模型，无需任何上游供应商密钥：

```bash
bitrouter cloud login   # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

你也可以不运行本地二进制，直接把 agent 指向托管端点。无论哪种方式，核心都是同一套——云端账号是账号与网络，而非另一套部署。模型目录与价格见 [Supported Models](/docs/get-started/supported-models)。

## 将 Cloud 附加到自托管二进制

Cloud 不是另一个二进制——它是你附加的一个账户：

```bash
bitrouter cloud login
# 在浏览器中登录并选择一个工作区。
# 你的本地二进制现在可以在 BYOK 密钥之外路由 Cloud 托管的模型。
```

你可以随时添加或移除 Cloud 账户，二进制的自托管能力不受任何影响。

## 把你的 agent 指向代理

无论你以何种方式启动，BitRouter 都是一个即插即用的代理。把你的 agent 运行时指向代理的基础 URL——自托管时为 `http://127.0.0.1:4356`——每一次模型调用都会经由 BitRouter 路由，且无需修改任何 harness。

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

<Cards>
  <Card title="集成" href="/docs/integrations" description="覆盖每个 Agent 运行时的分步接入指南" />
  <Card title="Supported Models" href="/docs/get-started/supported-models" description="完整目录、定价与开放模型折扣" />
  <Card title="FAQs" href="/docs/get-started/faqs" description="自托管 vs Cloud、Cloud 的增量，以及 BitRouter 的对比" />
  <Card title="提供商接入" href="/docs/guides/register-as-a-provider" description="将你的模型列入 BitRouter Registry" />
</Cards>
