---
title: Advisor
description: Consult a stronger model mid-task — the calling model asks a self-contained question and gets back advice to act on, without handing off the whole job.
sourceHash: 1654b07a58e9e24b42977f9f0ba02d4fa5a60097ad0862b2195f09c13dfac363
---

**Advisor** is a server tool: BitRouter runs it mid-generation instead of handing the call back to your client. When the calling model hits something it's unsure about, it asks a stronger advisor model a self-contained question and gets back advice to act on — without delegating the whole task. Use it to let a fast, cheap main model escalate just the hard sub-questions to a stronger model.

## Quick start

Enable the advisor by declaring `advisor` on the request `tools` array. The declaration pins the advisor model and instructions; at call time the model asks its question:

<Tabs items={['Responses API', 'Anthropic Messages']}>
<Tab value="Responses API">

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-haiku-4.5",
    "input": "Refactor this auth module. Ask the advisor before changing the token-rotation logic.",
    "tools": [
      {
        "type": "advisor",
        "model": "anthropic/claude-opus-4.8",
        "instructions": "You are a senior security engineer. Be precise about auth edge cases."
      }
    ]
  }'
```

</Tab>
<Tab value="Anthropic Messages">

```bash
curl https://api.bitrouter.ai/v1/messages \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-haiku-4.5",
    "max_tokens": 4096,
    "messages": [
      {"role": "user", "content": "Refactor this auth module. Ask the advisor before changing the token-rotation logic."}
    ],
    "tools": [
      {
        "type": "advisor",
        "name": "advisor",
        "model": "anthropic/claude-opus-4.8",
        "instructions": "You are a senior security engineer. Be precise about auth edge cases."
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**Protocol support.** The `advisor` declaration works on the **OpenAI Responses API** (`/v1/responses`) and the **Anthropic Messages API** (`/v1/messages`), which both carry server tools on the wire. The **Chat Completions** `tools` array only accepts `{type:"function"}` entries and cannot carry the declaration.
</Callout>

## How it works

When the calling model invokes the tool, it supplies:

| Argument | Description |
| --- | --- |
| `prompt` | A clear, self-contained question for the advisor. |
| `model` | Optional per-call override of the advisor model. |

The advisor answers and its advice is returned to the calling model, which then continues the task. Unlike [Sub-agent](/docs/features/subagent), the advisor doesn't take over the work — it just answers a question.

## Configuration

Declared on the `tools`-array entry:

| Field | Type | Description |
| --- | --- | --- |
| `model` | string | The advisor model. An absent value falls back to the per-call `model` arg, then the parent request model. |
| `instructions` | string | System instructions for the advisor. |
| `tools` | array | Provider server tools the advisor may use (e.g. web search), in provider-namespaced declaration form. |

<Callout type="info">
**Bounded by the server-tool loop.** An advisor turn runs inside a bounded loop — a default of 10 tool rounds, 30s per tool, and a 120s total budget per turn.
</Callout>

## On Cloud

Advisor is enabled and managed on **BitRouter Cloud**, with per-run cost for the advisor calls visible in the request log. Self-hosters enable it with `server_tools.advisor: true`; it is then advertised per-request only when the caller declares it.

## See also

- [Sub-agent](/docs/features/subagent) — delegate a self-contained task instead of asking a question
- [Fusion](/docs/features/fusion) — deliberate across a panel of models on one prompt
- [Model variants](/docs/features/model-variants) — address a stronger variant of a model as the advisor
