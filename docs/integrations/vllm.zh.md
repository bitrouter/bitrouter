---
title: vLLM
description: 把本地 vLLM 服务注册为 BitRouter 供应商——在 OpenAI 兼容 API 背后实现高吞吐 GPU 服务。
sourceHash: 0df180b6a21a047cbc136a0560b2a46035de88ae4222a18eb00b99109eaa0048
---

[vLLM](https://docs.vllm.ai) 是一款用于在你自己的 GPU 上提供模型服务的高吞吐推理引擎。它的 `vllm serve` 命令会在 `http://localhost:8000/v1` 暴露一个 OpenAI 兼容 API，BitRouter 将其作为一个供应商区块对接。

## 前置条件

- 已安装 BitRouter，并有一份 `bitrouter.yaml`（用 `bitrouter init` [生成一份脚手架](/docs/integrations/models#scaffold-a-config)）。
- vLLM 正在提供模型服务：

  ```bash
  vllm serve meta-llama/Llama-3.1-8B-Instruct    # 默认端口 8000
  ```

## 将 vLLM 添加到 BitRouter

```yaml
# bitrouter.yaml
providers:
  vllm:
    api_base: http://localhost:8000/v1
    api_protocol:
      - "*": chat_completions
    models:
      - id: meta-llama/Llama-3.1-8B-Instruct
```

`models` 中的 id 必须与 vLLM 提供服务时使用的名称一致。默认情况下，这就是完整的 Hugging Face 仓库 id；可以给 `vllm serve` 传入 `--served-model-name my-model` 将其别名为更短的名称，然后在这里使用该别名。

<Callout type="info">
**可选的鉴权。** vLLM 默认不需要密钥。如果你在启动时带上了 `--api-key <token>`（或设置了 `VLLM_API_KEY`），请在供应商区块中添加 `api_key: ${VLLM_API_KEY}`——它会在加载时从环境变量中解析。
</Callout>

<Callout type="warn">
**与 Unsloth 的端口冲突。** vLLM 与 Unsloth Studio 默认都使用 `:8000`。如果两者都要运行，请把其中一个启动在另一个端口上（`vllm serve … --port 8001`），并相应更新 `api_base`。
</Callout>

## 路由到它

```bash
bitrouter route vllm:meta-llama/Llama-3.1-8B-Instruct
```

然后[启动 BitRouter 并发送请求](/docs/integrations/models#start-bitrouter-and-send-a-request)。

## 了解更多

- [vLLM —— OpenAI 兼容服务器](https://docs.vllm.ai/en/stable/serving/openai_compatible_server/)
- [模型回退](/docs/features/model-fallback) · [供应商选择](/docs/features/provider-selection)
