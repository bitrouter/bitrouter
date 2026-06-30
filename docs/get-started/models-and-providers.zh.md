---
title: Models & Providers
description: 托管的 BitRouter Cloud 供应商——一个账户，无需上游密钥——完整的模型目录与定价，并对开放模型自动打折。
sourceHash: 86d87ac9130c04fd5ca161b83bc6f263323dbb88eb5d4be1d45f451a216af27c
---

**BitRouter Cloud 供应商**让 agent 只用一个 BitRouter 账户即可调用下面的任意模型——无需上游供应商密钥，也无需逐个供应商注册。你按这里列出的价格直接向 BitRouter 付费，按请求计费；失败的请求不计费。

```bash
bitrouter auth login    # one-time device-flow sign-in
bitrouter start         # the `bitrouter` provider auto-enables once signed in
```

更想用自己的供应商账户？改用 [BYOK](/docs/features/byok)——你按各供应商的官方价直接向其付费。要运行自己的模型？参见[本地与私有模型](/docs/integrations/models)（免费）。

## 受支持的模型与定价

价格以美元 / **百万 token** 计，并持续从实时目录刷新。开放模型默认以**官方价低 25%** 提供——参见下文[折扣开放模型](#discounted-open-models)。

<ModelsTable />

## 供应商

上述每个模型都由一个或多个**已注册供应商**提供服务。成员资格维护在公开、开源的 [provider-registry](https://github.com/bitrouter/provider-registry) 中——任何人都可以[注册供应商](/docs/guides/register-as-a-provider)。该列表持续从注册表刷新，因此新合并的供应商会在几分钟内出现。

<ProvidersTable />

## 折扣开放模型

BitRouter 为开放模型运营自己的**自托管供应商**，价格比官方定价**低 25%**。你会自动享有该价格——开源项目的开发者还可申请更深的定制折扣。

### 默认立享 25% 折扣

除闭源系列——OpenAI（`gpt-*`）、Anthropic（`claude-*`）、Google（`gemini-*`）、xAI（`grok-*`）——之外的所有模型，都由 BitRouter 的自托管供应商提供，价格比该模型的**官方价低 25%**。

这**无需任何后缀、无需任何配置**。由于自托管供应商是这些模型最便宜的来源，常规路由已经会把你的请求发往那里，并按折扣价计费。（上述四个闭源系列不在自托管供应商上，因此仍按标准价路由到各自的常规上游。）

### 用 `:discount` 固定到自托管供应商

在模型 ID 后追加 `:discount`，即可把该请求**专门路由到 BitRouter 的自托管供应商**：

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "moonshotai/kimi-k2.6:discount",
    "messages": [{"role": "user", "content": "把 Hello 翻译成法语。"}]
  }'
```

该后缀随 `model` 字符串一起传递——无需请求体字段、无需 SDK——在 OpenAI、Anthropic、Google 三套接口（`/v1/messages`、`/v1beta/models/{model}:generateContent`）上行为一致。当你想确保流量落在折扣的自托管供应商上时使用它；账户上的任何定制折扣也在这里生效。

<Callout type="info">
`:discount` 绝不改变鉴权。[Guardrail](/docs/features/guardrails) 白名单与 [BYOK](/docs/features/byok) 规则会把 `moonshotai/kimi-k2.6:discount` 完全当作 `moonshotai/kimi-k2.6` 来判定——该后缀无法放宽或绕过任何策略。
</Callout>

### 面向开源项目的最高 50% 定制折扣

正在 BitRouter 上构建**开源 agent harness** 或其他开源项目？我们为你和你的社区提供**最高 50% 的定制折扣**。

欢迎联系我们安排：

- **邮件** [kelsenliu@bitrouter.ai](mailto:kelsenliu@bitrouter.ai)
- **或预约与创始人的会议：**

<CalInline />
