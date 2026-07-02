---
title: Self-Hosted & Cloud
description: 安装开源的 BitRouter 二进制并自托管运行，或登录 BitRouter Cloud——两种方式核心完全一致。如何安装两者，以及 Cloud 在其之上额外提供了什么。
---

BitRouter 有两扇入口，且都运行**同一套开源核心**（Apache 2.0）。用你自己的密钥自托管二进制，或登录 BitRouter Cloud——路由引擎完全相同。本页介绍如何安装并运行两者，再拆解一个 Cloud 账户额外提供了什么，帮助你选择起点。

## 安装二进制

安装开源二进制：

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
bitrouter auth login    # RFC 8628 device flow against api.bitrouter.ai
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

你也可以不运行本地二进制，直接把 agent 指向托管端点。无论哪种方式，核心都是同一套——云端账号是账号与网络，而非另一套部署。模型目录与价格见 [Models](/docs/get-started/models)。

## 把你的 agent 指向代理

无论你以何种方式启动，BitRouter 都是一个即插即用的代理。把你的 agent 运行时指向代理的基础 URL——自托管时为 `http://127.0.0.1:4356`——每一次模型调用都会经由 BitRouter 路由，且无需修改任何 harness。

## 核心能力完全相同

无论你自托管二进制还是附加 Cloud 账户，所有路由、回退、模型变体、BYOK、本地模型、防护（guardrails）、可观测性、MCP、ACP 和结构化输出能力都完全一致。Cloud 补充的是那些需要_你不用自己运维_的服务器才能提供的功能——它不会替换或限制核心能力。

### 能力对比

| 能力 | 自托管（OSS） | 云 |
| --- | --- | --- |
| 通用 API + 跨协议路由 | ✅ | ✅ |
| BYOK（自带供应商密钥） | ✅ | ✅ |
| 本地 / 私有模型部署 | ✅ | ✅ |
| 模型回退与供应商选择 | ✅ | ✅ |
| 模型变体与预设 | ✅ | ✅ |
| 防护（Guardrails） | ✅ | ✅ |
| 可观测性（OTLP 追踪 + 指标导出） | ✅ | ✅ |
| MCP 与 ACP 网关 | ✅ | ✅ |
| 结构化输出 | ✅ | ✅ |
| 命名空间隔离原语 | ✅ | ✅ |
| 托管供应商网络（无需上游密钥） | — | ✅ |
| 开放模型价格折扣 | — | ✅ |
| 团队席位与工作区级访问控制 | — | ✅ |
| 托管可观测性控制台 | — | ✅ |
| 托管计费（统一钱包，按请求计费） | — | ✅ |
| 托管端点 SLA | — | ✅ |
| 优先支持 | — | ✅ |
| Agent 自主支付市场 | — | ✅ |

## 各选项的适用场景

**自托管**适合以下情况：

- 你已有供应商 API 密钥，希望完全掌控流量走向。
- 你运行本地或私有模型，数据不能离开自己的网络。
- 你有合规或数据驻留要求，流量不能流出自有基础设施。
- 你正在独立原型验证，暂时不需要团队访问控制。

**Cloud** 适合以下情况：

- 你想要统一账户，无需管理上游密钥——一张账单，按请求计费，失败请求不收费。
- 你需要以折扣价使用开放模型，而不必自己注册供应商账号。
- 你在团队中协作，需要工作区隔离、席位管理和托管控制台。
- 你的生产工作负载需要可用性 SLA 和优先支持。

## Cloud 详细增量

### 托管供应商网络

Cloud 的托管供应商网络让你无需注册上游账号或存储 API 密钥即可发起模型请求。目前对应 [托管模型](/docs/get-started/models)——一个账户，按 token 计费，开放模型价格低于官方定价。托管工具与 Agent 已在路线图上。

### 团队工作区

Cloud 账户提供工作区：每个工作区有独立的 API 密钥、路由策略、使用数据和访问控制，成员席位作用于特定工作区。凭证作用域严格隔离——工作区级密钥无法访问其他工作区或管理账单。完整模型请参阅 [Cloud 工作区](/docs/features/namespaces)。

OSS 命名空间隔离原语（在自托管与 Cloud 下均可用）请参阅 [命名空间](/docs/features/namespaces)。

### 托管可观测性

Cloud 控制台呈现每个工作区的请求历史、消费和用量明细，无需你自己搭建任何基础设施。自托管二进制若有自己的可观测性栈，同样通过 OTLP 导出相同数据。

### 计费与 SLA

Cloud 提供托管计费（统一钱包，按请求计费，失败请求不收费）以及托管端点的可用性 SLA。自托管不收取软件许可费，自有基础设施也无 SLA 承诺。

## 将 Cloud 附加到自托管二进制

Cloud 不是另一个二进制——它是你附加的一个账户：

```bash
bitrouter auth login
# 在浏览器中登录并选择一个工作区。
# 你的本地二进制现在可以在 BYOK 密钥之外路由 Cloud 托管的模型。
```

你可以随时添加或移除 Cloud 账户，二进制的自托管能力不受任何影响。

## 下一步

<Cards>
  <Card title="快速开始" href="/docs/get-started/quickstart" description="一分钟内让 agent 经由 BitRouter 路由" />
  <Card title="Models" href="/docs/get-started/models" description="完整目录、定价与开放模型折扣" />
</Cards>
