# Model history compatibility audit

This document inventories the baseline SDK rules that affect conversation
history when requests and responses pass through the four model protocols.
It supports the proposed `bitrouter-ai` extraction. It describes existing
behavior, including information loss and synthesized content; it does not
approve that behavior as the future contract or introduce new repair rules.

The SDK has local replay guards and protocol conversions, but no shared
history-repair pass equivalent to pi-ai's `transformMessages`. Some losses
occur during inbound parsing, before a canonical `Prompt` exists. Others
occur when rendering an upstream request or a client response. Those stages
must be distinguished during extraction.

The implementation now lives under `crates/bitrouter-ai`. The baseline tables
below remain historical findings. Batches 1, 13 and 14 in the
[implementation progress](BITROUTER_AI_REFACTOR_PROGRESS.md) corrected tool-result
cardinality, Responses input/result order and completed Responses reasoning
output retention. Batch 14 also retains Responses reasoning on ingress and adds
content-free diagnostics/preflight for native reasoning and structured outputs.
Batch 15 rejects initial four-wire ingress omissions with structured reports before
returning a partial Prompt, including unknown blocks/items and unsigned Messages thinking.
Batch 16 adds initial structural projection refusal for history/declaration omissions,
provider execution/MCP identity loss, tool-result media/file IDs, approvals and
cross-wire native continuity tokens. Batch 17 additionally refuses omitted explicit
strict flags, structured-output metadata, filenames and error/denial status, and
unclassified Gemini tool-schema cleanup. A bounded nullable/single-type rewrite
remains admitted. Batch 18 preserves text-only array boundaries and refuses initial
argument substitution, result-wrapper, ordering and nested-attribute losses. JSON
encoding into native string slots retains its value; the original typed source remains
authoritative. Batch 19 records those JSON/schema equivalent effects and exposes
eligible-candidate observations separately from exclusions and provider attempts.
These changes do not rewrite the baseline tables below as current
behavior. Authority proofs, remaining attributes/schema/boundary losses and native
stream fields remain unresolved.

## Scope and agreed registry boundary

ACP runtime and harness metadata will leave the model/provider registry. This
is an agreed direction, not an implemented removal. This audit does not change
`registry/agents`, `registry/runtimes`, their generated artifacts, or ACP
execution. The follow-up [spec](BITROUTER_AI_REFACTOR_SPEC.md) keeps the catalog
data-driven under the application/ACP integration; exact source and artifact
locations remain to be reviewed.
The ACP official-registry client in `apps/bitrouter/src/agent_registry.rs` is
a separate discovery path and is not implicitly removed by this decision.

The inventory covers ordinary history blocks, reasoning continuity, tool
calls/results, provider tools, citations, and adjacent request compatibility.
It does not enumerate every media codec, finish-reason mapping, or pipeline
hook. Server-tool execution creates new turns; that is separate from repairing
caller-supplied history.

## Evidence and terminology

Source baseline: `d93ed73b`, audited on 2026-10-01. Only this document and the
documentation index were changed by the audit.

Sources below are relative to `crates/bitrouter-sdk/src/language_model/`.
Function and test names are included so the evidence remains searchable after
line numbers change. Test references mean existing assertions were inspected;
this documentation-only audit did not run tests or exercise live providers.
An implementation observation without a listed test is not a claim of missing
coverage across the entire repository.

- **Preserve:** keep a value or carry it through provider metadata.
- **Convert:** change representation, possibly losing distinctions or order.
- **Drop:** omit supplied content or fields.
- **Synthesize:** emit content or identifiers absent from the source wire.
- **Reject:** return an error rather than silently transform the request.

## Reasoning and replay credentials

