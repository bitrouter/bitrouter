# Bitrouter AI refactor proposal

**Status: proposed for team review; no implementation in this PR.**

Model integration is split between `bitrouter-providers`, model/provider
registry data, and `bitrouter-sdk::language_model`. Providers depend on SDK
configuration and model types; protocol execution depends on routing targets
and pipeline context. This proposal brings model integration into a
`bitrouter-ai` crate while continuing the gradual retirement of SDK
responsibilities. A consumer should be able to call a model without assembling
a BitRouter router or server.

The proposal borrows pi-ai's separation of model semantics, provider runtime,
and shared API implementations. BitRouter also serves incoming model API
requests, so its bidirectional codecs and client stream encoders remain a
first-class requirement. See the accompanying
[history compatibility audit](MODEL_HISTORY_COMPATIBILITY_AUDIT.md) for the
existing behavior that an extraction must account for.

## Agreed direction and review scope

The initiating discussion established three directions:

1. Evolve `bitrouter-providers` into `bitrouter-ai`, absorbing model/provider
   registry runtime responsibilities and SDK components related to model APIs.
2. Remove ACP runtime and harness metadata from this registry. Its replacement
   home is a separate decision; removing ACP execution itself is not implied.
3. Inventory current compatibility rules before deciding whether to preserve,
   fix, or replace them. Do not equate current conversions with a general
   history-repair facility.

The detailed boundaries, migration order, and public API below are proposals.
Merging this document records a design for review; it does not authorize a
silent change to model semantics or mark the refactor as implemented.

## Evidence and motivation

The local code review uses commit `d93ed73b` as its baseline.

- [`bitrouter-providers/Cargo.toml`](../crates/bitrouter-providers/Cargo.toml)
  depends on SDK with `config_file`; provider auth implementations also import
  SDK errors, `RoutingTarget`, and `AuthApplier`.
- [`registry/apply.rs`](../crates/bitrouter-providers/src/registry/apply.rs)
  mixes catalog mapping with credential-based activation and user-config merge.
- [`protocol/mod.rs`](../crates/bitrouter-sdk/src/language_model/protocol/mod.rs)
  already separates inbound/outbound adapters and stream encoders/decoders,
  but outbound adapters and transports accept `RoutingTarget`.
- [`executor.rs`](../crates/bitrouter-sdk/src/language_model/executor.rs)
  includes HTTP execution, pipeline context, credential authority, and native
  Responses continuation substitution.
- [`types.rs`](../crates/bitrouter-sdk/src/language_model/types.rs)
  combines model content with `RoutingTarget` and pipeline request/response
  envelopes. Renaming the existing crate without separating these contracts
  would preserve the coupling.

The concrete beneficiaries are the existing router and its library consumers.
A standalone call example is an acceptance check on the boundary, not a reason
to invent additional framework abstractions without callers. Downstream
consumers outside this workspace must be inventoried before deleting old APIs.

## What to borrow from pi ai

