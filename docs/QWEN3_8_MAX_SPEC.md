# Qwen3.8 Max Registry Design

## Goal

Publish the generally available Qwen3.8 Max model in the BitRouter OSS
registry. The canonical model must represent the formal `qwen3.8-max`
release, not the earlier Token Plan-only `qwen3.8-max-preview` alias.

## Upstream evidence

The official QwenCloud model page now exposes `qwen3.8-max` as a
2.4-trillion-parameter multimodal MoE flagship. It accepts text, image, and
video input, produces text, has a 1,000,000-token context window, and supports
up to 131,072 output tokens. Its published international price is USD 2.00 per
million uncached input tokens and USD 6.00 per million output tokens, with USD
0.25 cache reads and USD 2.50 cache writes.

Alibaba Cloud's China pricing and cache documentation also lists
`qwen3.8-max`. The base CNY 12 input / CNY 36 output prices are converted at
approximately 7.2 CNY/USD. Explicit cache reads are billed at 10% of standard
input and cache creation at 125%.

The live OpenRouter and opencode go catalogs expose the formal model as
`qwen/qwen3.8-max` and `qwen3.8-max`, respectively. No Qwen3.8 entry exists in
the current BitRouter registry.

## Canonical model

Add `qwen/qwen3.8-max` with:

- name `Qwen: Qwen3.8 Max`;
- text, image, and video input with text output;
- 1,000,000 maximum input tokens and 131,072 maximum output tokens;
- release date `2026-08-03`;
- proprietary weights and family `qwen3.8`;
- a concise description of the 2.4T MoE, multimodal, long-horizon agent model.

Do not add `qwen/qwen3.8-max-preview`. The preview is a temporary Token Plan
model and is explicitly outside this change.

## Provider mappings

Add only providers with current public evidence for the formal model:

| Provider | Upstream model ID | Pricing (USD / 1M tokens) |
| --- | --- | --- |
| Alibaba Cloud Model Studio (International) | `qwen3.8-max` | input 2.00, cache read 0.25, cache write 2.50, output 6.00 |
| Alibaba Cloud Model Studio (China) | `qwen3.8-max` | input 1.667, cache read 0.167, cache write 2.083, output 5.00 |
| OpenRouter | `qwen/qwen3.8-max` | input 2.00, cache read 0.25, cache write 2.50, output 6.00 |
| opencode go | `qwen3.8-max` | subscription; no token price |

Alibaba and OpenRouter inherit their existing OpenAI, Responses, and
Anthropic protocol declarations. opencode go inherits its OpenAI protocol.

Do not add the model to Alibaba Coding Plan: official documentation excludes
Qwen3.8 from Coding Plan and its live catalog does not list the model. Do not
introduce new Alibaba Token Plan providers in this model-scoped change.

## Generated artifacts and regression coverage

- Rebuild `dist/registry/models.json` and `dist/registry/providers.json`.
- Add a repository-registry regression test that asserts the complete canonical
  metadata, absence of a preview canonical, and the exact four provider
  mappings and prices.
- Run registry validation and generated-artifact checks.
- Because the regression test changes Rust source, run formatting, Clippy, and
  the full all-features test suite required by the repository.

## Non-goals

- Do not add or repoint `qwen3.8-max-preview`.
- Do not create Alibaba Token Plan provider definitions.
- Do not infer availability for providers whose public catalogs omit the
  formal model.
- Do not modify Qwen3.7 or unrelated model/provider entries.