| Boundary and trigger | Current rule | Consequence | Implementation and inspected tests |
| --- | --- | --- | --- |
| Messages inbound history contains `thinking` or `redacted_thinking` | Keep thinking only with a non-empty string signature; keep redacted thinking only with non-empty string data | Drop unsealed blocks before canonicalization; no conversion to ordinary text | `protocol/messages.rs`: `parse_content`, `is_replayable_reasoning_block`; `messages_parse_request_drops_unsigned_thinking_blocks` |
| Messages reasoning parse/render | Preserve Anthropic signature and redacted payload in the `anthropic` metadata namespace | Signed and redacted blocks can round-trip; namespace presence does not prove validity for a different account/model | `parse_reasoning_metadata`, `render_reasoning_block`; `messages_reasoning_signature_round_trips`, `messages_redacted_thinking_round_trips` |
| Messages output/request render encounters canonical reasoning | Render signed reasoning as thinking, redacted reasoning as redacted thinking, and omit unsigned reasoning; omit reasoning cache-control | Loss of unsigned reasoning text; no same-model provenance check. Hand-built canonical metadata is not validated by the inbound guard | `render_reasoning_block`; `messages_response_drops_unsigned_reasoning`, `messages_cache_control_not_emitted_on_thinking_block` |
| Messages client stream receives reasoning | Buffer text until terminal signature; emit signed block only when signature arrives | Streaming reasoning is delayed; unsigned buffer is discarded | `MessagesStreamEncoder::flush_reasoning_buffer`; `messages_stream_encoder_drops_unsigned_reasoning`, `anthropic_thinking_signature_survives_stream_roundtrip` |
| Generate Content thinking or function-call part has `thoughtSignature` | Preserve and restore it through the `google` namespace | Part-level continuity preserved; no source-model/account comparison | `protocol/generate_content.rs`: `parse_thought_signature`, `apply_thought_signature`, `render_part`; `generate_content_thought_signature_round_trips`, `generate_content_thought_signature_round_trips_on_function_call` |
| Generate Content renders canonical reasoning | Emit `thought: true` and restore a Google signature when present | Unsigned or foreign reasoning is still emitted as thought; no general cross-model text downgrade | `render_part`; implementation observation |
| Chat renders history with reasoning | Concatenate reasoning text into `reasoning_content` | Block boundaries and opaque foreign continuity credentials are not represented on this history wire | `protocol/chat_completions.rs`: `render_message`; implementation observation |
| Responses renders history with reasoning | Omit canonical `Content::Reasoning` from request input | Explicit history replay loses reasoning, even for the same protocol; separate native continuation may retain upstream state | `protocol/responses.rs`: `render_message_items`; implementation observation |
| Responses stream closes a reasoning item | Emit `ReasoningEnd` with `signature: None`; encrypted reasoning replay data is not carried in that slot | Anthropic-style signature round-trip must not be assumed to cover Responses encrypted reasoning | `ResponsesStreamDecoder::decode`; implementation observation |

These are protocol-local rules, not a generalized same-model replay policy.
[`Message`](../crates/bitrouter-sdk/src/model_call/types.rs) currently
contains only `role` and `content`. It has no message-level source provider,
model, account, or completion status. Content metadata retains some native
fields, but does not establish authority to replay them to another target.

## Calls and conversation shape

| Boundary and trigger | Current rule | Consequence | Implementation and inspected tests |
| --- | --- | --- | --- |
| Gemini function call has no ID | Parse its ID as an empty string; outbound `functionCall` uses name/args and does not emit the canonical ID | No global stable-ID assignment or paired remapping | `generate_content.rs`: `parse_parts`, `render_part`; implementation observation |
| Gemini function result has no ID/name | Inbound ID falls back to name; outbound name falls back to `call_id`; emit response ID when distinct and non-empty | Local fallback, not recovery of the originating call's name by scanning history | `parse_parts`, `render_part`; `tool_name_survives_gemini_function_response_round_trip`, `gemini_function_response_carries_call_id_when_distinct` |
| Messages/Gemini render a call whose argument string is invalid JSON | Substitute `{}` | Synthesize empty arguments; do not reject malformed argument text here | `messages.rs`: `render_content_block`; `generate_content.rs`: `render_part`; implementation observation |
| Gemini inbound content contains results and other parts | Partition results into a Tool message, followed by remaining parts in the original role; omit empty partitions | Original interleaving across those partitions is not preserved | `GenerateContentAdapter::parse_request`; implementation observation |
| Chat canonical Tool message contains multiple results | Write each result into the same JSON object's `tool_call_id` and `content` fields | Last result wins; no expansion into one wire message per result. Gemini parsing can produce such a canonical message | `chat_completions.rs`: `render_message`; implementation observation, needs a focused regression check before changing behavior |
| Responses canonical message mixes text/media and standalone calls/results | Collect text/media into one message item inserted before standalone items | Original block interleaving may change; no transcript-wide reordering pass | `responses.rs`: `render_message_items`; implementation observation |

No shared implementation was found that fills missing ordinary function-call
results, normalizes IDs to each provider's restrictions with paired updates,
or removes failed/aborted assistant turns from caller-supplied history.
`FinishReason` belongs to generation results, not to canonical `Message`, so
that status is not automatically available when history is parsed again.
Unknown roles can be rejected; that is input validation, not history repair
(`regression_454_4_unknown_role_is_an_error`).

