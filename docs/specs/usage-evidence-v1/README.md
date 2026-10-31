# Execution usage evidence v1

Status: draft. Specification revision: 1. Wire `schema_version`: 1.

This contract publishes bounded technical observations from an execution. It
does not define prices, payable quantities, charges or financial settlement.
MUST and MUST NOT describe required observable behavior. The existing strict
v1 execution receipt remains unchanged; this is a separate document and API.

## Discovery and access

`GET /control/v1/tenancy/usage-evidence/capabilities` requires authentication
and `Bitrouter-Installation-Id`. The response identifies the installation,
schema version, byte/frame/quantity bounds, estimator capture, owned child
linkage and generic server-tool capture. Discovery MUST NOT grant authority.
Consumers MUST check the advertised facilities rather than infer support from
a version number or the presence of a DTO in a client library.

`GET /control/v1/tenants/{tenant_id}/executions/{execution_id}/usage-evidence`
uses the existing receipt-read authority and installation binding. The producer
MUST authorize both the tenant and requested execution. Responses MUST use
`Cache-Control: no-store`. Installation, tenant and execution identities MUST
match the addressed resource. Correlation IDs are not execution identities and
MUST NOT be used to merge evidence or deduplicate inference.

Evidence MUST be bound to the immutable terminal receipt and committed under
the same durable publication gate. A missing or corrupt required document MUST
fail closed; it MUST NOT be presented as a historical execution without capture.
For an older receipt that predates capture, the producer returns
`capture: not_captured`, empty attempts/frames/child IDs, null parent and estimate,
`estimation: not_captured`, `truncated: false` and `dropped_frames: 0`.
For a new execution whose persisted admission seed records capture version 1,
recovery without the original observations MUST instead persist a document with
`capture: unavailable`. Its empty observations assert no quantity or successful
completion. The producer MUST NOT label this loss as historical `not_captured`.
Historical admission seeds retain their original serialization and absence
semantics; unsupported capture versions fail validation.

## Observations

An execution document contains these identities, `capture`, `extractor_version`,
`estimation`, required nullable `estimate`, required nullable
`parent_execution_id`, `child_execution_ids`, `attempts`, `frames`, `truncated`
and `dropped_frames`. All fields are required. `extractor_version` is 1.

Attempts MUST retain receipt dispatch order and the exact attempt ID, target,
outcome and `terminal_observed`. Target includes provider/model/account and
credential revision identifiers; it MUST NOT expose credential material.
Unknown execution outcomes remain unknown even if some usage was observed.

Each frame contains a zero-based contiguous `sequence`, `attempt_id`, original
`protocol`, fixed `source`, `semantics`, `quantities`, `unsupported` and `invalid`.
Repeated frames MUST be retained in observation order. Numeric observations MUST
come from the original upstream payload, before protocol conversion or a
zero-filled canonical usage projection. Arbitrary upstream keys, strings,
response IDs, prompts, generated content and tool arguments/results MUST NOT be
copied into this document.

| Protocol | Eligible sources | Semantics in extractor v1 |
| --- | --- | --- |
| Chat Completions | JSON, HTTP error usage, accepted usage chunk | Snapshot |
| Responses | JSON, HTTP error usage, accepted terminal usage | Snapshot |
| Messages | JSON, HTTP error usage, message start, message delta | Snapshot |
| Decisions | JSON, HTTP error usage | Unknown |

Snapshot fields replace observations of the same dimension; they MUST NOT be
summed across frames. A Messages `message_delta` usage update is cumulative even
though the event name includes delta. Version 1 does not qualify additive delta
sources. Consumers MUST NOT invent an arithmetic interpretation for unknown
semantics. See the [Messages streaming specification](https://platform.claude.com/docs/en/build-with-claude/streaming)
and [Chat API reference](https://developers.openai.com/api/reference/resources/chat).

Quantities use closed dimension names and exact nonnegative integers up to
9,007,199,254,740,991. Missing source fields have no quantity entry. Explicit
zero remains zero. An invalid known numeric field has a null value and sets
`invalid`; unknown source fields set `unsupported` without copying their names
or values. Token, cache, reasoning, reported search and reported image/audio
dimensions retain their native units. The producer MUST NOT synthesize search
or media counts from output content or canonical default zeroes.

## Bounds and incomplete evidence

The encoded document is at most 256 KiB, with at most eight attempts, 512 frames
per execution, 128 frames per attempt, 32 quantities per frame and 64 child IDs.
Capture retains an ordered prefix. Once a frame cannot be retained, truncation
is sticky and `dropped_frames` counts discarded observations with saturation at
the exact integer maximum. `truncated` MUST equal `dropped_frames > 0`.
Truncation, unsupported fields, invalid numbers and absent completion evidence
MUST NOT be interpreted as complete provider totals.

## Estimates and owned children

`estimation` distinguishes `not_captured`, `not_used` and `captured`.
`estimate` is nonnull only for an estimator that actually ran. Algorithm
`stream_character_ceiling`, version 1, divides Unicode scalar counts by four
and rounds upward. The estimate records prompt/output character counts, resulting
input/output token estimates, `characters_per_token: 4`, coverage
`prompt_text_and_observed_text_reasoning`, and trigger `completed` or
`disconnected`. Coverage excludes media, unseen output and executed tools.
This is an execution-stream estimate, not a provider-reported per-attempt amount
or a total across fallback and child executions.

Parent/child identifiers MUST originate from the execution owner's admission
path, never from caller correlation or provider-supplied IDs. Each child retains
its own receipt and evidence. Its quantities MUST NOT be silently folded into
the parent's frames. Child linkage support MUST be advertised. A producer that
does not execute a generic server tool loop MUST NOT advertise that capability.

## Compatibility

All envelopes are closed: unknown or duplicate fields, invalid enum values,
missing required nullable fields, unsupported versions and out-of-bound values
MUST be rejected. Wire/schema validation does not establish execution ownership
or receipt consistency. The packaged schemas are
[execution evidence](../../../crates/bitrouter-ai/src/tenancy/usage_evidence/execution.schema.json)
and [capabilities](../../../crates/bitrouter-ai/src/tenancy/usage_evidence/capabilities.schema.json).
See [conformance](conformance.md) for behavioral requirements.
