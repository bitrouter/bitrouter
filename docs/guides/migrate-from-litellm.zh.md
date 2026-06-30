---
title: 从 LiteLLM 迁移
description: 将 LiteLLM SDK 或代理迁移到 BitRouter —— 本地二进制、托管云，或两者并用。
sourceHash: 374ade959220237745e7bd565e3c56fbe9c79914947aea588f4507ef86fb5121
---

# 从 LiteLLM 迁移到 BitRouter

LiteLLM 是用于统一访问 100+ LLM 提供商的 Python SDK 与自托管代理。BitRouter 从另一个角度解决同一问题：一个**面向 Agent 的代理**，提供单一 OpenAI 兼容接口 —— 既可作为本地二进制运行（BYOK，无需基础设施），也可通过托管端点支持 Agent 自主支付。本指南面向工作负载已从后端服务转向 Agent 运行时、希望使用更精简、Agent 优先接口的团队。

## 为什么要迁移？

| | LiteLLM Proxy | BitRouter |
|---|---|---|
| **运行时** | Python | Rust（单一静态二进制） |
| **生产依赖** | Postgres + Redis + Docker/K8s | 无 |
| **部署模式** | 仅自托管 | 本地二进制**或**托管（`cloud.bitrouter.ai`）—— OpenAI 兼容端点相同 |
| **Agent 身份与支付** | 无 | x402 / MPP 自主支付（云模式） |
| **设计重心** | 一体化 LLM 网关：管理 UI、虚拟密钥、预算，再附加 Agent 网关 | Agent 优先代理：MCP / ACP / 技能、Agent 防火墙、Agent 支付即核心接口 |
| **Agent 协议面** | MCP、A2A、Skills、CLI —— 附加在横向网关之上 | MCP、ACP、Skills、CLI —— 即产品本身，而非附加 |
| **许可证** | MIT（SDK）/ 付费企业版 | 全栈 Apache 2.0 |

## 两个值得强调的差异

### 1. 云端与本地共享同一接口

LiteLLM 的代理需要你自己运维。BitRouter 的**托管云与本地二进制对外暴露同一个 OpenAI 兼容端点** —— 开发期可在本地启动，生产期切换到 `cloud.bitrouter.ai`（反之亦然），客户端代码不需要修改。CLI、向导和 Agent Skills 在两种模式下都能使用；在设置 TUI 中按一个键即可切换。两种流程详见[快速开始](/docs/get-started/quickstart)。

当你希望 Agent *按请求付费*、而你无需为其分配密钥时，这一点尤其重要 —— 云模式支持 [x402 / MPP 自主支付](/docs/features/payment)，LiteLLM 中没有对应能力。

### 2. Agent 原生，而非一体化

LiteLLM 已在其横向 LLM 网关之上推出 MCP、A2A、Skills 与 CLI —— 同时还附带虚拟密钥、团队预算、消费仪表板、管理 UI 一整套。BitRouter 反其道而行：Agent 原语*即*产品，团队管理那一套保持极简。BitRouter 接口暴露的能力：