## Tool results and approval

| Target | Current result conversion | Loss or synthesis | Evidence |
| --- | --- | --- | --- |
| Messages ordinary tool result | Tool-role message becomes user with `tool_result`; retain error flag; JSON becomes a string; multimodal results retain text/images | Non-image result media and provider file IDs are omitted | `messages.rs`: `render_message`, `render_tool_result_content`; `tool_result_error_json_round_trips_through_anthropic`, `tool_result_content_skips_non_image_media_on_anthropic` |
| Chat tool result | Emit `tool_call_id` and string/content-array payload | Error flag and tool name omitted; provider file IDs omitted | `chat_completions.rs`: `render_tool_result_content`, `tool_result_part_to_content`, `render_message`; `tool_result_content_round_trips_through_chat_completions` |
| Responses ordinary tool result | Emit call ID and string/content-array output; preserve custom-output kind in metadata | Error flag and tool name omitted; JSON stringified. File IDs have a native representation | `responses.rs`: `render_message_items`, `render_responses_tool_output`; `tool_result_json_renders_responses_output_as_string`, `tool_result_content_file_id_round_trips_through_responses` |
| Gemini tool result | Preserve object JSON; wrap other JSON/text under `result` | Error distinction omitted; multimodal result collapses to concatenated text, dropping media/file IDs | `generate_content.rs`: `render_part`; `tool_result_text_degrades_to_json_result_on_gemini`, `tool_result_json_round_trips_through_generate_content`; multimodal loss observed in implementation |
| Responses approval handshake | Preserve approval response ID/boolean; omit output-only approval request from input; suppress denial result already represented by approval response | Approval reason omitted from native approval response; paired denial is not emitted twice | `responses.rs`: `render_message_items`; `responses_mcp_approval_response_denied_pairs_execution_denied`, `responses_approval_then_tool_runs_full_handshake` |
| Messages/Gemini approval parts | Omit approval request/response; denial result can render as ordinary tool output | Approval semantics lost; missing denial reason becomes `Tool call execution denied.` | `render_content_block`, `render_part`; `types.rs`: `ToolResultOutput::to_provider_string`; `approval_response_part_is_dropped_on_non_responses_wires`, `responses_execution_denied_without_reason_uses_default_sentinel` |

## Provider tools and citation pairing

Provider-defined declarations and historical provider-executed calls are
different objects. Their compatibility policies must not be conflated.

| Boundary | Current rule | Consequence | Evidence |
| --- | --- | --- | --- |
| Provider-defined declaration to Messages/Responses/Gemini | Reconstruct source-native tool shape, including when source provider differs from target | Preserve foreign shape without translating it into a target-native equivalent; upstream may reject it | `protocol/mod.rs`: `provider_defined_native`, adapter tool renderers; `provider_defined_tool_cross_protocol_is_preserved_verbatim`, `provider_defined_tool_cross_protocol_onto_anthropic_is_preserved` |
| Provider-defined declaration to Chat | Omit it; emit only function declarations | Capability removed silently at this renderer boundary | `chat_completions.rs`: `render_chat_tool`; `provider_defined_tool_dropped_from_chat_completions` |
| Historical provider-executed call to Chat | Render as ordinary function tool call | Provider-executed distinction lost | `render_message`; response-side assertion: `dynamic_mcp_call_degrades_to_plain_tool_call_cross_protocol` |
| Historical call to Messages | Preserve server-tool shape; dynamic provider-executed MCP uses native MCP shape only with Anthropic server metadata, otherwise degrades to plain `tool_use` | Foreign server identity not translated; native dynamic MCP results support JSON/error-JSON and otherwise drop | `messages.rs`: `render_content_block`; `messages_request_mcp_blocks_round_trip_in_assistant_turn` |
| Historical server calls/results to Responses input | Omit provider-executed calls and dynamic MCP results; client calls/results still render | Response rendering can recombine MCP pairs, but that does not establish request-side replay support | `responses.rs`: `render_message_items`, `render_output_items`; `responses_request_function_call_pair_independent_of_dynamic_mcp`, `responses_mcp_call_round_trips_with_inline_result` |
| Citation sources to Messages client response | Reuse a surviving web-search call ID, otherwise use preserved originating ID, otherwise synthesize a stable fallback call/result pair | Synthesize a protocol wrapper, not an answer to a missing ordinary function call | `messages.rs`: `render_web_search_result_blocks`; `messages_web_search_pair_reuses_real_id`, `messages_source_tool_use_id_correlation_is_exact`, `messages_web_search_pair_synthesized_cross_protocol` |
| Citation sources to Messages client stream | Buffer sources and emit a synthetic paired server call/result using a fixed ID | Stream path should not be assumed to use the full response path's originating-ID priority | `MessagesStreamEncoder::flush_pending_sources`; `messages_streams_and_reencodes_source` |
| Citation sources to Responses/Gemini request | Omit response-side citation metadata from input content | Citations can round-trip through response renderers but are not replayed as input parts | `responses.rs`: `render_message_items`; `generate_content.rs`: `render_part`; implementation observation |

