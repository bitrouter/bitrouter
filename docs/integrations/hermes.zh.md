---
title: Hermes
description: 通过设置自定义模型端点，让 Nous Research 的 Hermes Agent 经由 BitRouter 路由。
sourceHash: e2af206e53c098a3a99612591e241869057d46d6cdbf5418686ed8d8d59da7f0
---

Hermes 是 Nous Research 出品的自我改进 Agent。它支持自定义的 OpenAI 兼容模型端点，因此你可以把它指向 BitRouter，让它的模型调用路由到整个[registry](/docs/concepts/models)。

## 前置条件

- BitRouter 正在运行——本地代理 `http://127.0.0.1:4356`，或 [BitRouter Cloud](/docs/get-started/configuration)（`https://api.bitrouter.ai`）。
- 已安装 Hermes。安装方法见 [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent)。

## 把 Hermes 指向 BitRouter

在 `~/.hermes/config.yaml` 中，把模型 provider 设为 `custom`，并把 base URL 设为 BitRouter 的 `/v1` 根路径：

```yaml
model:
  provider: custom
  base_url: http://127.0.0.1:4356/v1
  api_key: local-placeholder    # any value for the local proxy
  model: openai/gpt-4o
```

也可以通过 CLI 设置同样的内容：

```bash
hermes config set model.base_url http://127.0.0.1:4356/v1
```

<Callout type="info">
若使用 **Cloud**，把 `base_url` 设为 `https://api.bitrouter.ai/v1`，并使用你的 BitRouter 密钥。本地代理会忽略 `api_key`，所以填个占位符即可。具体字段名请以你所用版本的 [Hermes 配置文档](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/configuration.md)为准。
</Callout>

<Callout type="info">
**想专门跑在 Hermes 模型上？** Nous 的 Hermes 模型同样在 registry 中——把 `nousresearch/hermes-4-405b`（或任意带供应商前缀的 Hermes id）设为模型，BitRouter 就会路由到承载它的供应商。
</Callout>

## 选择模型

`model` 字段接受任意 `provider/model` 形式的 registry id，也可以附带 `:cost` / `:latency` 变体后缀。详见[模型](/docs/concepts/models)。

## 了解更多

- [Hermes —— 配置](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/configuration.md)
- [失败回退](/docs/features/model-fallback) · [供应商选择](/docs/features/provider-selection)
