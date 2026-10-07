# Retire native Gemini Generate Content support

Status: **approved for implementation; replacement validation pending**.

Date: 2026-10-06 (America/New_York).

Related work: [PR #962](https://github.com/bitrouter/bitrouter/pull/962),
`refactor(ai)!: extract model integration (WIP)`.

Audit baseline: PR head `529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16`.
This draft is authored on main baseline
`31cf68ed367658e1ca5123c34431e45e961707c3`, where the AI extraction is not yet
present. Paths naming `bitrouter-ai` below describe the audited PR head.
Recheck the implementation inventory if that head changes.

## 1. Approved decision

Remove Gemini `generateContent` and `streamGenerateContent` from both BitRouter's
public HTTP gateway and its supported upstream protocols. Retain three built-in
wire protocols: Chat Completions, Responses, and Messages.

Keep metered Gemini inference through Google's official OpenAI-compatible Chat
Completions endpoint. Keep the provider-neutral `Prompt`, `GenerateResult`, and
`StreamPart` contract introduced in #962; Core does not adopt a provider's wire
format as its internal protocol.

The approved provider disposition is:

| Surface | Approved disposition |
| --- | --- |
| `google` API-key provider | Retain; serve Gemini through Chat Completions only, after the fidelity and live validation gates below pass. |
| `google-ai` subscription provider | Retire its private Antigravity/Code Assist transport, auth registration, and credential-import integration. Do not reproduce the Gemini codec under a custom protocol name. |
| `vertex` Express Mode provider | Retire the current native-only entry unless a Chat-compatible replacement using its existing authentication mode is independently demonstrated. Do not expand this change into service-account OAuth support. |
| Antigravity SDK as an agent client | Retain an integration path through `LocalOpenAIAgentConfig` and BitRouter's OpenAI-compatible gateway; verify actual interoperability before describing it as tested. |
| Google's Interactions API | Defer implementation. A future upstream adapter requires a concrete workflow and its own reviewed continuation, tool, and streaming contract. |

The user approved the full-removal scope and all recommended decisions in section
7. Implementation remains subject to the replacement validation gates below.

## 2. Rationale and source evidence

Reducing four built-in protocols to three reduces the ordered conversion matrix
from 16 pairs to nine. At the audited head, `generate_content.rs` has 2,049 lines.
These are scope measurements, not measured maintenance, latency, or cost savings.

Google's current documentation identifies Interactions as generally available
since June 2026, recommends it for new projects, and directs future model and
feature launches there. Generate Content is classified as legacy but remains
supported. This supports limiting investment in the old protocol; it does not
establish a shutdown deadline or the future coverage of the Chat compatibility
endpoint. [Interactions overview](https://ai.google.dev/gemini-api/docs/interactions-overview)

The official Gemini compatibility endpoint supports common Chat inference,
streaming, function calls, image input, and reasoning controls. Some Google
options use `extra_body.google`; compatibility remains beta. This is evidence of
a replacement path, not proof that every native feature is equivalent.
[Google OpenAI compatibility](https://ai.google.dev/gemini-api/docs/openai)

Gemini tool-call continuity also exists on the Chat wire: Google documents
`tool_calls[].extra_content.google.thought_signature` and requires preservation
through subsequent tool-result requests, including sequential and parallel calls.
The opaque signature must survive the entire client/router/upstream loop.
[Thought signatures](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures)

Antigravity SDK supports external OpenAI-compatible servers through
`LocalOpenAIAgentConfig(model=..., base_url=...)`. The SDK source forwards the URL
into its local harness model endpoint. This establishes a documented client
connection mechanism; it provides no replacement for BitRouter's private Google
subscription backend. [SDK documentation](https://antigravity.google/docs/sdk/local-models/#external-openai-compatible-servers),
[connection implementation](https://github.com/google-antigravity/antigravity-sdk-python/blob/main/google/antigravity/connections/local/local_openai_connection.py).

The evidence was read on 2026-10-06. This spec does not claim credentialed-provider
inference, Antigravity SDK execution, Vertex replacement, or production validation.

## 3. Audited implementation and migration gaps

The following paths are present at the pinned #962 head:

| Area | Source and finding |
| --- | --- |
| Public ingress | `crates/bitrouter-sdk/src/server.rs` mounts `POST /v1beta/models/{*model_action}` and handles both native verbs. |
| Built-in codec | `crates/bitrouter-ai/src/protocol/generate_content.rs`; `protocol/mod.rs` registers its inbound adapter and outbound adapter/transport. |
| Protocol identity | `crates/bitrouter-ai/src/types.rs` defines `ApiProtocol::GenerateContent`, serialized as `generate_content`; unknown strings currently become custom protocols. |
| Registry vocabulary | `crates/bitrouter-ai/src/catalog/types.rs` maps registry `google` to Generate Content and `antigravity` to its custom transport. |
| Metered Google | `registry/providers/google.yaml` declares `[google, openai]` and an OpenAI-compatible endpoint override. This already supplies the intended endpoint, but not migration proof. |
| Subscription Google | `providers/antigravity/protocol.rs` composes `GenerateContentAdapter` and `GenerateContentTransport`, targeting `cloudcode-pa.googleapis.com/v1internal:*`. |
| Host registration | `apps/bitrouter/src/assemble.rs` installs the Antigravity transport and OAuth applier; application modules own `agy` binary/Keychain discovery and import. |
| Vertex | `registry/providers/vertex.yaml` declares native `google` only and uses Express Mode with `VERTEX_EXPRESS_API_KEY`. |
| Chat fidelity | `protocol/chat_completions.rs` does not retain Google signatures in its typed ingress tool calls, parsed response tool calls, stream deltas, or rendered history. |
| Admission | `crates/bitrouter-ai/src/conversion.rs` currently treats Google signature metadata as representable only on Generate Content. |

Source links: [AI protocol tree](https://github.com/bitrouter/bitrouter/tree/529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16/crates/bitrouter-ai/src/protocol),
[Antigravity adapter](https://github.com/bitrouter/bitrouter/blob/529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16/crates/bitrouter-ai/src/providers/antigravity/protocol.rs),
[registry providers](https://github.com/bitrouter/bitrouter/tree/529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16/registry/providers).

Changing a protocol list or base URL alone is insufficient. In particular, the
current Chat codec would discard metadata required for a later Gemini tool turn.

## 4. Required behavior

### 4.1 Gateway and protocol retirement

- Remove the native HTTP route and its request/response/SSE handlers. Requests to
  the removed paths use the ordinary unmatched-route behavior and must not invoke
  an upstream or create a billable model attempt. Do not keep a native translator
  or a permanent compatibility handler.
- Delete the built-in native codec, transport, registration, schema snapshot, and
  Rust protocol variant. Remove their live registry/schema vocabulary and fixtures.
- Remove the private Antigravity adapter and its supporting product registration,
  login choices, import, refresh, binary discovery, and integration-only tests when
  retiring `google-ai`. Keep shared auth machinery used by other providers.
- Retain outbound custom-protocol extensibility for supported consumers. Reserve
  the retired built-in names against accidentally becoming unregistered custom
  protocols; removal must produce an actionable error for active configuration.
- Remove unused Gemini-specific native structures and conversion branches only
  after inspecting their callers. Retain Google metadata that the Chat migration
  actually uses. Do not retain a duplicate Generate Content implementation behind
  a renamed module, feature, or `Custom("antigravity")` registration.

### 4.2 Metered Gemini through Chat Completions

Retain `GEMINI_API_KEY`, the `google` provider identity, and canonical
`google/gemini-*` model identities where their declared capabilities are verified.
Use `https://generativelanguage.googleapis.com/v1beta/openai` as the Google Chat
base. Verify the actual effective headers, selected endpoint, and model ID; Google's
documented Chat request uses bearer authentication. Retain existing credentials
without printing or rewriting secret values.

The supported initial workflow is text/image input, text output, ordinary function
tools, structured output within demonstrated schema support, streaming, and
declared reasoning controls. Support is per selected model and request shape;
sharing a Chat envelope does not imply identical capability or schema semantics.

Preserve all function-call IDs, names, arguments, result cardinality, and meaningful
order. Reject unrepresentable constraints before dispatch rather than relying on
upstream success after ignored fields. Native-only built-in tools, media options,
or other controls have no implied migration: map and verify those actually used,
or report incompatibility. Do not create an unused capability framework.

Explicit cache references and Google thinking configuration may pass through
`extra_body.google` only when their target-specific meaning is established. Do not
forward those options to another provider solely because it also speaks Chat.
Do not silently discard them to make a fallback candidate appear admissible.

Normalize reported usage without counting reasoning tokens twice. Preserve the
available distinction between input, output, cached input, and reasoning tokens;
unavailable counts remain unavailable. Verify downstream settlement and pricing
against the migrated response shape. Explicit cache-resource creation APIs are
outside this change.

### 4.3 Thought signatures and replay boundaries

Use the existing canonical Google provider metadata rather than a second transcript
format. Map Chat's `extra_content.google.thought_signature` to and from the existing
Google signature metadata when the selected target supports this representation.
Preserve signature placement on individual tool calls and any other documented
message/content locations used by supported workflows.

The preservation path includes ingress parsing, upstream response/SSE decoding,
stream collection, downstream response/SSE encoding, durable canonical capture
where used, and rendering a later request. Late signature chunks and multiple
parallel calls must retain their associations; signature-only updates must not be
dropped merely because they contain no argument text.

Opaque signatures are never fabricated, edited, logged, or treated as credentials
that can be replayed across unrelated targets/accounts. Change #962's admission
rule only for a demonstrated Google Chat representation with valid continuity.
Do not broadly mark all Chat-compatible providers as signature-capable. Preserve
the source transcript and existing authority rules; evaluate fallback from that
source. If continuity cannot be established, exclude or reject the candidate
explicitly. Do not reset history or invoke a signature-validation bypass.

Follow the existing stream commitment and side-effect policy. A late encoding or
continuity failure retains attempt/usage facts and cannot trigger replay after
visible output. The same rules apply to direct AI calls and SDK-routed calls.

### 4.4 Provider retirement, configuration, and stored data

Native protocol selections in active user configuration fail with the affected
location and migration guidance. Do not silently rewrite endpoints, credentials,
or protocol lists, even when another list member is still supported. Explicit
`google-ai` and retired Vertex selections report retirement before credential
discovery, token refresh, upstream work, or metering.

Retiring subscription access does not authorize replacing it with paid API-key
inference. Require explicit user configuration of a supported provider and retain
the existing activation, account-selection, routing, and billing boundaries.

Handle older downloaded catalogs and caches before unsupported registry tokens
can invalidate the entire catalog. Quarantine retired entries with bounded,
content-free diagnostics while keeping unrelated supported entries available;
explicit attempts to use a retired entry still fail. Preserve user overrides and
do not let an older remote catalog reactivate the retired integrations. Historical
canonical model metadata can remain where another supported provider serves it.

Preserve saved credentials, archives, request history, and canonical transcripts.
Do not delete `agy`/Keychain sessions or migrate subscription credentials into API
keys. Historical readers may recognize retired identifiers as provenance without
making them callable protocols. Retain only decoding needed by actual supported
stored formats, not an executable compatibility codec. If an old raw transcript
cannot be replayed with valid continuity, report it as non-resumable through this
path and preserve it; do not invent results or automatically start a fresh session.

### 4.5 Antigravity SDK as a client

The integration to validate is:

```text
Antigravity SDK LocalOpenAIAgentConfig
  -> BitRouter /v1/chat/completions
  -> an admitted, configured upstream
```

Candidate configuration:

```python
from google.antigravity import LocalOpenAIAgentConfig

config = LocalOpenAIAgentConfig(
    model="configured-chat-model",
    base_url="http://127.0.0.1:4356/v1",
)
```

This example assumes an explicitly configured compatible Chat model, a running local router and compatible
gateway authentication. The actual 0.1.20 harness completed a fixture tool task, but failed the signed Google fixture by losing replay proof and function details. Google signed-tool use remains a release blocker; do not bypass continuity admission. Record the SDK version, effective requests, tools, and
stream behavior in validation. `.lightweight()` is an optional SDK preset, not a
BitRouter requirement. Do not claim CLI, IDE, ACP, or subscription interoperability
from this SDK connection mechanism alone, and do not add a new launcher in this
protocol-retirement change.

### 4.6 Implementation decisions

Retire native-only Gemini model entries from `opencode-zen`; its official endpoint table advertises the Google native SDK path, so do not infer a Chat replacement from other Zen models. Keep its supported models. [Zen endpoints](https://opencode.ai/docs/zen/#endpoints).

Remove the bundled `gemini-cli` agent/runtime entry: its first-party harness wiring targets the removed native gateway. Independent own-auth `agy` launch remains supported.

Google tool signatures retain the existing canonical `google.thoughtSignature` slot. Actual selected calls add a stateless `google.replayProof`, scoped to the static bearer key, provider, endpoint, model, account label, tool ID/name, exact argument bytes, and signature. The gateway maps this to `extra_content.bitrouter.google_replay_proof`; clients must echo both Google and BitRouter metadata unchanged. This sidecar is removed before upstream dispatch. Missing or changed proof rejects replay before effects, including unrelated fallbacks. This client preservation requirement must be checked with the actual SDK.

The initial Google schema subset is conservative: unverified schema keywords, explicit strict flags or schema metadata, disabled parallel calls, and unclassified Chat extras exclude Google. Explicit cache resources and message-level continuity have no established replay authority and are refused. Available usage from a successful HTTP response remains attached to late validation errors and settlement.

## 5. Relationship to #962 and documentation

The [AI extraction spec](https://github.com/bitrouter/bitrouter/blob/529f2fdeb7dc1f6bd3cef2ae243b22106b66ef16/docs/BITROUTER_AI_REFACTOR_SPEC.md)
currently requires four-protocol codecs and fixtures. If this proposal is approved,
revise that requirement, its AI-03 acceptance criterion, ownership tables, and
progress claims to three built-in protocols. Carry forward the remaining fidelity,
authority, and conversion-admission obligations for supported paths; reducing the
matrix does not complete the other WIP acceptance criteria.

Implementation must update:

- protocol/config schemas, public API inventories, and consumers in AI, SDK, app,
  helpers, and known external consumers including `bitrouter-cloud`;
- editorial registry entries and committed `dist/registry`, regenerated together;
- runtime catalogs/caches, provider activation, route resolution, and relevant
  conformance/benchmark fixtures;
- README, internal CLI/development docs, and `skills/bitrouter/` in lockstep with
  changed provider/login/harness guidance;
- `.claude-plugin/`, `.codex-plugin/`, and `.agents/plugins/marketplace.json` wherever
  their distributed guidance references the removed surface;
- a release note stating the removed endpoints/protocols, provider consequences,
  manual configuration migration, and preserved stored-data boundaries.

Product API/migration prose and the supported-model catalog refresh belong in
`bitrouter-docs`, following that repository's authoring contract. Track their
delivery separately; this spec does not claim external repositories are updated.
Do not advertise the proposed behavior in current product docs before delivery.

## 6. Implementation sequence and acceptance

1. **Approve disposition and refresh the audit.** Confirm the decisions in section
   7, inventory current callers and stored-data consumers, and revise #962's
   four-protocol requirement. Pin the implementation head and supported Google
   model/SDK versions; do not base capability declarations on moving examples.
2. **Repair and prove the Google Chat path.** Add focused fixtures for signatures,
   supported generation options, and admission; exercise both direct AI calls and
   the assembled router. Keep native support during this preparatory step.
3. **Change provider/catalog policy and remove native execution.** Migrate `google`,
   implement retired-config/cache handling, remove `google-ai`, resolve the Vertex
   gate, and delete native ingress/codec/registrations together. Update consumers
   and generated artifacts.
4. **Validate the replacement and publish accurate migration guidance.** Record
   local fixture, hosted CI, credentialed-provider, SDK, and production evidence
   separately. None of those evidence classes substitutes for another.

| ID | Required evidence |
| --- | --- |
| GR-01 | The three remaining inbound protocols and supported outbound combinations pass ordinary/SSE fixtures; native paths perform zero upstream requests. |
| GR-02 | Signature round trips preserve bytes and placement across ordinary/SSE calls, collection, downstream output, and later requests. Include sequential calls, parallel calls to the same function, fragmented arguments, and late/signature-only updates. |
| GR-03 | Real Google Chat inference completes at least two tool cycles and a parallel-tool turn in ordinary and streaming modes, including a real supported thinking model. Measure task completion, errors/rework, latency, and available usage/cost; document any native comparison conducted before deletion. |
| GR-04 | Supported reasoning controls, image input, structured output, and reported cache/reasoning usage work through the actual selected Google target. Unsupported constraints fail explicitly; provider-specific extras and signatures cannot leak into unrelated fallback targets. Explicit cache references require separate proof if retained. |
| GR-05 | An actual Antigravity SDK agent completes a tool-using task through BitRouter with captured request/stream evidence. A constructed config or mock SDK client alone is insufficient. |
| GR-06 | Retired active configs and explicit provider selections fail before credential/upstream effects; old catalog/cache entries cannot reactivate them or break unrelated providers. No automatic change from subscription credentials to API-key billing occurs. |
| GR-07 | Saved credential/history fixtures remain readable where currently supported; native-only replay is blocked without altering source records, inventing continuity, or resetting a conversation. |
| GR-08 | No first-party native codec, transport, registrations, or hidden renamed implementation remains. Search matches are limited to migration diagnostics, historical provenance, and historical/design documentation. |
| GR-09 | Registry validation/build/check, schema/public API checks, workspace tests, doctests, Clippy, and formatting pass on the implementation head. Known external consumers have explicit delivery evidence or a documented release blocker. |

For GR-09, run the repository checks required by `AGENTS.md`:

```sh
cargo nextest run --all-features
cargo test --all-features --doc
cargo clippy --all-features
cargo fmt -- --check
cargo run -p dist-helper -- registry validate
cargo run -p dist-helper -- registry build
cargo run -p dist-helper -- check
```

If nextest is unavailable, use `cargo test --all-features`. Nextest does not execute
doctests, so the separate doctest invocation is required when using it. Registry
data changes do not need tests that freeze provider entries or model counts;
runtime protocol/retirement behavior does need focused verification.

No credentialed-provider or SDK proof has been collected for this proposal.
Failure of those gates blocks claiming a successful replacement; mocks and #962's
existing test results do not discharge them.

## 7. Approved decisions

1. **Subscription access:** approve retiring `google-ai` and its private backend
   integration? Recommended: yes, while retaining the SDK client path described
   above. If subscription inference is required, full codec removal needs a
   verified replacement and this spec must change before implementation.
2. **Vertex Express:** approve retirement if existing static-key access cannot be
   demonstrated through a supported Chat endpoint? Recommended: yes; do not add
   service-account auth to this scope.
3. **Interactions:** approve deferring a native Interactions adapter until a
   concrete workflow needs it? Recommended: yes. Adding it now would be a separate
   protocol project rather than the proposed reduction to three built-in codecs.
4. **Delivery:** perform this as a distinct implementation change dependent on
   #962, or revise #962 while it remains WIP? Recommended: a separate reviewable
   implementation change, coordinated with #962's specification and consumers.

The user approved all four recommended decisions. The implementation proceeds as
a separate change based on #962. This approval does not substitute for the
replacement validation gates or authorize deleting saved credentials/history.

Google reasoning uses the canonical declared `reasoning_effort` path (`minimal` through `high`) plus scoped `include_thoughts`. Raw `thinking_level`/`thinking_budget`, `none`/`xhigh`/`max`, seeds and penalties remain excluded until selected-model equivalence is demonstrated. Static-auth validation rejects duplicate bearer headers and conflicting Google key headers/query credentials.