- [MCP 网关](/docs/concepts/tools) —— 代理 MCP 服务器，让 Agent 跨主机发现工具。
- [ACP 网关](/docs/concepts/agents) —— 一等支持 Claude Code、Codex、OpenCode 等使用的 Agent Client Protocol。
- [Guardrails](/docs/features/guardrails) —— 代理跳点上的正则规则，原位对匹配内容脱敏或拦截。
- [云端追踪](/docs/features/opentelemetry#cloud-activity-hosted) —— 内置消费与请求追踪，无需外部收集器。
- Agent Skills 网关（即将推出）—— 按技能而非模型名称安装与路由能力。
- [Headless CLI](/docs/concepts/cli) —— TUI 向导与可脚本化命令,用于配置与运维。
- [Agent 身份与支付](/docs/features/payment) —— x402 / MPP,让 Agent 无需你为其分配密钥即可按请求付费。LiteLLM 中没有对应能力。

如果你依赖 LiteLLM 的团队管理 UI、虚拟密钥与按用户预算,LiteLLM 仍是更合适的选择。如果你在构建 Agent —— 尤其是需要自主支付的 Agent —— BitRouter 是更合适的选择。

## 迁移路径

### 从 LiteLLM Python SDK 迁移

LiteLLM 作为库的用法，可以在标准 OpenAI SDK 上换成 BitRouter 的 base URL：

<Tabs items={['迁移前（LiteLLM SDK）', '迁移后（BitRouter）']}>
<Tab value="迁移前（LiteLLM SDK）">
```python
from litellm import completion

response = completion(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "你好"}],
    api_key="sk-...",
)
```
</Tab>
<Tab value="迁移后（BitRouter）">
```python
import openai

# 本地：`bitrouter`（通过环境变量 BYOK）—— 见 /docs/get-started/quickstart
# 云端：base_url="https://cloud.bitrouter.ai/v1"，api_key=$BITROUTER_API_KEY
client = openai.OpenAI(
    base_url="http://127.0.0.1:4356/v1",
    api_key="not-used-in-local-byok",
)

response = client.chat.completions.create(
    model="openai/gpt-4o",
    messages=[{"role": "user", "content": "你好"}],
)
```
</Tab>
</Tabs>

你以前用 `litellm.Router` 配置的回退与提供商选择，现在迁移到 BitRouter 的[路由预设](/docs/features/presets)与[模型回退规则](/docs/features/model-fallback) —— 一次声明，而非每次调用都写。

### 从 LiteLLM Proxy 迁移

代理迁移把 Python 进程 + Postgres + Redis 替换成一个二进制。安装并启动：

<Tabs items={['迁移前（LiteLLM Proxy）', '迁移后（BitRouter 本地）']}>
<Tab value="迁移前（LiteLLM Proxy）">
```bash
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=pass postgres
docker run -d -p 6379:6379 redis
pip install 'litellm[proxy]'
litellm --config config.yaml --port 8000
```
</Tab>
<Tab value="迁移后（BitRouter 本地）">
```bash
# 安装方式（curl、npm、brew 或 cargo —— 详见快速开始）
curl -fsSL https://bitrouter.ai/install.sh | sh

# 设置提供商密钥；BitRouter 启动时自动识别
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...

# 交互式向导（云或本地）；默认本地监听 :4356
bitrouter
```
</Tab>
</Tabs>

如果你想完全跳过本地代理，可以直接让客户端指向 `https://cloud.bitrouter.ai/v1`，使用 BitRouter API 密钥 —— 无二进制，无基础设施，端点形态完全一致。

## 功能映射

| LiteLLM 概念 | BitRouter 等价物 | 文档 |
|---|---|---|
| `config.yaml` 中的 `model_list` | 提供商密钥 + 路由预设 | [Presets](/docs/features/presets) |
| `router_settings`（重试、回退） | 模型回退规则 | [Model fallback](/docs/features/model-fallback) |
| `routing_strategy`（least-busy、latency） | 提供商选择 | [Provider selection](/docs/features/provider-selection) |
| `cache`（Redis/DynamoDB 后端） | 代理内未内置 —— 如需，请在应用 / 边缘层处理 | — |
| 虚拟密钥 + 预算 + 管理 UI | Workspace 密钥（云）；环境变量密钥（本地） | [BYOK](/docs/features/byok)、[Workspaces](/docs/features/namespaces) |
| Guardrails / PII / 内容过滤 | 代理跳点上的 Agent 防火墙 | [Guardrails](/docs/features/guardrails) |
| Callbacks（Langfuse、Datadog 等） | 内置消费与请求日志；OTLP 导出 | [OpenTelemetry](/docs/features/opentelemetry) |
| MCP Gateway | MCP 网关 | [MCP](/docs/concepts/tools) |
| A2A Agent Gateway | ACP 网关 | [ACP](/docs/concepts/agents) |
| Skills Gateway / `/skills` 端点 | Skills 网关 + [agentskills.io](https://agentskills.io) 注册表 | [Agent Skills](/docs/concepts/tools) |
| LiteLLM Proxy CLI | `bitrouter` CLI / TUI | [Headless CLI](/docs/concepts/cli) |
| —（无对应） | Agent 自主支付（x402 / MPP） | [Payment](/docs/features/payment) |

## BitRouter 有意未提供的能力

为了让预期更准确：BitRouter 没有内置可与 LiteLLM Enterprise 对齐的团队/用户预算管理 UI、API 化的虚拟密钥生成或消费分析仪表板。云模式下的 Workspace 密钥隔离与本地模式下的环境变量密钥能覆盖常见需求，但如果你的迁移依赖于在代理内部按用户配额执行的虚拟密钥，请提前规划这一缺口，或在这些工作负载上继续使用 LiteLLM。

## 迁移清单

<Callout type="info">
**迁移前**
- [ ] 列出你实际使用的提供商与模型（其余可跳过）
- [ ] 记录所有自定义 callback / 中间件 —— 看看是否有 [guardrail 规则](/docs/features/guardrails)能覆盖
- [ ] 决定使用云、本地或两者（它们共享端点）
</Callout>

<Callout type="success">
**迁移**
- [ ] 安装 BitRouter CLI（[快速开始](/docs/get-started/quickstart)）
- [ ] 导出提供商密钥，或在云端控制台粘贴（sealed-box 加密）
- [ ] 将客户端 `base_url` 更新为 `http://127.0.0.1:4356/v1`（本地）或 `https://cloud.bitrouter.ai/v1`（云）
- [ ] 发送示例请求进行验证
- [ ] 如本地方案足够，停用 Postgres / Redis
</Callout>

## 下一步

<Cards>
  <Card title="快速开始" href="/docs/get-started/quickstart" description="一分钟内在本地或云端运行 BitRouter" />
  <Card title="对比" href="/docs/get-started/comparison" description="与 OpenRouter、LiteLLM 及通用网关的并排对比" />
  <Card title="Agent 功能" href="/docs/concepts/tools" description="MCP、ACP、技能、Agent 防火墙、x402 支付" />
  <Card title="API 参考" href="/docs/reference" description="OpenAI 与 Anthropic 兼容端点" />
</Cards>

## 获取帮助

- **Discord**：[加入社区](https://discord.gg/G3zVrZDa5C)获取迁移支持
- **GitHub**：[提交 issue](https://github.com/bitrouter/bitrouter/issues)
- **邮件**：contact@bitrouter.ai 获取企业迁移支持
