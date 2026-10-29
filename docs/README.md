# BitRouter docs

This folder holds **internal development docs** — the CLI reference, the
workspace architecture guide, and design specs. It is *not* published anywhere.

First-party TUI/Conversation UX, rendering and navigation documents are historical
after CLI-only G05. Their implementation labels describe those earlier stages;
the CLI-only contract and implementation record define the current product.

## Contents

- [Gateway multi-instance contract](GATEWAY_MULTI_INSTANCE_SPEC.md),
  [progress ledger](GATEWAY_MULTI_INSTANCE_PROGRESS.md), and
  [final acceptance record](GATEWAY_MULTI_INSTANCE_ACCEPTANCE.md) — implemented
  P1 shared PostgreSQL authority, admission, recovery, credentials and trajectory
  guards. Local acceptance is complete with recorded performance costs; the
  acceptance record links raw evidence, hosted results and delivery limits.

- [Service refactor source inventory](BITROUTER_SERVICE_REFACTOR_INVENTORY.md) —
  revision-scoped reconciliation of baseline modules with actual destinations;
  coverage and remaining gaps are explicit, separate from acceptance results.

- [`DISTRIBUTION.md`](DISTRIBUTION.md) — full and gateway profiles, archive
  identity/checksums, profile-preserving updates and packaged SDK acceptance.
