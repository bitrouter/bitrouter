# DeepSeek V4 Flash 0731 Registry Design

## Goal

Publish DeepSeek V4 Flash 0731 as a distinct canonical model while preserving
the existing unversioned DeepSeek V4 Flash entry as the preview release.
Routing must never mix the preview and 0731 revisions under one canonical ID.

## Canonical models

- Keep `deepseek/deepseek-v4-flash` unchanged as the preview revision.
- Add `deepseek/deepseek-v4-flash-0731` for the 2026-07-31 official release.
- Record the official release's text-only modalities, 1,000,000-token input
  limit, 384,000-token output limit, May 2025 knowledge cutoff, open weights,
  and `deepseek-flash` family.

## Provider mappings

Only providers whose current public catalog identifies the 0731 revision are
eligible for the new canonical model.

| Provider | Preview mapping | 0731 mapping |
| --- | --- | --- |
| DeepSeek | remove (upstream alias moved) | `deepseek-v4-flash` |
| opencode zen | remove (upstream alias moved) | `deepseek-v4-flash` |
| opencode go | remove (upstream alias moved) | `deepseek-v4-flash` |
| Alibaba Cloud Model Studio (China) | keep `deepseek-v4-flash` | `deepseek-v4-flash-0731` |
| Ambient | keep `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-flash-0731` |
| Atlas Cloud | keep `deepseek-ai/deepseek-v4-flash` | `deepseek-ai/deepseek-v4-flash-0731` |
| Novita AI | keep `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-flash-0731` |
| OpenRouter | keep `deepseek/deepseek-v4-flash` | `deepseek/deepseek-v4-flash-0731` |
| Baidu Qianfan International | keep `deepseek-v4-flash` | `deepseek-v4-flash-0731` |

All other provider mappings remain on the preview canonical model until their
upstream catalog explicitly confirms the 0731 revision.

Provider pricing and protocol support come from each provider's current public
catalog. In particular, the first-party DeepSeek mapping advertises its native
Responses API support, and OpenRouter uses the price published for its explicit
0731 model rather than copying the preview price.

## Sync durability

DeepSeek and the two opencode providers reuse an unversioned upstream model ID
for the 0731 revision. The additive registry sync currently deduplicates only by
canonical ID, so it would reattach that same upstream ID to the preview
canonical model after the source mapping is moved.

The sync planner will also deduplicate by `provider_model_id`. A provider model
already mapped to any canonical ID cannot be automatically attached a second
time under another canonical ID. Validation will reject duplicate upstream IDs
inside a provider file. This preserves explicit maintainer mappings and prevents
one upstream endpoint from representing two supposedly different canonical
models.

## Generated artifacts and verification

- Rebuild `dist/registry/models.json` and `dist/registry/providers.json`.
- Add regression coverage for canonical metadata, exact provider mappings, and
  the preview/0731 separation.
- Add sync-planner and validation coverage for duplicate upstream IDs.
- Run registry validation, catalog build/check, targeted dist-helper tests,
  formatting, linting, and the repository test suite required for Rust source
  changes.

## Non-goals

- Do not infer 0731 availability for providers without explicit evidence.
- Do not repoint the preview canonical ID to the official release.
- Do not change DeepSeek V4 Pro or unrelated provider catalogs.

