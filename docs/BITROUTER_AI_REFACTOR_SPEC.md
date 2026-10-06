# BitRouter AI refactor specification

**Status: design baseline; phased implementation started locally.**

Updated on 2026-10-04 to record the design discussion for
[PR #953](https://github.com/bitrouter/bitrouter/pull/953).
The providers-retirement plan was revised on 2026-10-06 to separate reusable
authentication mechanisms from application policy and Cloud assembly.

At the audit baseline, model integration was split between `bitrouter-providers`,
model/provider registry data, and `bitrouter-sdk::language_model`. Providers depended
on SDK configuration and model types; protocol execution depended on routing targets
and pipeline context. This refactor brings model integration into
`bitrouter-ai`. A consumer must be able to call a selected model without
assembling a BitRouter router or server.

BitRouter also serves incoming model API requests. Bidirectional codecs and
client stream encoders remain first-class requirements. The accompanying
[history compatibility audit](MODEL_HISTORY_COMPATIBILITY_AUDIT.md) records
existing behavior; it does not approve current losses as the future contract.

## Decisions recorded from the discussion

| Topic | Selected direction |
| --- | --- |
| Model integration | AI owns model semantics, protocol codecs, selected-upstream execution, provider authentication mechanisms and catalog runtime |
| Core and orchestrator | Integrate with the Core native protocol being designed; do not choose a provider wire protocol as their default |
| Public API migration | Alpha permits breaking changes; migrate known consumers and remove old ownership without a standing compatibility layer |
| Automatic routing | Allow equivalent conversions and classified nonessential losses with diagnostics; exclude task-semantic losses and unknown effects by default |
| History | Preserve the source transcript and derive a projection for each target; conversion is not automatic transcript repair |
| ACP catalog | Keep data-driven runtime and harness metadata under the application/ACP integration, separate from the model/provider catalog |
| Model catalog | AI owns schema/loading/explicit refresh; the application supplies its offline baseline and persistence |
| Credentials | AI provides explicit login/authentication/refresh mechanisms, transaction contracts and reusable explicit-path storage; the application selects accounts, activation, storage locations/protection and interactive UI |
| Cloud | AI owns hosted-provider authentication and credential bindings; the application owns Cloud management, default scopes, account assembly, provider activation and telemetry wiring |

These directions are accepted for this draft. Remaining review items below
concern concrete contracts and locations. See the
[implementation progress](BITROUTER_AI_REFACTOR_PROGRESS.md) for completed batches
and validation. Core integration, remaining conversion admission, ACP relocation and known
external consumer migration remain pending.

## Evidence and upstream reference

The source audit uses `d93ed73b` (2026-10-01). The follow-up design was checked
against PR #953 head `8e267e720795b1b770bd1a72c758fda8f909f612`.

- [`bitrouter-providers/Cargo.toml` at the audit baseline](https://github.com/bitrouter/bitrouter/blob/d93ed73b/crates/bitrouter-providers/Cargo.toml)
  depended on SDK with `config_file`; auth implementations imported SDK errors,
  `RoutingTarget` and `AuthApplier`. The package is now removed locally.
- [`registry/apply.rs`](../apps/bitrouter/src/providers/registry/apply.rs)
  retains catalog mapping, credential-based activation and user-config merge
  under the application owner.
- At the baseline, protocol adapters accepted SDK `RoutingTarget`. The moved
  [`protocol/mod.rs`](../crates/bitrouter-ai/src/protocol/mod.rs) now uses the
  selected `ModelTarget`; SDK routing and public-error policy remain above AI.
- [`executor.rs`](../crates/bitrouter-sdk/src/language_model/executor.rs)
  combined HTTP execution, pipeline context, credential authority and native
  Responses continuation substitution at the baseline. It now delegates
  request projection, HTTP I/O and decoding to
  [`ModelClient`](../crates/bitrouter-ai/src/client.rs), retaining account
  refresh, gateway policy and continuation authority during extraction.
- [`types.rs`](../crates/bitrouter-sdk/src/language_model/types.rs)
  combined model content with routing and pipeline envelopes at the baseline;
  model content is now owned by [`AI types`](../crates/bitrouter-ai/src/types.rs).

The pi-ai reference is
[earendil-works/pi at b2b5c42](https://github.com/earendil-works/pi/tree/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai),
reviewed on 2026-10-04, rather than an assumed npm release.

| Upstream behavior | BitRouter interpretation |
| --- | --- |
| Unified messages and assistant stream events | Give model semantics and event lifecycle one owner |
| Provider catalog, authentication and invocation | Keep provider behavior with its integration, sharing codecs |
| Explicit `Models` collection and injected stores | Use explicit runtime state and storage contracts |
| Agent loop outside AI | Keep scheduling, tool execution and session control above model calls |
| Target-specific history adaptation | Classify conversion effects; do not copy permissive repair defaults |

Pi's [Models implementation](https://github.com/earendil-works/pi/blob/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai/src/models.ts)
uses in-memory credential/model stores by default. Built-in provider catalogs
are supplied through an explicit
[provider collection](https://github.com/earendil-works/pi/blob/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai/src/providers/all.ts);
refresh and login are explicit operations. Its
[credential resolver](https://github.com/earendil-works/pi/blob/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai/src/auth/resolve.ts)
prioritizes an explicit key, then stored credentials, then ambient provider
credentials. Its
[credential store](https://github.com/earendil-works/pi/blob/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai/src/auth/credential-store.ts)
coordinates updates through a store operation; durable storage is injectable.
BitRouter needs account-scoped authority beyond a provider-only lookup.

Pi's [message transforms](https://github.com/earendil-works/pi/blob/b2b5c42f6138b73ec4b2f49ec0ca468800f88586/packages/ai/src/api/transform-messages.ts)
can remove signatures, turn reasoning into text, substitute image placeholders,
synthesize missing tool results and omit failed turns. These are not BitRouter's
defaults. Pi's streaming aggregation also does not require removing BitRouter's
non-streaming HTTP path. Image generation and classifier expansion are outside
this extraction.

## Responsibility and dependency boundary

| `bitrouter-ai` owns | Router, Core, orchestrator or application owns |
| --- | --- |
| Model content, generation options, results, usage and stream events | Caller identity, turn/session state and pipeline envelopes |
| Chat Completions, Responses, Messages and Generate Content bidirectional codecs and stream framing | HTTP endpoints, admin API, App assembly and extension lifecycle |
| Calls to a selected upstream, timeouts, cancellation and model-call errors | Route resolution, ordering, account selection and cross-provider fallback |
| Auth application, explicit login/token exchange/refresh and provider request adaptations | BYOK/activation policy, interactive UI, account selection and storage location |
| Model/provider catalog schema and runtime metadata, including compatibility rules | Editorial YAML, dist publishing, curated routing defaults and settlement |
| Explicit catalog loading/refresh and minimal injected stores | ACP/MCP runtime control, harness wiring and server-tool execution |

The dependency direction is:

```text
router / app / Core integration / orchestrator / remaining SDK
                              |
                              v
                         bitrouter-ai
```

AI must not depend on SDK, app configuration, Core or orchestrator runtime.
Pure codecs require neither authentication nor networking. Model-call errors
are independent of gateway HTTP error policy. Transport usage normalization
and accumulation are separate from stream hooks and settlement.

Split the selected call description from `RoutingTarget`: endpoint, concrete
model/service ID, protocol, effective authentication, headers and applicable
compatibility settings belong below the boundary. Route alternatives, caller
identity, account selection policy and charging do not. Exact Rust names remain
an implementation review item; do not add abstractions without a current caller.

Retries for a selected upstream may live in AI under an explicit policy.
Cross-provider fallback remains in the router. Coordinate retry budgets and
stream commitment so retries cannot multiply unexpectedly or replay after
visible output.

### Core native protocol integration

Core and `bitrouter-orchestrator` use the Core native protocol under development
as their internal contract. This spec does not define that protocol's complete
schema or impose Responses, Chat Completions or another provider wire format.
External gateway clients continue to receive their requested supported protocol.

```text
Core native request
  -> integration mapping to AI model semantics
  -> target admission and provider codec
  -> selected upstream
  -> AI result/events
  -> integration mapping to Core native result/events
```

AI model semantic types have one owner. A Core-specific bridge lives above AI;
if Core adopts AI semantic types directly, reuse them rather than introducing
a duplicate model representation. Provider-native opaque fields may be carried
with provenance and replay restrictions, without becoming Core's default wire
contract.

The bridge must specify ordered content, tool-call/result identity, usage,
terminal success/error/cancellation and stream lifecycle. Model events do not
execute tools or advance durable turns. Core defines live state and recovery
semantics; the Harness owns durable storage and ACK authority. Extracting AI
does not create a second session or persistence framework.

### Alpha API migration

Breaking public API changes are allowed. Inventory workspace and known external
consumers, migrate them, and document the change. Move each model type and
implementation to one owner, then delete its old definition.

An SDK executor may delegate during staged extraction. This is a migration
step, not a permanent facade or a second complete API. Do not add public
re-export compatibility shims contrary to the repository's module rules.
Breaking API permission does not authorize losing saved credentials, transcript
data or model semantics. Remaining SDK responsibilities outside model calls
may stay until their own migration; their final crate layout is out of scope.

## Catalog and ACP ownership

### Model/provider catalog defaults

1. AI owns runtime schema, catalog inspection, loading and refresh behavior.
   Keep editorial YAML, validation and dist publishing repository-level
   initially. Moving schema does not move build tooling into the runtime.
2. Catalog loading is explicit. Ordinary invocation must not trigger catalog
   discovery or refresh. Network refresh is a distinct operation with an
   explicit network policy; listing a loaded catalog requires no credentials.
3. The application supplies its bundled offline baseline. Preserve the existing
   [`bundled_registry.rs`](../apps/bitrouter/src/bundled_registry.rs) bootstrap
   and overlay behavior during migration. AI does not silently install a global
   baseline or read application config/files on a direct model call.
4. A library collection can default to in-memory state. Durable catalog storage
   is injected by its caller. A failed refresh retains the last valid snapshot
   and reports its failure/staleness rather than replacing it with an empty
   catalog. With no snapshot, report unavailable data explicitly.
5. User overrides and disabled entries remain authoritative over refreshed
   defaults. Application-level activation, `apply_registry(Config)`, provider
   classification and curated routing defaults stay above AI.
6. An explicit endpoint/model/protocol/credential call works without catalog,
   routing table or `Config`. Define only runtime metadata used by current
   callers; the current `CanonicalModel` reading only `id` is not evidence of
   a complete capability API.

Catalog and crate releases remain independent. Do not introduce mandatory
all-provider discovery or speculative database/storage backends.

### Data-driven ACP catalog

ACP runtime and harness metadata leaves the model/provider catalog but remains
data-driven. Its source/schema/loading and generation belong to the application
or ACP integration that consumes it. Exact source and artifact paths require
review before deleting existing inputs.

Keep execution environment metadata (`runtime`) separate from program invocation
and configuration wiring (`harness`). Preserve current config synthesis and
package-local build inputs consumed by
[`apps/bitrouter/build.rs`](../apps/bitrouter/build.rs). Do not replace the
catalog with per-harness branches in application Rust code.

Migration covers `registry/agents`, `registry/runtimes`, both generated dist
locations and `dist-helper`. Relocate consumers and inputs together; model and
provider validate/build/check must then work independently of ACP metadata.
The official ACP registry client is a separate discovery path. Update affected
architecture docs and skill/plugin manifests when actual CLI or harness wiring
changes, per `AGENTS.md`.

## Credential resolution and refresh

AI supplies mechanisms and storage contracts; the application selects account
authority, persistence backends and their operating scope. A reusable backend
may be supplied by AI without choosing a product's default path or account.
A selected call receives the effective credential or an explicitly selected
account-scoped resolver. AI must not choose another account to recover from an
authentication error.

| Mechanism in AI | Policy in the application |
| --- | --- |
| Apply API keys/tokens and provider auth headers | Select account, BYOK and activation scope |
| Execute explicit PKCE/device-code login, token exchange and refresh with caller-supplied inputs | Start interaction, open the browser, render prompts and choose login/logout behavior |
| Coordinate credential updates through an injected store; provide reusable explicit-path storage when used by current callers | Choose the backend, default location, access protection, migration policy and cross-process coordination |
| Resolve permitted ambient credentials | Define allowed environment/file scope and opt-in configuration |

Ownership of persistence policy does not require every file backend to be
implemented in the application. An optional explicit-path backend can be useful
to library consumers. It must use the same transaction contract as memory or
caller-supplied storage, with no implicit home-directory lookup or account
discovery. Existing formats and saved credentials remain compatible during
extraction; optional dependency boundaries and actual durability must be tested.

Hosted-provider authentication is part of AI: resolve bearer credentials,
refresh tokens and retain issuer/origin/namespace/scope bindings. Cloud account
management, default login scopes, provider auto-activation, CLI flags and
telemetry registration are application responsibilities. Model requests, Cloud
management and telemetry must share the selected account's refresh/commit
coordination; moving modules must not create independent token rotators.

Required behavior:

1. Explicit call credentials have priority. Otherwise use the selected stored
   credential; ambient convenience is allowed only when no stored credential
   exists and the caller permits it. Do not silently switch from failed OAuth
   or an invalid stored key to an environment key.
2. Missing credentials return an actionable auth error. A model call never
   starts interactive login. Login is an explicit operation and persists only
   through the caller's chosen store.
3. Refresh is serialized for the selected credential/account. Concurrent calls
   observe the committed replacement rather than independently rotating the
   same refresh token. In-memory coordination guarantees only this process;
   durable/multi-process guarantees require the application's store contract.
4. Once an irreversible token rotation starts, caller cancellation must not
   discard a returned replacement token. Finish the bounded auth operation and
   persist its outcome, while stopping cancelled model execution. Persistence
   failure is explicit and must not be reported as a successful durable update.
5. Store/resolver interfaces must support current consumers, without introducing
   an unused generic account framework. Specify atomic refresh/update behavior
   before integration; a read/write-only trait is insufficient for rotation.
6. Native response IDs and sealed reasoning are bound to their target and
   credential authority. Preserve those bindings through account refresh and
   migration. Review how a refreshed credential proves continuity; provider
   names or metadata namespaces alone do not establish replay authority.
7. Authentication diagnostics contain status and identifiers permitted by the
   application, never keys, tokens or credential payloads.

Preserve real `pkce`/`hosted` feature isolation. Check dependencies with current
consumers before altering optional features; a speculative feature matrix is
outside this refactor.

## Conversion and automatic routing contract

### Default admission policy

Availability means completing the intended task, not merely obtaining HTTP
success. Silent loss of tool results, constraints or history can create an
apparently healthy route that fails the task and contaminates reliability data.
The default therefore retains harmless conversion opportunities while excluding
known task-semantic loss and effects that have not been classified.

| Conversion effect | Default | Explicit relaxation |
| --- | --- | --- |
| Equivalent representation | Eligible; preserve identity and order relationships | Unnecessary |
| Proven nonessential loss | Eligible with structured diagnostic | Caller may require stricter preservation |
| Known task-semantic loss | Exclude target before dispatch | Only a specifically classified degradation authorized by the caller's policy |
| Unknown effect | Exclude target before dispatch | Classify it before allowing it |
| Invalid replay authority or broken tool causality | Reject unsafe replay | Never waived by permissive conversion policy |

Loss classification is data-driven where target metadata describes compatibility;
codecs detect actual request effects. A nonessential classification needs a
defined request/target scope and supporting evidence; an HTTP success or a
provider-wide label is insufficient. Do not grow this into a complete speculative
capability matrix. Define the first rules from the audit and current callers.
A paired tool-ID remap can be equivalent if uniqueness and call/result pairing
survive. Dropping one of several tool results cannot be labeled nonessential.
Native reasoning/signature omission is not automatically harmless: check its
continuity requirements and authority before admitting the target.

An explicit degradation policy names the accepted effect; a blanket permissive
flag must not authorize arbitrary future loss. Candidate ranking by configured
policy or observed reliability happens among admitted candidates. An excluded
candidate is not an attempted provider failure. Admitted conversions and
relaxations accompany attempt observations so HTTP success alone does not hide
semantic degradation. This spec does not introduce a new routing optimizer.

### Preparation, fallback and diagnostics

1. Preserve the original request/transcript and relevant provenance. Parsing
   must report ingress losses too; inspecting only outbound codecs is too late.
2. Derive a fresh projection from that source for each selected candidate.
   Never feed a previous candidate's lossy projection into fallback.
3. Check compatibility, authority and known losses before dispatch. Required
   fields/blocks cannot disappear silently. Use the same contract for direct
   model calls and router calls; the router owns candidate selection.
4. If no candidate is admissible, return structured incompatibility. Do not
   silently relax policy, reset history or select a fresh conversation.
5. Report conversion diagnostics for ingress, upstream projection and client
   response/stream encoding. Include stage, reason, field/block location,
   target, semantic effect and disposition. Avoid transcript content, opaque
   replay credentials and secrets. Use existing observation/Core recording
   paths, rather than adding a diagnostic database or event framework.
6. Some output losses become known only after upstream execution. Report the
   actual attempt, usage and terminal encoding failure. Stream commitment is
   explicit: do not replay after visible output to make the result look clean.
   Before commitment, fallback still follows existing retry/side-effect policy.

Diagnostics expose losses; they do not make a conversion safe. Define a minimal
report shared by preparation and execution, with gateway/Core mapping above AI.

### Transcript repair and required corrections

Default conversion does not synthesize missing tool results, omit failed or
aborted turns, convert reasoning into answer text, substitute image descriptions
or reset history. Any future repair needs a separate reviewed contract and
caller authority. Protocol packaging, such as citation call/result pairs, must
be distinguished from invented execution results and retain source provenance.

Before moving the affected paths, resolve the audit's two source findings:

- Gemini can parse several tool results into one Tool message; Chat rendering
  currently overwrites all but the last result. Preserve cardinality or reject
  the conversion explicitly.
- Responses rendering groups text/media before standalone items, potentially
  changing interleaved block order. Preserve meaningful order or reject a
  representation that cannot carry it.

These are source-derived findings, not live-provider reproductions. Add focused
assertions for the actual conversion paths; do not freeze suspected loss as a
behavior-preserving contract. Same-protocol round trips do not prove cross-model
replay safety. Authority, ordering and stream cases require their own evidence.

## Migration sequence

| Phase | Work | Completion evidence |
| --- | --- | --- |
| 0. Establish semantics | Classify audit cases; resolve cardinality/order; define diagnostic/admission contract | Focused conversion assertions and reviewed initial rules |
| 1. Extract model semantics | Move types, independent errors and four-protocol codecs; split selected target; agree Core bridge | Single type owner; protocol fixtures; no reverse AI dependency |
| 2. Extract invocation/auth | Move selected-upstream execution, explicit login mechanisms, refresh, cancellation and continuation handling; SDK delegates temporarily | Direct stream/non-stream calls, explicit login fixtures, auth concurrency, failure and commitment evidence |
| 3. Consolidate catalog/integrations | Move provider/catalog runtime and reusable explicit-path storage; retain activation/config, product defaults and Cloud assembly above AI | Explicit load/refresh, offline fallback, override, file compatibility, shared credential coordination and feature evidence |
| 4. Relocate ACP data | Move data/schema/build consumers together, independently of model catalog | Harness config/invocation preserved; independent registry generation |
| 5. Retire old APIs | Migrate known consumers; remove providers crate and obsolete SDK ownership; update release docs | No duplicate types/implementation or standing compatibility facade |

Each phase is reviewable. Types/codecs, selected-target extraction and standalone
HTTP invocation and shared auth/store contracts are implemented locally. Codex,
Claude Code, SuperGrok and Google AI use the shared transaction contract. Hosted
OAuth uses AI's optional `hosted` feature with a specialized transaction retaining
its complete issuer/namespace/scope envelope and the same owned refresh/commit
rules. Native provider request mechanisms are in AI; application/provider glue
retirement and Core mapping are still pending. Catalog schema/lifecycle and application-owned offline
bootstrap/persistence are implemented locally (see the progress document). Do not
combine the entire
migration into a crate rename. Admission enforcement must accompany semantic changes in affected
paths; an intermediate move must not newly accept unclassified losses.
ACP relocation can proceed independently after exact paths are reviewed.

### Revised next steps: retire `bitrouter-providers`

Proceed with provider retirement after Batch 19. The following steps change
ownership by responsibility rather than moving the whole crate into the
application. Batches 20–21 implement explicit login and ordinary file storage extraction
locally; Batch 22 relocates application policy/Cloud assembly and removes the
retired package in the local workspace. External consumer migration/publication
is not verified. The progress
record contains validation evidence and the alpha import/feature changes.

| Original source | Target responsibility |
| --- | --- |
| `oauth/{pkce,auth_code,device_code,listener,login}.rs` | Reusable explicit login mechanisms in AI auth; browser/terminal interaction and command-specific diagnostics stay in the application |
| `oauth/registry.rs` | Provider registration constraints used by AI login/refresh stay with AI provider integrations; CLI display labels and login-method selection stay in the application |
| `oauth/{credential_store,credential_backend}.rs` | Reusable explicit-path file storage and transactions in an opt-in AI backend; default-directory/filename selection and application migration policy stay above AI |
| `hosted/account/flow.rs` | Hosted device authorization, polling, revocation and complete token-envelope construction in AI, accepting explicit inputs rather than application Settings |
| `hosted/account/{credentials,transaction,manager,settings}.rs` | Application Cloud account module retains its current file format/backend, shared-session assembly, login/logout persistence, default scopes and flag/env resolution; AI already owns hosted credential types and session mechanisms |
| `claude_code`, `import`, `antigravity/agy_client.rs` | Application-selected live CLI adoption, permitted environment/file/Keychain/binary discovery and write-back policy; AI keeps injected sessions/refresher mechanisms |
| `apply.rs`, `builtin.rs`, `entry.rs`, `registry/apply.rs`, compiled-in hosted TOML | Application provider configuration/activation bridge; preserve data-driven AI catalog metadata and existing override/reload behavior |

1. **Explicit login mechanisms (Batch 20 implemented locally).** Extract PKCE,
   authorization-code and device-code mechanisms and the registration inputs
   actually used by current callers into AI. Separate loopback/state handling
   and callback-driven protocol orchestration from browser/terminal actions,
   CLI labels and SDK invocation-name errors. Hosted flow extraction uses its
   existing full token envelope; do not unify distinct credential schemas merely
   to remove files. Bound polling, expiration and cancellation, and keep endpoint
   validation and provider-required redirect constraints. Invocation with
   missing credentials must still return an error without starting login.
   Migrate each affected application caller and its tests in the same batch.
2. **Explicit-path storage (Batch 21 implemented locally).** Move the existing ordinary credential file
   backend's reusable reader/writer and compare-and-commit implementation into
   AI behind an opt-in storage feature. Keep XDG/home/default filename selection
   in the application. Preserve current labeled/legacy decoding, permissions,
   atomic replacement, unrelated accounts, pending replacements and canonical
   path identity. Retain the hosted envelope file backend in the application
   Cloud account module for this retirement; keep its existing injected AI
   contract and coordination. Do not create a new storage crate or add unused
   backend abstractions. Neither backend gains cross-process exclusion or crash
   durability merely by changing its location.
3. **Application policy and assembly (Batch 22 implemented locally).** Move the remaining provider
   activation/configuration, credential-source discovery and Cloud account
   glue to application modules. Reuse existing Cloud management and telemetry
   consumers. Construct shared hosted sessions/backends at the application
   boundary so model, management and telemetry use one coordination domain.
   Preserve login/logout races, selected-account authority, zero-config,
   startup/reload, user overrides and saved credentials. A new Cloud/auth crate
   requires demonstrated consumers; none is introduced for this retirement.
4. **Delete the retired package after migration (Batch 22 workspace deletion implemented locally).** Inventory workspace and known
   external consumers, update imports/tests/features, then remove the package,
   dependency/lockfile entries and obsolete CI/release/doc references. Verify
   that no active code depends on the old crate and no forwarding facade or
   duplicate implementation remains. Record alpha breaking imports/features
   and any actual CLI changes with the required skill/plugin updates.

Deletion requires AI-only login/invocation checks, isolated default/login/storage/
hosted feature checks, credential migration and refresh/commit regressions,
application login/logout/Cloud/startup/reload coverage and the required workspace
tests, Clippy, formatting, doctests, Rustdoc and distribution checks. Record
known external consumers explicitly: an in-workspace dependency inventory does
not prove downstream migration or package publication.

Core mapping, remaining conversion semantics and ACP relocation continue as
separate work. They are not blanket prerequisites for deleting this package;
any actual dependency on a retired API must be migrated first. Removing the
providers package does not complete obsolete SDK API retirement or AI-12 by
itself. Batch 22 records local post-removal checks. Known external migration, hosted CI
and publication remain unverified.

## Acceptance and validation

The following are implementation requirements, not results of this doc PR.

| ID | Required evidence |
| --- | --- |
| AI-01 | AI-only consumer calls an explicit endpoint/model with credentials in streaming and non-streaming modes; handles errors/cancellation without `Config`, catalog, router, App or orchestrator |
| AI-02 | Core bridge preserves ordered content, tool identity, usage and terminal lifecycle; no model event executes tools or independently commits turns |
| AI-03 | Four-protocol ingress/egress and stream fixtures pass; cardinality/order findings are corrected or explicitly rejected |
| AI-04 | Equivalent/nonessential conversions are eligible; semantic/unknown losses are excluded by default; explicit degradation is scoped; authority/causality cannot be waived |
| AI-05 | Candidate projections leave source history unchanged; fallback starts from source; no admissible target reports incompatibility without reset |
| AI-06 | Diagnostics identify stage/effect/disposition without content or secrets; late failures retain attempt/usage facts and respect commitment |
| AI-07 | Explicit/stored/ambient priority is demonstrated; failed stored auth does not switch accounts/keys; missing auth never triggers login |
| AI-08 | Concurrent refresh, rotation during cancellation and persistence failure preserve credential authority and report actual durability |
| AI-09 | Offline bootstrap, explicit network refresh, retained snapshot on failure, overrides and disabled entries work; invocation never refreshes catalog implicitly |
| AI-10 | Relocated ACP data preserves runtime/harness distinctions, config synthesis and package-local generation; model/provider tooling works independently |
| AI-11 | AI has no SDK, Core, orchestrator or app-config dependency; default features do not pull in Axum, ACP, MCP or pipeline runtime |
| AI-12 | Workspace/known downstream consumers migrate; old ownership is deleted; breaking changes and affected docs/skills are recorded |

Preserve existing router fallback, settlement, account selection and authorized
continuation behavior, except separately reviewed corrections. Move existing
regression coverage with its implementation; add focused cases for new contracts.

Source implementation PRs run `cargo nextest run --all-features` (or
`cargo test --all-features`), `cargo clippy --all-features` and
`cargo fmt -- --check`. Registry/tooling changes additionally run
`cargo run -p dist-helper -- registry validate`,
`cargo run -p dist-helper -- registry build` and
`cargo run -p dist-helper -- check`, committing generated artifacts as needed.
Report local tests, hosted CI and real-provider evidence separately.

Review of the design itself requires reference review, local link checks and
`git diff --check`. Implementation changes additionally require the source
checks above; record their scope and evidence in the progress document.

## Remaining items for review

| Item | Concrete review output | Blocks |
| --- | --- | --- |
| Core native integration | Map model semantics/events to the developing Core contract; select shared type and bridge locations | Core consumer migration |
| Initial conversion rules | Classify each audited loss, including reasoning continuity and protocol packaging; specify minimal diagnostics and explicit degradation policy | Admission implementation |
| ACP source/artifact locations | Select application/ACP-owned paths and generator inputs while retaining data-driven behavior | Deleting current registry inputs |
| Store/resolver contract | Define selected account identity, atomic rotation/persistence, cancellation bounds and continuation authority after refresh | Auth extraction |
| Login/storage extraction | Separate reusable explicit login and ordinary explicit-path backend from CLI interaction/default paths; preserve hosted envelope storage and shared coordination | Providers package removal |
| Consumer inventory | List workspace and known external callers and their breaking migration steps | Retiring old public APIs |

The exact Rust API follows these contracts and current callers. No additional
protocol standard, storage framework, transcript repair engine or routing
optimizer is required to begin the refactor.
