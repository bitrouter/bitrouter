---
title: OpenRouter
description: 把你的 OpenRouter 密钥接入 BitRouter——一把聚合器密钥，背后是数百个模型，统一纳入注册表。
sourceHash: 177d9a2dbc0eff17580a51b6a4fa1440253c8a787a604e413b84a60c32a659dd
---

[OpenRouter](https://openrouter.ai) 是一个 OpenAI 兼容的聚合器，用一把密钥就能访问数百个模型。BitRouter 的注册表包含 `openrouter` 供应商，因此接入你的密钥只需在 `bitrouter.yaml` 中加一个区块——此后 OpenRouter 的模型目录就会并入你其余的[注册表](/docs/concepts/models)，并叠加供应商选择与回退能力。

<Callout type="info">
**想完全迁移出 OpenRouter？** 如果你想*替换*而非通过它来路由，请参见[从 OpenRouter 迁移](/docs/guides/migrate-from-openrouter)——只是换一下 base URL 和密钥。
</Callout>

## 前置条件

- 已安装 BitRouter，并有一份 `bitrouter.yaml`（用 `bitrouter init` [搭建一份](/docs/integrations/models#scaffold-a-config)）。
- 一个来自 [openrouter.ai/keys](https://openrouter.ai/keys) 的 OpenRouter API 密钥，导出为 `OPENROUTER_API_KEY`。

## 把 OpenRouter 接入 BitRouter

`openrouter` 注册表条目带有正确的 `api_base` 和 Bearer 鉴权默认值——你只需提供密钥和想要的模型：

```yaml
# bitrouter.yaml
providers:
  openrouter:
    api_key: ${OPENROUTER_API_KEY}     # resolved from the environment at load
    models:
      - id: openai/gpt-4o
      - id: anthropic/claude-sonnet-4-6
      - id: google/gemini-2.5-pro
```

每个 `models` id 都是一个 [OpenRouter 模型 id](https://openrouter.ai/models)。base URL（`https://openrouter.ai/api/v1`）和 `chat_completions` 协议都是该供应商的默认值，无需另行设置。

<Callout type="info">
**归属头（Attribution headers）。** 不会注入 OpenRouter 可选的 `HTTP-Referer` / `X-Title` 排行榜头。如果你需要它们，可以在 `bitrouter.yaml` 中以按供应商的请求头覆盖方式添加。
</Callout>

## 路由到它

```bash
bitrouter route openrouter:openai/gpt-4o
```

然后[启动 BitRouter 并发送请求](/docs/integrations/models#start-bitrouter-and-send-a-request)。用带供应商前缀的 id `openrouter:openai/gpt-4o` 把请求固定到 OpenRouter，或用裸模型名让 BitRouter 在各来源间级联。

## 了解更多

- [OpenRouter —— API 参考](https://openrouter.ai/docs/api-reference/overview)
- [供应商选择](/docs/features/provider-selection) · [模型回退](/docs/features/model-fallback)
