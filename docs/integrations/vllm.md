---
title: vLLM
description: Register a local vLLM server as a BitRouter provider — high-throughput GPU serving behind an OpenAI-compatible API.
sourceHash: 0df180b6a21a047cbc136a0560b2a46035de88ae4222a18eb00b99109eaa0048
---

[vLLM](https://docs.vllm.ai) is a high-throughput inference engine for serving models on your own GPUs. Its `vllm serve` command exposes an OpenAI-compatible API at `http://localhost:8000/v1`, which BitRouter fronts as one provider block.

## Prerequisites

- BitRouter installed, with a `bitrouter.yaml` ([scaffold one](/docs/integrations/models#scaffold-a-config) with `bitrouter init`).
- vLLM serving a model:

  ```bash
  vllm serve meta-llama/Llama-3.1-8B-Instruct    # default port 8000
  ```

## Add vLLM to BitRouter

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

The `models` id must match the name vLLM serves. By default that's the full Hugging Face repo id; pass `--served-model-name my-model` to `vllm serve` to alias it to something shorter, then use that alias here.

<Callout type="info">
**Optional auth.** vLLM is keyless by default. If you launched it with `--api-key <token>` (or `VLLM_API_KEY`), add `api_key: ${VLLM_API_KEY}` to the provider block — it resolves from the environment at load time.
</Callout>

<Callout type="warn">
**Port clash with Unsloth.** vLLM and Unsloth Studio both default to `:8000`. If you run both, start one on another port (`vllm serve … --port 8001`) and update `api_base`.
</Callout>

## Route to it

```bash
bitrouter route vllm:meta-llama/Llama-3.1-8B-Instruct
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request).

## Learn more

- [vLLM — OpenAI-compatible server](https://docs.vllm.ai/en/stable/serving/openai_compatible_server/)
- [Model fallback](/docs/features/model-fallback) · [Provider selection](/docs/features/provider-selection)
