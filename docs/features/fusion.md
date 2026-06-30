---
title: Fusion
description: Multi-model deliberation — a panel of models answers in parallel, a judge compares their answers, and your model writes the final reply from structured analysis.
sourceHash: 023a1f0a6746d2c7f985aa44b314fa09d8ec26655957ccc70147968d510a7211
---

**Fusion** is a server tool: BitRouter runs it mid-generation instead of handing the call back to your client. A **panel** of models answers your prompt in parallel, a **judge** model compares (not merges) their answers into structured analysis, and the calling model writes the final answer grounded in that analysis. Use it for hard, high-stakes questions where one model's blind spots are worth catching.

## Quick start

The simplest way to use Fusion is the `bitrouter/fusion` **model alias** — set it as your `model` and BitRouter expands it into a deliberation using your deployment's default panel and judge. This works on any protocol, including Chat Completions:

<Tabs items={['Model alias (cURL)', 'Explicit panel (Responses)']}>
<Tab value="Model alias (cURL)">

```bash
curl https://api.bitrouter.ai/v1/chat/completions \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "bitrouter/fusion",
    "messages": [
      {"role": "user", "content": "Should we migrate our event pipeline from Kafka to a managed alternative? Trade-offs."}
    ]
  }'
```

The alias rewrites the request onto a default outer model, attaches the Fusion declaration, and nudges the model to call the tool once. You get back a single, deliberated answer.

</Tab>
<Tab value="Explicit panel (Responses)">

Declare Fusion yourself to pick the exact panel, judge, and (optionally) a dedicated synthesizer. Fusion rides the request `tools` array as a server tool:

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-opus-4.8",
    "input": "Should we migrate our event pipeline from Kafka to a managed alternative?",
    "tools": [
      {
        "type": "fusion",
        "panel": [
          {"model": "anthropic/claude-opus-4.8"},
          {"model": "openai/gpt-5.5"},
          {"model": "google/gemini-3-pro-preview"}
        ],
        "judge": {"model": "anthropic/claude-opus-4.8"}
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**Protocol support.** The explicit `tools`-array declaration works on the **OpenAI Responses API** (`/v1/responses`) and the **Anthropic Messages API** (`/v1/messages`), which both carry server tools on the wire. The **Chat Completions** `tools` array only accepts `{type:"function"}` entries, so declare Fusion there with the **`bitrouter/fusion` model alias** instead.
</Callout>

## How it works

1. **Panel** — every model in the panel answers your prompt in parallel. Each member can be given provider web tools (e.g. web search) to ground its answer.
2. **Judge** — the judge model compares the panel's answers and returns structured analysis: `consensus`, `contradictions`, `partial_coverage`, `unique_insights`, and `blind_spots`. The judge **compares, it does not merge** — it surfaces where the models agree and disagree rather than blending them into mush.
3. **Synthesis** — the calling model (or a dedicated `synthesizer`) reads that analysis and writes the final answer.

## Configuration

Declared on the `tools`-array entry:

| Field | Type | Description |
| --- | --- | --- |
| `panel` | array | One entry per model answering in parallel. Each is `{ "model": string, "tools"?: array }`, where `tools` are provider web tools forwarded to that member. Defaults to a single member on the request model. Capped at **8** members. |
| `judge` | object | `{ "model": string }` — the model that compares the panel answers. Defaults to the request model. |
| `synthesizer` | string | Optional dedicated model to write the final answer. When omitted, the calling model writes it from the returned analysis. |

The deployment-wide defaults that the `bitrouter/fusion` alias expands to — panel, judge, synthesizer, and the web tools forwarded to every member — are set in the `server_tools.fusion` config section. An explicit declaration on a request overrides them.

<Callout type="info">
**Bounded by the server-tool loop.** Like all server tools, a Fusion turn runs inside a bounded loop — a default of 10 tool rounds, 30s per tool, and a 120s total budget per turn. A panel that exceeds the budget terminates with a truncation finish reason.
</Callout>

## On Cloud

Fusion is enabled and managed on **BitRouter Cloud** — a curated default panel and judge back the `bitrouter/fusion` alias out of the box, with per-run cost and the full panel→judge→synthesis chain visible in the request log. Self-hosters enable it by adding a `server_tools.fusion` block to their config (its presence turns on both the `fusion` server tool and the `bitrouter/fusion` alias).

## See also

- [Sub-agent](/docs/features/subagent) — delegate a self-contained task to a single worker model
- [Advisor](/docs/features/advisor) — consult one stronger model mid-task
- [Provider selection](/docs/features/provider-selection) — control which provider serves each panel model
