# bitrouter-chainlink (demo)

OpenAI/Anthropic/Gemini-compatible access to Chainlink Confidential Inference
(TEE-backed, async submit-then-poll) via BitRouter. Hackathon demo; off by
default, isolated from cloud/registry.

## Run

```bash
export CHAINLINK_CONFIDENTIAL_API_KEY=<key>

# Foreground (logs to stdout):
cargo run -p bitrouter --features chainlink-demo -- \
    serve -c crates/bitrouter-chainlink/examples/chainlink.yaml

# Background daemon:
cargo run -p bitrouter --features chainlink-demo -- \
    start -c crates/bitrouter-chainlink/examples/chainlink.yaml
```

## Smoke test (chat-completions)

```bash
curl localhost:4356/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gemma4",
    "messages": [{"role":"user","content":"Say hi in five words."}]
  }'
```

## Smoke test (Anthropic messages)

```bash
curl localhost:4356/v1/messages \
  -H 'content-type: application/json' \
  -d '{
    "model": "gemma4",
    "max_tokens": 64,
    "messages": [{"role":"user","content":"Say hi in five words."}]
  }'
```

## Config

The example config lives at `crates/bitrouter-chainlink/examples/chainlink.yaml`.
Key fields:

| Field | Value |
|---|---|
| `api_base` | `https://confidential-ai-dev-preview.cldev.cloud` |
| `api_protocol` | `chainlink_confidential` |
| `api_key` | `${CHAINLINK_CONFIDENTIAL_API_KEY}` |
| Models | `gemma4`, `qwen3.6` |

## Notes

- The `chainlink-demo` feature is required at build time and is excluded from the
  default feature set, the provider registry, and cloud deployments.
- The Chainlink executor uses an async submit-then-poll pattern: `POST /v1/inference`
  returns a job id, which is then polled at `GET /v1/inference/{id}` until
  `status == "completed"` or an error is returned.
- No live key is required to build; the executor is compiled in only when the
  feature flag is set.
