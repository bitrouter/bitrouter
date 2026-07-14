---
title: Call BitRouter Cloud APIs from the CLI
description: Use a gh api-style command for models, Chat Completions, Messages, Responses, and Google Generative AI with OAuth or API-key login.
---

`bitrouter cloud api` sends authenticated requests directly to BitRouter Cloud. It reuses the credential from `bitrouter cloud login`, so scripts do not need to move secrets between separate config files or start the local daemon.

## Sign in

Use browser OAuth on a developer workstation, or pass an existing `brk_` key in CI:

```bash
bitrouter cloud login
bitrouter cloud login --api-key "$BITROUTER_API_KEY"
bitrouter cloud whoami
```

Both forms write `$XDG_DATA_HOME/bitrouter/account-credentials.json` with mode `0600` on Unix. `whoami` reports `authentication: oauth` or `authentication: api_key` without printing the bearer. API-key login performs no network request.

<Callout type="warning">
Pass only relative endpoints such as `/v1/models`. Absolute URLs, scheme-relative paths, and fragments are rejected; redirect following is disabled so the stored credential never leaves its login origin.
</Callout>

## List models

The default method is `GET` when no fields or input body are present:

```bash
bitrouter cloud api /v1/models
```

Add query parameters with an explicit `GET`:

```bash
bitrouter cloud api /v1/models -X GET -F owned=true -F limit=25
```

## Chat Completions

Save an OpenAI-compatible request:

```json
{
  "model": "openai/gpt-5",
  "messages": [
    { "role": "user", "content": "Explain speculative decoding." }
  ],
  "stream": true
}
```

Then send its exact bytes:

```bash
bitrouter cloud api /v1/chat/completions --input chat.json
```

## Messages

The Anthropic-compatible Messages route uses the same stored credential:

```json
{
  "model": "anthropic/claude-sonnet-4-6",
  "max_tokens": 512,
  "messages": [
    { "role": "user", "content": "Explain speculative decoding." }
  ]
}
```

```bash
bitrouter cloud api /v1/messages --input messages.json
```

## Responses

Responses requests can come from a file or fields:

```json
{
  "model": "openai/gpt-5",
  "input": "Explain speculative decoding.",
  "stream": true
}
```

```bash
bitrouter cloud api /v1/responses --input responses.json
bitrouter cloud api /v1/responses \
  -f model=openai/gpt-5 \
  -f input="Explain speculative decoding." \
  -F stream=true
```

## Generate Content

Google Generative AI uses the model action in the path:

```json
{
  "contents": [
    {
      "role": "user",
      "parts": [{ "text": "Explain speculative decoding." }]
    }
  ]
}
```

```bash
bitrouter cloud api \
  /v1beta/models/google/gemini-2.5-flash:generateContent \
  --input generate-content.json
```

## Stream Generate Content

Use the streaming action for SSE output:

```bash
bitrouter cloud api \
  /v1beta/models/google/gemini-2.5-flash:streamGenerateContent \
  --input generate-content.json
```

When stdout is redirected, response chunks are written as they arrive and every byte is preserved. This makes normal pipelines safe:

```bash
bitrouter cloud api /v1/chat/completions --input chat.json > events.sse
```

## Build requests with fields

`-f/--raw-field` always creates a string. `-F/--field` converts `true`, `false`, `null`, and integers to JSON values and reads `@file` or `@-` as a string. Bracket syntax builds nested objects and arrays:

```bash
bitrouter cloud api /v1/chat/completions \
  -f model=openai/gpt-5 \
  -f 'messages[][role]=user' \
  -f 'messages[][content]=Hello' \
  -F stream=true \
  -F max_tokens=256
```

Fields make the implicit method `POST` and form a JSON body. With explicit `GET`, or whenever `--input` owns the body, fields become query parameters. Only one field or `--input -` may consume stdin.

## Control headers and output

```bash
bitrouter cloud api /v1/models -H 'Accept: application/json'
bitrouter cloud api /v1/models --include
bitrouter cloud api /v1/models --silent
bitrouter cloud api /v1/models --verbose
```

`-H/--header` is repeatable. Supplying `Authorization` overrides the stored bearer for that request. `--include` writes the status line and headers before the body. `--silent` drains the body without printing it. `--verbose` writes redacted request/response metadata to stderr and conflicts with `--silent`.

Interactive JSON is pretty-printed. Non-TTY JSON, arbitrary binary bodies, and SSE are not reformatted. For HTTP 4xx/5xx, the response body remains on stdout, a diagnostic goes to stderr, and the command exits non-zero.

## First-release scope

The first release intentionally omits GitHub-specific `gh api` features: GraphQL, pagination/slurp, `--jq`, Go templates, cache, hostname selection, preview headers, and placeholder expansion. It supports arbitrary relative BitRouter Cloud endpoints, while the documented and tested initial matrix is:

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/messages`
- `POST /v1/responses`
- `POST /v1beta/models/{model}:generateContent`
- `POST /v1beta/models/{model}:streamGenerateContent`

For the complete flag reference, see the [CLI reference](/docs/concepts/cli).