- [`BITROUTER_SERVICE_REFACTOR_PLAN.md`](BITROUTER_SERVICE_REFACTOR_PLAN.md) —
  **P0 integration tracked in [#984](https://github.com/bitrouter/bitrouter/pull/984).** Complete package/module
  migration inventory, lightweight AI contracts, HTTP SDK, application-owned
  telemetry, feature dependencies, P0 single-instance delivery and acceptance,
  capability-aware daemon compatibility, profile-preserving updates, and the
  separate P1 gateway multi-instance milestone.
- [`BITROUTER_SERVICE_REFACTOR_PROGRESS.md`](BITROUTER_SERVICE_REFACTOR_PROGRESS.md)
  — P0 baseline, implementation stages, review evidence and acceptance ledger.

- [`CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md`](CLI_ONLY_AND_WORKFLOW_SKILLS_SPEC.md) —
  **current convergence contract.** Retains native/ACP orchestration and resource
  capabilities behind a CLI, removes first-party TUI entry points, and defines
  daemon auto-start, connected-agent inspection, simpler onboarding, MCP CRUD,
  direct local skill directory operations and explicit resource selection; acquisition is external.
- [`CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md`](CLI_ONLY_AND_WORKFLOW_SKILLS_GOALS.md) —
  Completed G00–G11 stage index and validation rules; full execution cards and
  copyable prompts remain linked to their immutable Git revision.
- [`CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md`](CLI_ONLY_MCP_SKILLS_ACCEPTANCE.md) —
  A27/A28 and M01–M10 distinguish gateway protocol checks from native host loading,
  approval, origin and cache requirements against a pinned specification;
  M01–M10 have local gateway/native evidence, including verified support files,
  independent consent, actual model inputs, budgets and original cache recovery.
- [`CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md`](CLI_ONLY_AND_WORKFLOW_SKILLS_IMPLEMENTATION.md) —
  Concise stage outcomes, key regressions and immutable implementation history.
  Current verification and immutable original G00–G11 history are linked separately.
- [`CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md`](CLI_ONLY_AND_WORKFLOW_SKILLS_G11_ACCEPTANCE.md) —
  Current directory/explicit-selection verification, retired behavior and separate external qualifications.
- [`CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md`](CLI_ONLY_AND_WORKFLOW_SKILLS_G00_AUDIT.md) —
  Source and dependency baseline, execution identities, existing protocol operations,
  TUI consumers and retained business-test mappings for G01–G05; no runtime changes.
- [`ORCHESTRATOR_CORE_SPEC.md`](ORCHESTRATOR_CORE_SPEC.md) — **v1.0, frozen
  implementation contract; implementation in progress.** Core-owned model/context routing
  and agent scheduling, harness-owned tools and durable state, managed
  multi-agent API, recovery protocol, stages and acceptance criteria.
- [`ORCHESTRATOR_CORE_IMPLEMENTATION.md`](ORCHESTRATOR_CORE_IMPLEMENTATION.md) —
  Stage plan, independent review findings, validation evidence and remaining gates.
- [`ORCHESTRATOR_CORE_ACCEPTANCE.md`](ORCHESTRATOR_CORE_ACCEPTANCE.md) —
  A01–A23 source/test evidence and remaining core/harness integration exit criteria.
- [`BITROUTER_AI_REFACTOR_SPEC.md`](BITROUTER_AI_REFACTOR_SPEC.md) — **historical
  extraction baseline; target ownership superseded by the service refactor plan.**
  Recorded Core native integration, conversion admission, catalog/auth boundaries,
  data-driven ACP relocation and alpha API migration. Behavioral/fidelity
  requirements remain applicable.
- [`MODEL_HISTORY_COMPATIBILITY_AUDIT.md`](MODEL_HISTORY_COMPATIBILITY_AUDIT.md) —
  Baseline model-history replay, conversion, omission and synthesis rules;
  evidence and boundaries for the proposed `bitrouter-ai` extraction.
- [`BITROUTER_AI_REFACTOR_PROGRESS.md`](BITROUTER_AI_REFACTOR_PROGRESS.md) —
  Implemented extraction batches, breaking import migration, validation evidence
  and remaining work against the AI spec.
- [`DECISIONS_API_SPEC.md`](DECISIONS_API_SPEC.md) — **v0.2, design direction
  accepted; implemented and verified locally, in CI and with bounded API-key calls.**
  First-class OpenAI Decisions support
  stacked on #962: typed calls, operation-compatible routing, shared lifecycle
  and protocol pricing. Decision-driven routing policy remains a separate PR.
- [`DECISIONS_API_PROGRESS.md`](DECISIONS_API_PROGRESS.md) — Local batch evidence
  and remaining gateway/provider acceptance for the stacked Decisions change.
- [`DECISIONS_API_MIGRATION.md`](DECISIONS_API_MIGRATION.md) — Alpha payload,
  hook and pricing migration, with the read-only external Cloud inventory.
- [`DECISIONS_API_ACCEPTANCE.md`](DECISIONS_API_ACCEPTANCE.md) — Requirement-by-requirement
  local/hosted/live evidence and qualified provider/Cloud delivery boundaries.
- [`GEMINI_PROTOCOL_RETIREMENT_SPEC.md`](GEMINI_PROTOCOL_RETIREMENT_SPEC.md) —
  **approved; implementation in progress.** Removes native Gemini Generate Content ingress and
  upstream support, retains metered Gemini through Chat Completions, and defines
  provider retirement, Antigravity SDK connectivity, and replacement validation.
- [`LOCAL_DAEMON_UPGRADE_SPEC.md`](LOCAL_DAEMON_UPGRADE_SPEC.md) — **implemented
  in #932; local and CI verification passed.** Safe local daemon handoff after
  a CLI upgrade: version and capability detection, idle-only restart,
  migration preflight, and truthful recovery states.
- [`GUARDRAILS_EXTENSION.md`](GUARDRAILS_EXTENSION.md) — Independent input checker
  setup, migration boundaries, distribution and process-level validation.
- [`GUARDRAILS_EXTENSION_ACCEPTANCE.md`](GUARDRAILS_EXTENSION_ACCEPTANCE.md) —
  Dated local implementation and test evidence, plus the release update.

- [`ROUTER_EXTENSION_SPEC.md`](ROUTER_EXTENSION_SPEC.md) — **v0.7, compile-only
  extensions merged in #923 and shipped in v1.0.0-alpha.33.** Current router,
  SDK author API, execution and migration contracts.
- [`HOST_EXTENSION_DX_SPEC.md`](HOST_EXTENSION_DX_SPEC.md) — **v0.3, S1–S4
  merged in #923 and shipped in v1.0.0-alpha.33.** Shared foreground host startup,
  capability-owned author types and inactive unused registrations; see the
  acceptance evidence for the validation scope.
- [`CONFIGURATION_STATE_CONTRACT_SPEC.md`](CONFIGURATION_STATE_CONTRACT_SPEC.md) —
  **implemented and locally verified.** Whole-configuration saved/running/restart
  evidence shared by local and remote status, CLI, and Code inspectors.
- [`ROUTER_PRESET_MIGRATION_SPEC.md`](ROUTER_PRESET_MIGRATION_SPEC.md) —
  **implemented and locally verified in PR #916.** Router/preset configuration
  and identity migration sub-batch; not completion of the original M0–M1 batch.
- [`CLI.md`](CLI.md) — full command reference, flags, and config resolution.
- [`DEVELOPMENT.md`](DEVELOPMENT.md) — workspace architecture and SDK internals.

## BRO current contracts and evidence

Start with [the standalone runtime contract](BRO_AGENT_RUNTIME_SPEC.md).
It incorporates implemented Thread/Turn behavior; historical documents do not
supply additional overrides. Contract scope and acceptance scope are separate.

| Subject | Document |
| --- | --- |
| Runtime identities, scheduling, context, durable commits, recovery and transport | [BRO_AGENT_RUNTIME_SPEC.md](BRO_AGENT_RUNTIME_SPEC.md) |
| Six tool interfaces, filesystem bounds and interpreter rules | [BRO_BASE_TOOLS_SPEC.md](BRO_BASE_TOOLS_SPEC.md) |
| AGENTS.md scopes and snapshots, native MCP and skills discovery | [BRO_HARNESS_RESOURCES.md](BRO_HARNESS_RESOURCES.md) |
| Native Conversation and durable Thread navigation | [BRO_CONVERSATION_UI_SPEC.md](BRO_CONVERSATION_UI_SPEC.md) |
| Source-specific validation and historical checkpoints | [BRO_AGENT_RUNTIME_IMPLEMENTATION.md](BRO_AGENT_RUNTIME_IMPLEMENTATION.md) |
| Tool provider/platform experiments and reproduction | [BRO_BASE_TOOLS_ACCEPTANCE.md](BRO_BASE_TOOLS_ACCEPTANCE.md) |
| Final Conversation/main fixture acceptance | [BRO_CONVERSATION_UI_IMPLEMENTATION.md](BRO_CONVERSATION_UI_IMPLEMENTATION.md) |
| Raw exports, summaries and archive integrity | [Evidence README](evidence/bro-base-tools/README.md) |
| Future core/harness separation, outside standalone acceptance | [BRO_AGENT_RUNTIME_HANDOFF.md](BRO_AGENT_RUNTIME_HANDOFF.md) |
| Native ACP v1/draft v2 ingress, approval reattachment and durable cancel/close semantics; implemented locally | [BRO_NATIVE_ACP_SERVER_SPEC.md](BRO_NATIVE_ACP_SERVER_SPEC.md) |
| Native ACP implementation evidence and remaining gates | [BRO_NATIVE_ACP_SERVER_ACCEPTANCE.md](BRO_NATIVE_ACP_SERVER_ACCEPTANCE.md) |

Old [native server](BRO_NATIVE_AGENT_SERVER_SPEC.md),
[shared-session](BRO_SHARED_SESSION_SERVER_SPEC.md) and
[Thread/Turn migration](BRO_THREAD_TURN_UNIFICATION_SPEC.md) pages are short
historical pointers to immutable Git snapshots. Their implementation ledgers
are indexed from current runtime acceptance. External agent ACP sessions retain
the separate controller contracts below; the native ACP ingress uses the same runtime. Its [acceptance record](BRO_NATIVE_ACP_SERVER_ACCEPTANCE.md)
distinguishes local fixtures from IDE/platform/provider gates.

## Other development contracts

- `*_SPEC.md` / `*_ACCEPTANCE.md` — design specs and acceptance criteria for
  in-flight work (spawn/launch, onboarding, the MCP `2026-07-28` upgrade,
  skills over MCP, the observability TUI, the ACP TUI, the ACP controller,
  the agent registry).
- [`ACTIONS_SPEC.md`](ACTIONS_SPEC.md) — **phases 0–4 implemented; phase 5
  proposed.** One actions table so the CLI, Code, and typed remote-control
  surfaces that answer the same question share one report type, one
  implementation, and a guard test. Its historical origin-MCP analysis is
  superseded in part. Written for
  [#868](https://github.com/bitrouter/bitrouter/issues/868); stands alone
  from #863 and #866.
- [`ACP_CONTROLLER_SPEC.md`](ACP_CONTROLLER_SPEC.md) — authoritative boundary
  for ACP controller topology, harness-owned sessions, endpoint configuration,
  native identity, and session-scoped routing.
- [`ACP_EVOLUTION_SPEC.md`](ACP_EVOLUTION_SPEC.md) — **implemented; controlled
  serving acceptance passed; historical calibration pending.** Recorded-evidence
  rubric evaluation, checkpoint feedback, batched
  Thompson sampling and session-sticky policy blocks. The
  [experiment report](ACP_EVOLUTION_EXPERIMENTS.md) separates controlled results,
  negative findings and remaining historical-data/product validation. The
  [main integration validation](ACP_EVOLUTION_PR_VALIDATION.md) records the
  `bro`/scrollback integration and final judge lease regression checks.
- [`CODE_TUI_UX_SPEC.md`](CODE_TUI_UX_SPEC.md) — **implemented; locally verified.**
  Replaces the seven-view Code dashboard with a conversation, contextual
  pickers/inspectors, and agent/route/activity/attributed-cost status; defines
  shared interaction behavior, ACP boundaries, and acceptance criteria.
- [`CODE_TUI_CODEX_NAVIGATION_SPEC.md`](CODE_TUI_CODEX_NAVIGATION_SPEC.md) —
  **implemented; locally verified.** Codex-style conversation entry and explicit Left-arrow
  navigation to a native-buffer Agents menu; first delivery covers the menu
  skeleton and includes acceptance criteria and an implementation goal prompt.
- [`CODE_TUI_CODEX_IMPLEMENTATION.md`](CODE_TUI_CODEX_IMPLEMENTATION.md) —
  first-delivery changes, acceptance evidence and live PTY screenshots.
- [`CODE_SLASH_COMMAND_UX_SPEC.md`](CODE_SLASH_COMMAND_UX_SPEC.md) — **implemented
  in #935.** Makes `/` the command input, preserves drafts on cancel,
  removes default action hotkeys, and adds configurable bindings under
  `/hotkeys`.
- [`BACKGROUND_AGENT_UX_SPEC.md`](BACKGROUND_AGENT_UX_SPEC.md) — **implemented;
  locally verified.** Keeps foreground history in native scrollback,
  makes background-agent awareness and routine commands a persistent bottom
  control deck, and reserves alternate screen for full history or complex
  detail while preserving supervisor and child-agent truth boundaries.
- [`BACKGROUND_AGENT_IMPLEMENTATION.md`](BACKGROUND_AGENT_IMPLEMENTATION.md) —
  implementation phases, ownership, and acceptance evidence for the control deck.
- [`AGENT_INTERFACE_UNIFICATION_SPEC.md`](AGENT_INTERFACE_UNIFICATION_SPEC.md) —
  **proposed for review.** Unifies the public agent UX around native
  `claude`/`codex` shortcuts, the `code` TUI, headless `run`, and one raw
  `acp serve` bridge; retires visible `spawn` and keeps sessions harness-owned.
  Its origin-MCP proposal is superseded in part.
- [`OSS_MCP_BOUNDARY_SPEC.md`](OSS_MCP_BOUNDARY_SPEC.md) — **implemented in
  #913.** Replaces the OSS first-party origin MCP with
  the `/bitrouter` Skill plus structured CLI for shell-capable local agents,
  while retaining the MCP gateway, aggregate `/mcp` endpoint, server-side tool
  loop, and Skills-over-MCP relay. Places any multi-tenant BitRouter control
  origin in Cloud rather than this repository.
- [`REMOTE_CONTROL_MVP_SPEC.md`](REMOTE_CONTROL_MVP_SPEC.md) — **implemented.**
  Read-only remote status/models/route/requests and operations inspectors over an
  authenticated, loopback-only HTTP control listener; ACP stays local.
- [`REMOTE_ADMINISTRATION_SPEC.md`](REMOTE_ADMINISTRATION_SPEC.md) — **implemented.**
  Expands remote inspection and adds explicitly authorized reload,
  with shared action metadata, live/disk policy views, partial-failure reporting,
  and recovery after a client disconnect. Remote agent execution stays deferred.
- [`REMOTE_CLI_TUI_SUPPORT_SPEC.md`](REMOTE_CLI_TUI_SUPPORT_SPEC.md) — **Phase 2
  RFD, deferred.** Remote ACP sessions over a versioned WebSocket transport,
  constrained execution, and reconnect behavior.
- [`TELEMETRY_CRATE_SPEC.md`](TELEMETRY_CRATE_SPEC.md) — **the live one.** Why
  the OTLP renderer ships as `crates/bitrouter-telemetry` while `bitrouter-sdk`
  keeps only the contract it renders (`observe::schema`, `SpanAttributes`).
  Start here; the two documents below are its history. Read its *The arguments
  that are dead* section before reopening anything — the crate-count and
  build-cache cases were measured, withdrawn, and are not what decided this.
- [`OTEL_SDK_MIGRATION_SPEC.md`](OTEL_SDK_MIGRATION_SPEC.md) — **D1 superseded.**
  Recorded why the exporter moved *into* `bitrouter-sdk` behind an `otel`
  feature. That placement was reversed before it reached a release. Still the
  best record of the hard constraint, the feature shape, and which names,
  targets and config keys are load-bearing.
- [`OTEL_TIERING_SPEC.md`](OTEL_TIERING_SPEC.md) — proposed splitting that
  module into schema / emission / export tiers. **Phases 0–2 landed and stand**
  (the committed span-schema artifact, the `tracing` bridge kept, the
  OTel-native ingress span); its D1 was withdrawn on measured benefit and then
  reopened on the positioning grounds it had itself reserved. Read its *cloud
  question* section before reopening any of it.
- `*_PLAN.md` — ordered execution plans derived from a spec, with per-task
  completion criteria. [`ACP_TUI_PLAN.md`](ACP_TUI_PLAN.md) is written to be
  driven by `/goal`.

- [`CLI_TUI_PARITY_SPEC.md`](CLI_TUI_PARITY_SPEC.md) — **track 1 implemented
  in #880; historical rationale.** Explains why CLI/TUI parity uses shared
  actions instead of a command-per-command mirror. Covers the selected session
  commands and the `tui_command` / `effect` / `requires` action fields. Its
  origin-MCP portions were superseded by #913.
- [`CLI_TUI_PARITY_IMPL_SPEC.md`](CLI_TUI_PARITY_IMPL_SPEC.md) — **track 1
  implemented in #880; historical build design.** Specifies the shared action
  rows, resolver and guards. Its source paths predate the #913 refactor.
- [`CLI_TUI_PARITY_BUILD_SPEC.md`](CLI_TUI_PARITY_BUILD_SPEC.md) — **executed
  in #880; all ten tasks complete.** The implementation plan and iteration
  protocol, with a resumable ledger
  ([`CLI_TUI_PARITY_PROGRESS.md`](CLI_TUI_PARITY_PROGRESS.md)); retained as a
  record of the completed work.
- [`CLI_TUI_PARITY_PROGRESS.md`](CLI_TUI_PARITY_PROGRESS.md) — **closed at T9.**
  Records what each task landed and the corrections made to the impl spec.

## Where product docs live

The **product** documentation that used to live here now lives in the
**[bitrouter-docs](https://github.com/bitrouter/bitrouter-docs)** repository, under
`content/docs/` — it is authored, reviewed, and published there.

- Edit product docs in `bitrouter-docs`, not here.
- The docs site's `supported-models` table is generated from its committed
  catalog snapshot. Refreshing it reads the public `/v1/models` catalog and
  falls back to this repository's `dist/registry/models.json` only for
  open-weight metadata. There is no generated `supported-providers` table;
  provider discovery lives in the API reference. Keep this repository's
  registry catalog current, then refresh the snapshot in `bitrouter-docs`.
- On each release, an agent in `bitrouter-docs` drafts a docs update from the
  changelog for human review.

- [Real Codex subscription ACP pilot](ACP_SUBSCRIPTION_PILOT.md): controlled tasks, frozen evidence, model-reference comparisons and observed follow-up issues.
- [Rubric v2 fresh-task comparison](ACP_RUBRIC_V2_HOLDOUT.md): four additional subscription task families, revised responsibility semantics, preserved unknown validation and version-compatibility checks.
- [TS goal audit](ACP_TS_GOAL_AUDIT.md): requirement-level evidence for the controlled reward pilot, learner experiments and serving implementation; separate limits on natural-history and live-benefit claims.

- [BRO Conversation and durable Threads navigation](BRO_CONVERSATION_UI_SPEC.md) — PR #952 UI integration into the native Thread client.
- [BRO Conversation UI integration evidence](BRO_CONVERSATION_UI_IMPLEMENTATION.md) — delivered behavior, local gates and remaining verification boundaries.
