---
title: Presets
description: Save a named @preset per namespace — a reusable bundle of base model, system prompt, params, and routing rules you invoke inline with @name.
sourceHash: 90a1b5a2af25e5fe3408474b108b5de98300d306800452813d8c56ab31f27d29
---

A **preset** is a named, reusable routing configuration you save once on a namespace and invoke inline by putting `@<name>` in the `model` field. Where a [model variant](/docs/features/model-variants) (`:cost`) only re-ranks providers for one request, a preset can also **substitute the base model**, **prepend a system prompt**, **set default generation params**, and **restrict which providers are eligible** — all behind a single short token.

Like a variant, the token lives in the `model` string itself, so it needs no body fields and no SDK — it works the same on the OpenAI, Anthropic, and Google surfaces. A request that uses `@fast` looks exactly like any other request; the preset is resolved server-side before routing.

## Invoking a preset

Put `@<name>` where you would normally put a model id. The grammar is `@<name>[/<base-model>][:<profile>]`:

| `model` value | Resolves to |
| --- | --- |
| `@fast` | The preset `fast`; its saved base model and overrides apply. |
| `@fast:cost` | The preset `fast`, with the [`:cost` variant](/docs/features/model-variants) overriding the preset's own `sort`. |
| `@fast/openai/gpt-5` | The preset `fast`, but routed to `openai/gpt-5` instead of the preset's saved model. |

A bare model id with no leading `@` — `anthropic/claude-sonnet-4.6` — is untouched and routes exactly as it does today. Presets are purely additive.

## What a preset can set

Every field is optional. An empty preset is valid (it just resolves to its base model unchanged).

| Field | Effect |
| --- | --- |
| `model` | The base model to route to (e.g. `openai/gpt-5-mini`). If omitted, the request must supply a base inline (`@name/<model>`). |
| `system_prompt` | A system prompt applied when the request doesn't already set one. |
| `params` | Default generation params (`temperature`, `max_tokens`, `top_p`, …), merged in for keys the request didn't set. |
| `routing.sort` | A default routing profile (`balanced` / `cost` / `latency` / `throughput`) — the same axes as [model variants](/docs/features/model-variants). |
| `routing.only` | A provider allow-list. Routing is restricted to these `provider_name`s. |
| `routing.ignore` | A provider deny-list. These providers are dropped from the chain. |

## Presets are defaults; the request always wins

A preset supplies *defaults*. Anything the caller sets explicitly on the request takes precedence:

- **Base model** — an inline `@name/<model>` (or a body that already names a model) overrides the preset's `model`. If neither the preset nor the request supplies a base, the request is rejected `400`.
- **Profile** — an explicit `:profile` suffix overrides the preset's `routing.sort`; with neither, routing is `balanced`.
- **System prompt** — the preset's `system_prompt` is applied only if the request didn't send one. An explicit system message always wins.
- **Params** — preset params are merged key-by-key, and only for keys the request omitted. A `temperature` in the request body beats the preset's.

## Creating a preset

Presets are scoped to a namespace. Create them in the console under **Settings → Routing Presets**, or with the management API:

```bash
curl -X POST http://127.0.0.1:4356/v1/namespaces/{nsid}/routing-presets \
  -H "Authorization: Bearer $BRK_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "fast",
    "model": "openai/gpt-5-mini",
    "system_prompt": "Be terse.",
    "params": { "temperature": 0.1 },
    "routing": { "sort": "latency", "only": ["openai"] }
  }'
```

Then invoke it from any inference surface:

```bash
curl http://127.0.0.1:4356/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "@fast",
    "messages": [{"role": "user", "content": "Summarize this in one line."}]
  }'
```

The full CRUD surface — `list`, `get`, `create`, `update`, `delete`, plus `disable`/`enable` — is documented under [Management API](/docs/reference/management/listRoutingPresets). Reading presets needs the `routing_preset:read` scope; creating or changing them needs `routing_preset:write`.

<Callout type="info">
**`name` is the `@token`.** A preset name must match `[A-Za-z0-9_-]+` (the same character set the `@name` grammar accepts), so a name like `my-fast_v2` is fine but `my preset` is rejected at create time — a name you could never invoke is never stored.
</Callout>

## Enabling and disabling

A preset can be disabled without deleting it (`POST …/routing-presets/{id}/disable`, re-enable with `/enable`, or toggle it in the console). A disabled preset is treated as if it doesn't exist: invoking its `@name` returns the same `400` as an unknown preset, while the definition is preserved for when you switch it back on.

## Presets never change authorization

Resolution happens *before* policy enforcement, and a preset can only ever **narrow** what a key could already do — never widen it:

- [Guardrail](/docs/features/guardrails) model allow/deny lists and BYOK rules judge the **resolved base model**, so a preset that substitutes `openai/gpt-5` is checked exactly as if you had asked for `openai/gpt-5` directly. A preset can't smuggle a request past a model denylist.
- `routing.only` / `routing.ignore` can only *remove* providers from the eligible set — they can never add a provider the request wasn't already allowed to reach. [BYOK](/docs/features/byok) providers still rank ahead of platform ones.
- Billing is unchanged — you pay the selected provider's rate for the resolved base model.

## Errors

| Condition | Result |
| --- | --- |
| `@name` is unknown or disabled in the namespace | `400` (distinct from an unknown-model `404`) |
| The preset has no `model` and the request supplied no base | `400` |
| `routing.only` / `routing.ignore` leave no eligible providers | `400` (no providers available under the preset's constraints) |
| At create/update: invalid `name`, a `routing.sort` that isn't a known profile, or a `params` key that collides with a transport control (`model` / `messages` / `stream`) | `400` |

## Presets vs. model variants

The two features overlap deliberately — reach for whichever fits:

- A [**model variant**](/docs/features/model-variants) (`openai/gpt-4o:cost`) is anonymous and zero-setup: it re-ranks providers along one axis for a single request and nothing else.
- A **preset** (`@fast`) is named and saved: it captures a base model, a prompt, params, and provider constraints once, so callers invoke a tested configuration by name instead of repeating it.

They compose — `@fast:cost` applies the preset and then overrides its routing profile with the inline variant.
