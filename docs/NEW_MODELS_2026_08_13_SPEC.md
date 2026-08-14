# DeepSeek V4 Pro 0813, Grok 4.6, and Gemini 3.6 Flash Registry Design

## Goal

Publish three verified models in the BitRouter OSS curated registry using the
canonical IDs exposed by OpenRouter:

- `deepseek/deepseek-v4-pro-0813`
- `x-ai/grok-4.6`
- `google/gemini-3.6-flash`

Only the three first-party model-provider entries are in scope. OpenRouter,
BitRouter Cloud, and every other gateway or third-party provider remain
unchanged.

## Evidence and live verification

OpenRouter's public `GET /api/v1/models` catalog exposes all three canonical
IDs. First-party documentation and live APIs establish the upstream mappings
and model facts:

- DeepSeek's official pricing page identifies the current
  `deepseek-v4-pro` API alias as model version `DeepSeek-V4-Pro-0813`, with a
  1M-token context window, 384K maximum output, tool calling, Responses API,
  Anthropic API, and the published token prices.
- xAI's authenticated model endpoint exposes `grok-4.6`, a 500,000-token
  text-and-image model, with the base and long-context token prices.
- Google's authenticated model endpoint exposes `gemini-3.6-flash`, its input
  and output limits, and its supported generation methods. Google's changelog
  records the model as generally available on 2026-07-21, and its pricing page
  publishes the token rates.

The live checks used API keys injected from the Railway `production`
environment. Their output was restricted to model metadata, response model
versions, and usage; no credential value was printed or persisted. Minimal
generation requests succeeded against all three first-party APIs.

## Canonical models

### DeepSeek V4 Pro 0813

Add `deepseek/deepseek-v4-pro-0813` as a distinct canonical model. Preserve the
existing `deepseek/deepseek-v4-pro` entry as the earlier April revision so the
two releases are never conflated.

Record:

- name `DeepSeek: DeepSeek V4 Pro 0813`;
- description identifying it as the generally available V4 Pro release;
- text input and text output;
- 1,000,000 maximum input tokens and 384,000 maximum output tokens;
- release date `2026-08-13`;
- proprietary/unpublished weights (`open_weights: false`);
- family `deepseek-thinking`.

The release date follows DeepSeek's official `0813` model-version label; the
documentation does not yet carry a separate release-note entry for this update.
Do not infer a knowledge cutoff because neither DeepSeek nor OpenRouter
currently publishes one for this revision.

### Grok 4.6

Add `x-ai/grok-4.6` with:

- name `xAI: Grok 4.6`;
- a concise description of its coding, reasoning, and agentic positioning;
- text and image input with text output;
- 500,000 maximum input tokens;
- release date `2026-08-12`;
- February 2026 knowledge cutoff;
- proprietary weights and family `grok`.

Omit a maximum-output field because xAI's first-party model metadata does not
publish a separate output limit.

### Gemini 3.6 Flash

Add `google/gemini-3.6-flash` with:

- name `Google: Gemini 3.6 Flash`;
- a concise description of its efficient multimodal agentic positioning;
- text, image, audio, and video input with text output;
- 1,048,576 maximum input tokens and 65,536 maximum output tokens;
- release date `2026-07-21` and March 2026 knowledge cutoff;
- proprietary weights and family `gemini-flash`.

The registry has no `file` or `pdf` modality convention, so document inputs are
represented through the supported media modalities rather than a new modality
token.

## First-party provider mappings

### DeepSeek

Move the existing upstream alias `deepseek-v4-pro` from the April canonical ID
to `deepseek/deepseek-v4-pro-0813`. This mirrors the established V4 Flash 0731
pattern: the first-party alias advances in place while the old canonical model
remains available through providers that still explicitly identify it.

The mapping supports OpenAI Chat Completions, Responses, and Anthropic
protocols; advertises reasoning and tools; and uses the official prices per 1M
tokens:

- input cache miss: USD 0.435;
- input cache hit: USD 0.003625;
- output: USD 0.87.

### xAI

Map `x-ai/grok-4.6` to upstream `grok-4.6`, advertise reasoning and tools, and
record the official base prices per 1M tokens:

- input: USD 2.00;
- cached input: USD 0.50;
- output: USD 6.00.

xAI doubles all three rates at 200,000 tokens and above. The current registry
pricing schema cannot represent context tiers, so retain the provider file's
existing family-level comment and store the base tier, consistent with the
other Grok 4 mappings.

### Google Gemini API

Map `google/gemini-3.6-flash` to upstream `gemini-3.6-flash`, advertise
reasoning and tools, and record the official prices per 1M tokens:

- input: USD 1.50;
- cached input: USD 0.15;
- output, including thinking tokens: USD 7.50.

## Generated artifacts and verification

Rebuild and commit `dist/registry/models.json` and
`dist/registry/providers.json`. Because this change is confined to registry
data, repository policy explicitly says not to add model-specific Rust tests.
Verify with:

```sh
cargo run -p dist-helper -- registry validate
cargo run -p dist-helper -- registry build
cargo run -p dist-helper -- check
```

Inspect the generated JSON to confirm that all three canonical entries exist,
that only the three first-party providers gained or moved mappings, and that
the old DeepSeek canonical entry remains present.

## Non-goals

- Do not modify OpenRouter, BitRouter Cloud, Vertex AI, opencode, or any other
  non-first-party provider mapping.
- Do not run a BitRouter Cloud end-to-end request.
- Do not repoint third-party `deepseek-v4-pro` aliases without explicit evidence
  that those providers advanced to the 0813 revision.
- Do not remove or rewrite the earlier unversioned DeepSeek canonical model.
- Do not add unrelated registry sync or Rust source changes.