## Adjacent compatibility and continuation

These rules affect whether a historical conversation can continue, but are
not history repair themselves.

| Rule | Current behavior | Evidence |
| --- | --- | --- |
| Opaque request extras | Parsed extras are scoped to originating protocol; supplemental server extras are separate; typed values generally win on render | `types.rs`: `GenerationParams`; `raw_extras_are_scoped_to_their_inbound_protocol`, `server_supplied_extras_remain_available_after_protocol_scoping` |
| Chat target compatibility | Select token-limit field from target; remove unsupported `store`/`stream_options`, but reject explicit `store: true` when unsupported | `ChatCompletionsAdapter::render_request_for_target`; `chat_target_omits_explicitly_unsupported_optional_fields`, `chat_target_does_not_silently_drop_store_true` |
| Chat streaming usage | Force `stream_options.include_usage: true`, unless target compatibility removes the options | `ChatCompletionsAdapter::render_request`; implementation plus target tests above |
| Tool schemas | Messages/Gemini omit function `strict`; Gemini function schemas use a keyword allowlist and nullable-type conversion, dropping unsupported schema constraints | `generate_content.rs`: `sanitize_gemini_schema`; `function_tool_strict_dropped_on_anthropic_and_gemini`, `generate_content_render_sanitizes_tool_schemas_for_gemini` |
| Native Responses continuation | Replace `previous_response_id` from bound pipeline continuation; reject mismatched target; require authority when configured | `executor.rs`: `apply_provider_continuation`, `validate_continuation_authority`; `continuation_override_rewrites_only_the_bound_responses_target`, `authority_requirement_fails_closed_only_for_responses_targets` |
| Detached Responses continuation | Remove `previous_response_id`, retaining visible input/tools | `apply_provider_continuation`; `stock_responses_adapter_suppresses_only_the_detached_parent` |

Native continuation is an authority-bound upstream state reference, not a
repair or reconstruction of visible message history. Its router context must
not be merged blindly into a standalone model-call API.

## Concrete cases to resolve before extraction

A Gemini `contents` entry with two `functionResponse` parts becomes one
canonical Tool message containing two results. Chat's `render_message` loops
over both results but writes into a single JSON object. The second result
overwrites the first. This is a potential cross-protocol data-loss bug inferred
from the two implementations; it has not been reproduced against a live API.
Changing it should come with a focused test of that actual conversion path.

A canonical Responses message containing text, a call, then more text renders
as one combined text message before the standalone call. This changes block
order; it should not be described as preserving arbitrary history interleaving.
The desired contract needs a separate decision and focused assertion.

## Findings for extraction

1. **The current semantic contract is distributed.** Parsing, outbound request
   rendering, client response rendering, and streaming have different rules.
   Same-protocol round-trip tests do not prove cross-model replay safety.
2. **Information loss is not uniformly reported.** Omission and fallback happen
   inside renderers, while unsupported Chat `store: true` is explicitly rejected.
   Preserving foreign provider-tool declarations is another distinct policy.
3. **Provenance is incomplete.** Metadata namespaces preserve selected native
   fields, but there is no shared model/account-bound replay decision.
4. **Some synthesis is protocol packaging.** Citation call/result pairing and
   approval identifiers are not a general missing-result repair facility.
5. **Two ordering/cardinality cases need explicit decisions.** Gemini can produce
   multiple results in one Tool message that Chat currently collapses; Responses
   groups text/media before standalone items. These are implementation findings,
   not approved future behavior.

Keep the stages and existing regression assertions visible during extraction.
The follow-up [spec](BITROUTER_AI_REFACTOR_SPEC.md) selects source-preserving
target projections, structured conversion diagnostics and default exclusion of
task-semantic or unclassified losses. It keeps transcript repair separate.
These are design requirements; this audit remains an inventory of the earlier
source baseline. The [implementation progress](BITROUTER_AI_REFACTOR_PROGRESS.md)
records later corrections, including the two cardinality/order cases, separately
from the admission and diagnostics requirements.
