---
title: Overview
description: 把任意模型来源接入 BitRouter——Claude 或 Codex 订阅、OpenRouter 密钥，或你自己托管的模型。
sourceHash: 7be29fb8987333cd00928957202cd1263655a3e44205aef251f6c3d22aebfc2b
---

**模型来源（model source）**指的是你的 token 实际从何而来。BitRouter 把每个来源都统一到一个端点和一个[registry](/docs/concepts/models)背后，于是 Agent 只用 `provider/model` 形式的 id 来寻址模型，完全感知不到订阅、聚合器和你壁橱里的 GPU 之间的区别。

按认证方式划分，来源有三种形态：

<Cards>
  <Card title="Claude subscription" href="/docs/integrations/claude-subscription" description="走你的 Claude Pro/Max 套餐——OAuth，无需密钥" />
  <Card title="Codex subscription" href="/docs/integrations/codex-subscription" description="经 Codex 后端走你的 ChatGPT 套餐——OAuth" />
  <Card title="OpenRouter" href="/docs/integrations/openrouter" description="自带 OpenRouter 密钥——一个聚合器，数百个模型" />
  <Card title="Ollama" href="/docs/integrations/ollama" description="本地运行开放模型 · :11434 · 无需密钥" />
  <Card title="vLLM" href="/docs/integrations/vllm" description="高吞吐 GPU 服务 · :8000" />
</Cards>

## 来源是如何接入的

| 来源 | 认证方式 | 在哪里配置 |
| --- | --- | --- |
| **订阅**（Claude、Codex） | OAuth——用你套餐自带的登录 | `bitrouter providers login <provider>`（无需密钥，无需 yaml） |
| **聚合器 / 托管服务**（OpenRouter 等） | 自带 API 密钥 | `bitrouter.yaml` 中的一个 provider 块 |
| **自托管**（Ollama、vLLM） | 通常无需认证——回环地址 | `bitrouter.yaml` 中的一个 provider 块 |

订阅完全跳过 `bitrouter.yaml`：`bitrouter providers login claude-code` 或 `bitrouter providers login openai-codex` 会保存可刷新的订阅凭据，供 BitRouter 在请求时附加使用。其余所有来源都是一个 provider 块——一个 `api_base`、一个可选的 `api_key`，以及该来源提供的模型列表。

```yaml
# bitrouter.yaml
providers:
  openrouter:                          # an id you pick (or a built-in id)
    api_base: https://openrouter.ai/api/v1
    api_key: ${OPENROUTER_API_KEY}     # resolved from the environment at load
    api_protocol:
      - "*": chat_completions          # upstream wire format
    models:
      - id: openai/gpt-4o
```

`providers` 是一个以你自选 id 为键的映射；`api_base` 是该来源的 base URL；`api_protocol` 是上游的传输协议格式（任何 OpenAI 兼容的服务用 `chat_completions`——也是推断出的默认值）；每条 `models` 记录代表该来源提供的一个模型。

## 生成配置脚手架

生成一个初始的 `bitrouter.yaml`（默认写到 `./bitrouter.yaml`）：

```bash
bitrouter init
```

这会写出一份带注释、`skip_auth: true` 的配置，可直接填入 provider 块。用 `-c <path>` 可以写到其他位置，然后按你所用来源的对应文档页填好该块。（订阅类来源不需要任何配置——只需 `bitrouter providers login <provider>`。）

## 启动 BitRouter 并发送请求

来源接好之后——provider 块已就位，或订阅已登录——启动代理。它默认监听 `127.0.0.1:4356`：

```bash
bitrouter
```

然后用你配置中的模型 id（换成你自己的 provider id / model）访问这个 OpenAI 兼容端点：

```bash
curl http://localhost:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "openrouter:openai/gpt-4o",
    "messages": [{"role": "user", "content": "Hello from BitRouter"}]
  }'
```

裸模型名同样可用——BitRouter 会自动级联到声明了该模型的活跃来源。带供应商前缀的形式（`openrouter:openai/gpt-4o`）则会把请求锁定到那一个确切的来源。

<Callout type="info">
**自由混合多个来源。** 声明一个[虚拟模型](/docs/features/model-fallback)，把本地来源列在端点列表第一位、托管来源列在第二位：请求默认免费跑在你自己的硬件上，出错或过载时自动失败回退到托管模型——一个模型名，自动故障转移。
</Callout>

关于这背后的概念——供应商选择、失败回退与 registry 探测——详见[模型](/docs/concepts/models)。要自行托管模型，请参阅本地服务器相关的集成文档：[Ollama](/docs/integrations/ollama) 与 [vLLM](/docs/integrations/vllm)。
