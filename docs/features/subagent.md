---
title: Sub-agent
description: Delegate a self-contained task to a focused worker model mid-generation — the worker sees only what you give it and returns its final result.
sourceHash: 2a7c24c88c1728c4c0f164738dfd58c7f424e0dbfc5d545bab0f92d71ed6cb3a
---

**Sub-agent** is a server tool: BitRouter runs it mid-generation instead of handing the call back to your client. The calling model hands off a self-contained task — `task_name` and `task_description` — to a focused worker model (typically a cheaper or faster one), which works in isolation and returns only its final result. Use it to fan out grunt work without spending the main model's context on it.

## Quick start

Enable the worker by declaring `subagent` on the request `tools` array. The declaration pins the worker model and instructions; at call time the model fills in the task:

<Tabs items={['Responses API', 'Anthropic Messages']}>
<Tab value="Responses API">

```bash
curl https://api.bitrouter.ai/v1/responses \
  -H "Authorization: Bearer $BITROUTER_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-opus-4.8",
    "input": "Summarize each of these 20 support tickets, then group them by theme.",
    "tools": [
      {
        "type": "subagent",
        "model": "anthropic/claude-haiku-4.5",
        "instructions": "You are a concise summarizer. Return one sentence per ticket."
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
    "model": "anthropic/claude-opus-4.8",
    "max_tokens": 4096,
    "messages": [
      {"role": "user", "content": "Summarize each of these 20 support tickets, then group them by theme."}
    ],
    "tools": [
      {
        "type": "subagent",
        "name": "subagent",
        "model": "anthropic/claude-haiku-4.5",
        "instructions": "You are a concise summarizer. Return one sentence per ticket."
      }
    ]
  }'
```

</Tab>
</Tabs>

<Callout type="warning">
**Protocol support.** The `subagent` declaration works on the **OpenAI Responses API** (`/v1/responses`) and the **Anthropic Messages API** (`/v1/messages`), which both carry server tools on the wire. The **Chat Completions** `tools` array only accepts `{type:"function"}` entries and cannot carry the declaration.
</Callout>

## How it works

When the calling model invokes the tool, it supplies:

| Argument | Description |
| --- | --- |
| `task_name` | A short identifier for the task. |
| `task_description` | The full, self-contained task: context, inputs, and expected output. |

The worker model sees **only** the `task_description` — no other conversation context — runs the task, and its final result is returned to the calling model. Because the worker is isolated, the calling model must put everything the worker needs into the description.

## Configuration

Declared on the `tools`-array entry:

| Field | Type | Description |
| --- | --- | --- |
| `model` | string | The worker model. Defaults to the parent request model. |
| `instructions` | string | System instructions for the worker. |
| `tools` | array | Provider server tools the worker may use (e.g. web search), in provider-namespaced declaration form. |

<Callout type="info">
**Bounded by the server-tool loop.** A sub-agent turn runs inside a bounded loop — a default of 10 tool rounds, 30s per tool, and a 120s total budget per turn.
</Callout>

## On Cloud

Sub-agent is enabled and managed on **BitRouter Cloud**, with per-run cost for the worker calls visible in the request log. Self-hosters enable it with `server_tools.subagent: true`; it is then advertised per-request only when the caller declares it.

## See also

- [Advisor](/docs/features/advisor) — consult a stronger model for guidance instead of delegating a task
- [Fusion](/docs/features/fusion) — deliberate across a panel of models on one prompt
- [Provider selection](/docs/features/provider-selection) — control which provider serves the worker model
