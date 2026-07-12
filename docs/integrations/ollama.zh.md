---
title: Ollama
description: 把本地 Ollama 服务注册为 BitRouter 供应商——OpenAI 兼容、无需密钥、运行在 localhost。
sourceHash: a32a5ae4c339a293eeb7a50f97e3569db8c0a1a58a384bf5e51ac377425db224
---

[Ollama](https://ollama.com) 是在本地运行开放模型最简单的方式。它在 `http://localhost:11434/v1` 提供 OpenAI 兼容的 API，因此只需在 `bitrouter.yaml` 中加一个供应商区块即可接入。

## 前置条件

- 已安装 BitRouter，并有一份 `bitrouter.yaml`（用 `bitrouter init` [搭建一份](/docs/integrations/models#scaffold-a-config)）。
- Ollama 正在运行，并已拉取一个模型：

  ```bash
  ollama serve &          # default port 11434
  ollama pull llama3.1
  ```

## 把 Ollama 接入 BitRouter

```yaml
# bitrouter.yaml
providers:
  ollama:
    api_base: http://localhost:11434/v1
    api_protocol:
      - "*": chat_completions
    models:
      - id: llama3.1
```

每个 `models` 条目都是你用 `ollama pull` 拉取过的模型名。用 `ollama list` 查看可用列表。

<Callout type="info">
**无需 API 密钥。** Ollama 接受匿名的本地回环请求，因此该区块没有 `api_key`。（它自己的 SDK 示例之所以传一个占位的 `"ollama"` 密钥，只是因为 OpenAI 客户端库要求一个非空字符串——BitRouter 没有这个限制。）
</Callout>

## 路由到它

```bash
bitrouter route ollama:llama3.1
```

然后[启动 BitRouter 并发送请求](/docs/integrations/models#start-bitrouter-and-send-a-request)。用带供应商前缀的 id `ollama:llama3.1` 来固定请求，或用裸 id `llama3.1` 让 BitRouter 进行级联。

## 了解更多

- [Ollama —— OpenAI 兼容性](https://docs.ollama.com/api/openai-compatibility)
- [模型回退](/docs/features/model-fallback) —— 从本地 Ollama 故障转移到托管模型。
