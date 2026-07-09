---
title: Guardrails
description: Named regex rules that block or redact matching content in requests and responses — enforced inside the router, no model call required.
sourceHash: 86d6e83b4e26ff5831ec2aee39c1d5d278b2252e27e7d49a2c496aea163bdbb4
---

A **guardrail** is a named regex rule with an action. BitRouter scans request prompts on the way in and response streams on the way out; when a rule matches, it either **blocks** the request (or aborts the stream) or **redacts** the matched span. Enforcement happens inside the router, on the proxy hop — no extra model call, no external service.

There are **no built-in classifiers** — no PII detector, toxicity model, or content categories. A guardrail does exactly what its regex says, so you bring the patterns that matter for your workload.

## Configure

Rules live under `custom_patterns` in the plugin's config. Each rule has a `name`, a `pattern` (a [Rust regex](https://docs.rs/regex/latest/regex/#syntax)), and an `action`:

```yaml
plugins:
  bitrouter-guardrails:
    custom_patterns:
      - name: ssn
        pattern: '\d{3}-\d{2}-\d{4}'
        action: redact
      - name: provider-key
        pattern: 'sk-[a-zA-Z0-9]+'
        # action omitted → defaults to block
```

Patterns are compiled when the router loads its config — an invalid regex fails fast at startup rather than at request time.

## Two actions

| Action | On a match |
| --- | --- |
| **`block`** *(default)* | Deny the request, or abort the response stream, naming the rule that fired. The default when `action` is omitted — block is the safe fallback. |
| **`redact`** | Replace the matched span with `[REDACTED]` and let the content through. |

## Request vs response

The two sides are deliberately asymmetric:

- **Request (inbound).** Only `block` rules apply — a prompt is allowed or denied, not rewritten. The scan covers the system instruction, every message's text and reasoning, **tool-result output, and tool-call arguments**. Binary parts (images, audio, documents) and citation sources carry no scannable text and are skipped.
- **Response (outbound, streaming).** `redact` rules swap matches for `[REDACTED]` as the stream flows; a `block` match aborts the stream. Streaming is never stalled — each delta is emitted in its own turn.

<Callout type="info">
**Block is checked across streaming chunks; redact is per-chunk.** A `block` pattern split across two streamed deltas is still caught (a short trailing window is kept for detection). A `redact` pattern split *exactly* across a delta boundary is a known gap — block, the security-critical action, is the one guaranteed to span boundaries.
</Callout>

## On Cloud

Everything above runs in the open-source binary off your config file. **BitRouter Cloud** manages the same block/redact rules for you as a per-workspace policy you edit in the console — no config file to deploy. See [Workspaces](/docs/features/namespaces).
