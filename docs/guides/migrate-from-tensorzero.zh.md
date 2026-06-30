---
title: 从 TensorZero 迁移
description: 将 TensorZero 网关迁移到 BitRouter —— 抛开依赖 ClickHouse 的 LLMOps 栈，换成单一、面向 Agent 的二进制，本地或托管皆可。
sourceHash: 0b6c985670a25c13b4f0d958728639e8b06480832b28005456e89d61a7a85e53
---

# 从 TensorZero 迁移到 BitRouter

[TensorZero](https://github.com/tensorzero/tensorzero) 是一个开源的 **LLMOps 平台** —— 一个 Rust 网关，捆绑了可观测性、评估、优化（微调、自动化提示工程）与实验能力，全部由 ClickHouse 数据仓库支撑，并以 GitOps 风格的 `tensorzero.toml` 驱动。BitRouter 有意只解决其中更窄的一块：一个**面向 Agent 的代理**，提供单一 OpenAI 兼容接口，既可作为本地二进制运行（BYOK，无需基础设施），也可通过托管端点支持 Agent 自主支付。

本指南面向那些主要把 TensorZero 当作**网关**使用（统一访问提供商、路由、重试、回退）、希望在不运维数据库的前提下使用更精简、Agent 优先接口的团队。如果你依赖 TensorZero 的优化与实验闭环，请先阅读下文的「BitRouter 有意不提供的能力」一节再做决定。

## 为什么要迁移？

| | TensorZero | BitRouter |
|---|---|---|
| **运行时** | Rust 网关 | Rust（单一静态二进制） |
| **生产依赖** | ClickHouse（可观测性存储）+ Docker/Compose | 无 |
| **配置** | `tensorzero.toml` —— functions、variants、models、providers | 提供商密钥 + 路由预设；无需维护 schema |
| **部署模式** | 仅自托管 | 本地二进制**或**托管（`api.bitrouter.ai`）—— OpenAI 兼容端点相同 |
| **设计重心** | 完整 LLMOps 闭环：网关 + 可观测性 + 评估 + 优化 + 实验 | Agent 优先代理：MCP / ACP / 技能、Agent 防火墙、Agent 支付 |
| **Agent 协议面** | 无（网关面向提供商） | MCP、ACP、Skills、CLI —— 即产品本身，而非附加 |
| **Agent 身份与支付** | 无 | x402 / MPP 自主支付（云模式） |
| **许可证** | Apache 2.0 | Apache 2.0 |

## 两个值得强调的差异

### 1. 是代理，不是平台

TensorZero 围绕一个反馈闭环构建：路由推理、把每次调用及其指标存入 ClickHouse，再基于这段历史进行评估、微调与 A/B 测试。如果你真的在跑这套闭环，它很强大 —— 这也是 TensorZero 需要一个数据库、以及一份由 functions 与 variants 组成的类型化配置的原因。

BitRouter 有意止步于网关。没有 `tensorzero.toml` 要维护，没有 ClickHouse 要运维 —— 你设置提供商密钥，按 `provider/model` id 路由。[OpenTelemetry](/docs/features/opentelemetry) 为每次请求提供 span 与指标（与 TensorZero 同样的 OTLP 导出），但它们发往你已经在运行的后端，而非由代理托管的数据仓库。

### 2. 面向 Agent，且云端与本地共用同一接口

TensorZero 的网关是面向提供商的 —— 它统一上游。BitRouter 补上**面向 Agent**的另一半：用于工具的 [MCP 网关](/docs/concepts/tools)、用于 Agent 身份与调度的 [ACP 网关](/docs/concepts/agents)、一个[服务端工具循环](/docs/features/server-tools)，以及 [Agent 支付](/docs/features/payment) —— 让 Agent 无需你为其预置密钥即可按请求付费。

而且**托管云与本地二进制暴露同一个 OpenAI 兼容端点** —— 开发时在本地启动，生产时指向 `api.bitrouter.ai`（或反过来），无需改动客户端代码。两种流程都见[快速开始](/docs/get-started/quickstart)。

## 迁移路径

### 从 OpenAI 兼容客户端迁移

如果你的应用已经在对接 TensorZero 的 OpenAI 兼容端点，迁移就是换一个 base URL，再把 `tensorzero::…` 模型前缀换成普通的 `provider/model` id：

<Tabs items={['迁移前（TensorZero）', '迁移后（BitRouter）']}>
<Tab value="迁移前（TensorZero）">
```python
import openai

client = openai.OpenAI(
    base_url="http://localhost:3000/openai/v1",
    api_key="not-used",
)

response = client.chat.completions.create(
    # TensorZero 模型 / function 引用
    model="tensorzero::model_name::openai::gpt-4o-mini",
    messages=[{"role": "user", "content": "Hello"}],
)
```
</Tab>
<Tab value="迁移后（BitRouter）">
```python
import openai

# 本地：`bitrouter`（通过环境变量 BYOK）—— 见 /docs/get-started/quickstart
# 云端：base_url="https://api.bitrouter.ai/v1", api_key=$BITROUTER_API_KEY
client = openai.OpenAI(
    base_url="http://127.0.0.1:4356/v1",
    api_key="not-used-in-local-byok",
)

response = client.chat.completions.create(
    model="openai/gpt-4o-mini",
    messages=[{"role": "user", "content": "Hello"}],
)
```
</Tab>
</Tabs>

TensorZero 的 **function**（一个命名的提示模板 + schema，含一个或多个 variant）没有单一对应物 —— 路由那一半映射到 BitRouter，模板/变体那一半留在你的应用里，或迁移到[预设](/docs/features/presets)。见下方的「功能映射」一节。

### 从网关 + ClickHouse 栈迁移

基础设施迁移把 Compose 栈（网关容器加 ClickHouse）替换成一个二进制、零数据库：

<Tabs items={['迁移前（TensorZero Compose）', '迁移后（BitRouter 本地）']}>
<Tab value="迁移前（TensorZero Compose）">
```bash
# docker-compose.yml 运行网关 + 一个 ClickHouse 仓库
export OPENAI_API_KEY=sk-...
export TENSORZERO_CLICKHOUSE_URL=http://chuser:chpass@clickhouse:8123/tensorzero
docker compose up   # 网关在 :3000，ClickHouse 在 :8123
# 配置从 ./config/tensorzero.toml 挂载
```
</Tab>
<Tab value="迁移后（BitRouter 本地）">
```bash
# 安装（curl、npm、brew 或 cargo —— 见快速开始）
curl -fsSL https://bitrouter.ai/install.sh | sh

# 设置提供商密钥；BitRouter 启动时自动检测
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# 交互式向导（云或本地）；默认本地在 :4356 提供服务
bitrouter
```
</Tab>
</Tabs>

若想完全跳过本地代理，把客户端指向 `https://api.bitrouter.ai/v1` 并带上 BitRouter API 密钥即可 —— 无二进制、无 ClickHouse、无基础设施。

## 功能映射

| TensorZero 概念 | BitRouter 对应 | 文档 |
|---|---|---|
| `tensorzero.toml` 中的 `[models.*]` + `[models.*.providers.*]` | 提供商密钥（自动检测）+ 模型注册表 | [BYOK](/docs/features/byok)、[模型](/docs/concepts/models) |
| `[functions.*]`（命名提示 + schema） | 留在应用侧，或一个路由[预设](/docs/features/presets) | [预设](/docs/features/presets) |
| `[functions.*.variants.*]`（按变体指定模型） | 路由预设变体 / 模型 id | [预设](/docs/features/presets) |
| `routing` / `retries` / `fallbacks` | 模型回退规则 | [模型回退](/docs/features/model-fallback) |
| 跨提供商 `load_balancing` | 提供商选择 | [提供商选择](/docs/features/provider-selection) |
| OpenAI 兼容 `/openai/v1` 端点 | OpenAI 兼容 `/v1` 端点 | [API 参考](/docs/reference/openai-compatible/createChatCompletion) |
| 原生 `POST /inference` 端点 | OpenAI、Anthropic 与 Google 兼容协议 | [API 参考](/docs/reference) |
| ClickHouse 可观测性 + UI | 发往你自己后端的 OTLP trace 与指标，或 Cloud 上的托管 Activity | [OpenTelemetry](/docs/features/opentelemetry)、[云端追踪](/docs/features/opentelemetry#cloud-activity-hosted) |
| OpenTelemetry（OTLP）导出 | OpenTelemetry（OTLP）导出 | [OpenTelemetry](/docs/features/opentelemetry) |
| 内置结构化输出（JSON functions） | 跨所有提供商的结构化输出 | [结构化输出](/docs/features/structured-outputs) |
| ——（无对应） | MCP / ACP / Skills Agent 网关 | [工具](/docs/concepts/tools)、[Agents](/docs/concepts/agents) |
| ——（无对应） | Agent 自主支付（x402 / MPP） | [支付](/docs/features/payment) |

## BitRouter 有意不提供的能力

为了坦诚地设定预期：BitRouter 是一个网关，而非 LLMOps 平台。它**不**提供 TensorZero 的优化与实验闭环 —— 没有内置的监督微调、RLHF、自动化提示工程、动态上下文学习、推理级或工作流级评估，也没有自适应 A/B 测试，更没有 ClickHouse 支撑、用于把历史推理在不同提示下重放的 UI。

如果你的工作流依赖那套封闭反馈闭环 —— 采集推理与反馈、评估、优化、实验 —— 那么 TensorZero 更合适，这些工作负载应继续留在它上面。当你从 TensorZero 真正需要的是*网关*、而缺的那一半是 *Agent 基础设施*（工具/Agent 网关、Agent 防火墙、按请求自主付费）时，迁移到 BitRouter 才是对的选择。

## 迁移清单

<Callout type="info">
**迁移前**
- [ ] 列出你真正经 TensorZero 路由的提供商与模型（其余略过）
- [ ] 区分网关用途与平台用途 —— 你用的是评估 / 优化 / 实验，还是仅仅路由？
- [ ] 记下 `[functions.*]` 的模板与 schema 在哪里；规划是保留在应用侧还是迁到[预设](/docs/features/presets)
- [ ] 决定云端还是本地（或两者并用 —— 它们共用端点）
</Callout>

<Callout type="success">
**迁移中**
- [ ] 安装 BitRouter CLI（[快速开始](/docs/get-started/quickstart)）
- [ ] 导出提供商密钥，或粘贴进云端控制台（sealed-box 加密）
- [ ] 将客户端 `base_url` 改为 `http://127.0.0.1:4356/v1`（本地）或 `https://api.bitrouter.ai/v1`（云端）
- [ ] 把 `tensorzero::…` 模型字符串替换为 `provider/model` id
- [ ] 把 OTLP 导出重新指向你的后端（[OpenTelemetry](/docs/features/opentelemetry)）
- [ ] 用一个示例请求验证
- [ ] 若不再需要平台闭环，下线网关容器与 ClickHouse
</Callout>

## 下一步

<Cards>
  <Card title="快速开始" href="/docs/get-started/quickstart" description="一分钟内在本地或云端运行 BitRouter" />
  <Card title="对比" href="/docs/get-started/comparison" description="与 OpenRouter、LiteLLM 及通用网关的并列对比" />
  <Card title="Agent 功能" href="/docs/concepts/tools" description="MCP、ACP、技能、Agent 防火墙、x402 支付" />
  <Card title="API 参考" href="/docs/reference" description="OpenAI 与 Anthropic 兼容端点" />
</Cards>

## 获取帮助

- **Discord**：[加入社区](https://discord.gg/G3zVrZDa5C)获取迁移支持
- **GitHub**：[提交 issue](https://github.com/bitrouter/bitrouter/issues)
- **邮箱**：contact@bitrouter.ai 获取企业迁移协助
