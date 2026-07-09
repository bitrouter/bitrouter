---
title: Ollama
description: Register a local Ollama server as a BitRouter provider — OpenAI-compatible, no key, runs on localhost.
sourceHash: a32a5ae4c339a293eeb7a50f97e3569db8c0a1a58a384bf5e51ac377425db224
---

[Ollama](https://ollama.com) is the easiest way to run open models locally. It serves an OpenAI-compatible API at `http://localhost:11434/v1`, so it drops into `bitrouter.yaml` as one provider block.

## Prerequisites

- BitRouter installed, with a `bitrouter.yaml` ([scaffold one](/docs/integrations/models#scaffold-a-config) with `bitrouter init`).
- Ollama running, with a model pulled:

  ```bash
  ollama serve &          # default port 11434
  ollama pull llama3.1
  ```

## Add Ollama to BitRouter

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

Each `models` entry is a name you've pulled with `ollama pull`. List what's available with `ollama list`.

<Callout type="info">
**No API key.** Ollama accepts anonymous loopback requests, so the block has no `api_key`. (Its own SDK examples pass a dummy `"ollama"` key only because the OpenAI client library demands a non-empty string — BitRouter doesn't.)
</Callout>

## Route to it

```bash
bitrouter route ollama:llama3.1
```

Then [start BitRouter and send a request](/docs/integrations/models#start-bitrouter-and-send-a-request). Use the provider-qualified id `ollama:llama3.1` to pin the request, or the bare `llama3.1` to let BitRouter cascade.

## Learn more

- [Ollama — OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility)
- [Model fallback](/docs/features/model-fallback) — fail over from local Ollama to a hosted model.