The reviewed upstream is [earendil-works/pi, packages/ai](https://github.com/earendil-works/pi/tree/main/packages/ai),
formerly `badlogic/pi-mono`. These observations concern the main-branch surface
reviewed on 2026-10-01, not a pinned npm release.

| Upstream responsibility | Proposed BitRouter interpretation |
| --- | --- |
| Unified messages and assistant stream events | Own model request/result/content semantics and event lifecycle in the AI crate |
| Provider owns catalog, authentication and invocation | Keep provider-specific behavior with the integration, sharing codecs across providers |
| Explicit `Models` collection with injected stores | Prefer explicit runtime state and storage contracts over a process-global registry |
| History adaptation for the next target model | Make adaptation a deliberate contract; inspect existing losses before selecting defaults |
| Agent loop lives in `pi-agent-core` | Keep server-tool execution, multi-turn scheduling and agent control above the model-call layer |

The upstream [types](https://github.com/earendil-works/pi/blob/main/packages/ai/src/types.ts),
[provider collection](https://github.com/earendil-works/pi/blob/main/packages/ai/src/models.ts),
and [message transforms](https://github.com/earendil-works/pi/blob/main/packages/ai/src/api/transform-messages.ts)
are the relevant references. Upstream `complete()` aggregates the stream result;
this is not a decision to remove BitRouter's existing non-streaming HTTP path.
Its transforms can remove reasoning credentials, substitute image placeholders,
synthesize missing results and omit failed turns. Those defaults must not be
copied automatically into a transparent gateway. Image-generation and
classifier additions upstream are not proposed additions to this refactor.

## Proposed responsibility boundary

`bitrouter-ai` owns the representation and execution of a model call, including
the compatibility data needed to make that call. The router owns selection and
deployment policy.

| Move into `bitrouter-ai` | Keep in the router, application or remaining SDK during migration |
| --- | --- |
| Model content, prompts, generation options, results, usage and stream events | Caller identity, pipeline context and request envelopes |
| Chat Completions, Responses, Messages and Generate Content bidirectional codecs, SSE encoding/decoding | Axum endpoints, admin API, App assembly and extension lifecycle |
| A call to an already selected upstream, timeouts, cancellation and provider error semantics | Route resolution, sort order, multi-account selection, cross-provider fallback |
| Provider auth application, token exchange/refresh and request adaptations | User-config activation, BYOK policy, CLI prompts and invocation-specific error text |
| Provider/model catalog schema, capabilities, pricing metadata and compatibility settings | Config YAML parsing, curated default routing decisions, settlement and charging |
| Explicit catalog loading/refresh and injectable credential/catalog storage where needed | ACP/MCP routing and runtime control, server-tool execution loops |

Inbound codecs remain usable without starting a server. A pure codec should
not need a credential store or a network request. Provider login/refresh can
live in the integration layer while interactive UI and storage location policy
remain application concerns. Existing `pkce`/`hosted` feature isolation should
be assessed rather than replaced with a new feature matrix by assumption.

Request retries against the selected upstream can belong in the AI layer,
with an explicit policy and stream-commit boundary. Cross-provider fallback
stays in the router. Avoid overlapping retries that multiply attempts or replay
a request after visible output without a defined contract.

## Dependencies and model call contract

The target dependency direction is:

```text
router / app / remaining SDK
             |
             v
        bitrouter-ai
```

`bitrouter-ai` must not depend back on `bitrouter-sdk` or app configuration.
The SDK may temporarily wrap the new implementation. Shared Rust types should
have one owner; moving them must not introduce competing SDK and AI versions.
Public module paths should follow the repository's rule against re-exporting
items from an already public module. Any temporary compatibility exception
requires an explicit team decision and retirement point.

Split the resolved call description from `RoutingTarget`. The lower layer needs
the concrete model/service ID, protocol, endpoint, effective authentication,
headers and model compatibility. It should not need route alternatives,
settlement, caller identity, or the policy that selected the account. Exact
type/function names are intentionally left for implementation review.

Create model-call error semantics independent of gateway HTTP status policy;
the router can map them into its public error response. Extract HTTP/SSE
execution from `PipelineContext` instead of relocating the whole executor.
Likewise separate transport framing, usage normalization and accumulation from
stream hooks and usage settlement.

Native Responses continuation needs particular care: an upstream response ID
is bound to a target and authentication authority. The lower layer can accept
an explicitly authorized continuation, but router/session code remains
responsible for establishing that authority. Do not flatten it into an
unrestricted extra parameter during extraction.

## Registry ownership and ACP removal

The AI crate consumes and understands the model/provider catalog. Owning its
runtime schema does not require embedding registry editorial work, discovery
jobs, validation and dist generation into the runtime library.

Proposed initial arrangement:

- Keep source YAML and dist publishing at repository level; move runtime
  model/provider schema and loading behavior into the AI crate.
- Preserve separate catalog and crate release lifecycles. The app currently
  bundles defaults in `build.rs` and overlays fetched data via
  [`bundled_registry.rs`](../apps/bitrouter/src/bundled_registry.rs); preserve
  offline bootstrap while deciding who supplies the baseline to library users.
- Keep `apply_registry(Config)`, credential-triggered activation, provider
  classification and curated routing defaults in the upper layer.
- Decide which canonical metadata becomes usable at runtime. Current
  `CanonicalModel` reads only `id`; renaming `RegistryData` does not produce a
  complete model-capability API.

ACP migration affects `registry/agents`, `registry/runtimes`, both dist output
locations, `dist-helper`, and the harness code generated by
[`apps/bitrouter/build.rs`](../apps/bitrouter/build.rs). Move or replace that
source before deleting build inputs. Keep model/provider generation working
independently. The official ACP registry client is a separate discovery path.
Update affected architecture docs and skill/manifests in lockstep if CLI or
harness wiring changes, as required by `AGENTS.md`.

## History compatibility contract

Use the [audit](MODEL_HISTORY_COMPATIBILITY_AUDIT.md) as the evidence baseline,
not as approval of every current behavior. Migration must account separately
for inbound parsing, upstream request conversion, client response conversion,
and streaming. A response round-trip does not prove history replay is safe.

Before moving these paths, add focused conversion assertions for two findings:

1. Gemini can parse several tool results into one Tool message; Chat rendering
   currently overwrites all but the last result.
2. Responses rendering groups text/media before standalone items, potentially
   changing interleaved block order.

These are source-derived findings, not live-provider reproductions. Fixes
should be separately reviewable, with the intended semantics stated explicitly.
Do not freeze suspected data loss as the desired contract merely to claim a
behavior-preserving move.

Recommended review direction: keep transcript repair distinct from wire
conversion and expose losses deliberately. Whether to add structured conversion
diagnostics, strict/permissive modes, model/account provenance or automatic
missing-result repair remains open. Existing metadata preserves selected native
fields but does not establish cross-model replay authority.

## Proposed migration sequence

| Phase | Work | Completion evidence |
| --- | --- | --- |
| 0. Establish semantics | Review audit; verify cardinality/order findings; decide treatment of observable losses | Targeted protocol conversion assertions and documented decisions |
| 1. Extract types and codecs | Establish `bitrouter-ai`; move model semantic types, independent errors, protocol/SSE codecs; split target contract | Four-protocol fixtures and existing regression assertions pass; AI has no SDK dependency |
| 2. Extract invocation | Move HTTP execution, auth contracts, timeouts/cancellation and selected-upstream error handling; SDK executor delegates | Direct streaming and non-streaming examples work without App/config/router; cancellation/error and continuation boundaries verified |
| 3. Consolidate integrations | Move provider implementations and registry runtime components; retain upper-layer activation/config merge | OAuth/token exchange, explicit credentials, offline baseline and catalog refresh remain supported; feature dependencies checked |
| 4. Remove ACP catalog coupling | Relocate harness metadata and generation to an agreed owner; remove it from model/provider registry tooling/artifacts | Harness behavior retained as agreed; model/provider validate/build/check succeed independently |
| 5. Retire old ownership | Migrate workspace and known downstream consumers, public docs/API checks and release metadata; remove providers crate and obsolete SDK implementations | One owner per model type/implementation; documented breaking changes or bounded compatibility path |

ACP relocation can proceed independently once its replacement owner is agreed.
Do not combine all phases into a crate rename PR. The old SDK remains only for
responsibilities not yet migrated; this proposal does not select their eventual
crate layout.

## Acceptance and validation

- A consumer depending only on `bitrouter-ai` can explicitly select an endpoint,
  model and credentials, complete one streaming or non-streaming call, and handle
  cancellation/errors. It does not construct `Config`, a routing table or App.
- The existing router preserves four-protocol ingress/egress, fallback,
  settlement, account selection and authorization-bound continuation behavior,
  except for separately reviewed corrections.
- Catalog metadata can be inspected without authenticating or launching a
  server; loading/refresh is explicit and preserves supported offline behavior.
- Codec, native-field and stream-state regression coverage moves with its
  implementation. Known losses and synthesis are documented at their stage.
- Default AI dependency/features do not pull in Axum, ACP, MCP or a pipeline
  runtime. Check public API dependency manifests and known downstream use.
- Implementation PRs run the repository's required all-feature tests, clippy
  and formatting checks. Registry data/tooling changes additionally run
  `dist-helper` registry validate/build/check and commit generated artifacts.

For this documentation PR, validation is source/reference review, local link
checks and `git diff --check`. Rust behavior, registry artifacts and dependencies
are unchanged; implementation acceptance remains future work.

## Decisions requested from reviewers

| Decision | Recommendation | What it blocks |
| --- | --- | --- |
| Independent model-call API and bidirectional codecs in one crate? | Yes; separate modules rather than additional crates without consumers | Initial extraction boundary |
| Registry runtime schema/loading versus source YAML/tooling ownership? | AI owns runtime; keep publishing repository-level initially | Catalog migration |
| ACP harness metadata's replacement owner? | Keep it with the application/ACP integration that consumes it; determine exact location | Deleting current ACP registry inputs |
| Existing silent losses and future history repair? | Verify findings first; review behavior changes separately; do not adopt pi-ai defaults wholesale | Semantic migration and diagnostics API |
| Old SDK/provider public API transition? | Inventory downstream callers; agree breaking release versus a time-bounded compatibility path | Removal of old crate/API ownership |
| Credential store, offline baseline and optional features? | Preserve current consumers through explicit injection and existing feature isolation | Standalone library defaults |

Team review should settle these boundaries before prescribing a detailed public
API. The evidence-backed extraction can then proceed in reviewable phases.
