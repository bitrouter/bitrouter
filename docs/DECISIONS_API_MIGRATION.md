# Decisions API SDK migration inventory

This stack introduces alpha public API changes. The contract is
[DECISIONS_API_SPEC.md](DECISIONS_API_SPEC.md), and validation status lives in
[DECISIONS_API_PROGRESS.md](DECISIONS_API_PROGRESS.md). This inventory is not
evidence of hosted Cloud support.

## Payload and accessor changes

| Previous surface | Migration |
| --- | --- |
| `PipelineRequest.prompt` | Use `PipelineRequest::new` for generation or `new_decisions` for native calls; literals use `input: PipelineInput::Generation(Box::new(prompt))` or `Decisions(request)` |
| `PipelineContext::prompt()` | Generation consumers use fallible `require_generation_prompt()?`; shared consumers inspect `generation_prompt()` or `decision_request()` |
| Direct `ExecutionResult.result` / `PipelineResponse.result` generation fields | Extract `PipelineOutput::Generation` or use `generation()`; shared accounting uses `usage()`; native consumers use `decisions()` |
| Generation parameter/default mutation | Propagate its `Result`; native input cannot accept generation defaults or reasoning effort |
| Infallible context response construction | Propagate `response()` / `into_response()` errors; missing execution cannot synthesize a generation result |
| `SettlementContext` literals | Supply `operation`; retain actual `target` evidence; native calls have no generation finish reason or first-token observation |
| `RoutingTarget` literals | Supply `chat_google_extensions`; native/ordinary fixtures use `false`, while verified Google Chat targets preserve the parent's explicit support declaration |
| Provider-model literals | Add independent `pricing_by_protocol` maps; ordinary `pricing` remains the generation fallback |
| Pricing literals | Supply optional `endpoint_profile` provenance or use the default; declared global/regional rates cannot rebind to another endpoint |
| Server-config literals | Supply `require_known_pricing: false` or use the default; app-level strict price coverage is opt-in |

Native custom executors implement `preflight_decisions` and `execute_decisions`.
Their successful typed output is validated before success hooks. Completed native
failure must preserve usable usage and must never enter fallback, even when a
custom fallback policy would otherwise retry.

## Hook and host assembly changes

Existing registrations and builders remain generation-scoped by default. A host
enabling native calls uses `served_operations(OperationScope::Both)` and explicit
`*_for` registration scopes. Independent `require_hook::<ConcreteHook>` declarations
name required protections and stages; registering a scoped hook alone does not
declare the host's security requirement.

Shared auth, principal/session identity, rate/spend policy and reserved-ID guards
must cover native calls. Generation tools, continuation, parameter defaults and
predictive selectors keep their generation scope. Request-check runners declare
operation support and inspect native fragments without reading safety identifiers.

The OSS app installs `MeteringRecorder::tariff_capture` at the route stage and
requires `CaptureTariffs` independently for both operations. This capture runs
after every mutable route hook. Settlement uses the matching admitted snapshot;
custom app hosts must install the capture when composing this recorder. SDK-only
hosts retain the legacy live table lookup unless they provide a frozen
`UsagePricingSnapshot`, whose explicit unknown price disables that lookup.

Deployment-owned billing hosts supply their own frozen tariff authority rather
than depending on the OSS app's SQLite metering module. Preserve exact protocol
prices, endpoint applicability, cache uncertainty and original admission evidence
through their exports and corrections.

## In-tree inventory

The migration covers AI direct invocation; SDK context, executor, routing,
pipeline, server and server tools; app session identity, continuation, policy,
policy locks, evolution costs/runtime/catalog and workflow observers; and
telemetry exporters. Generation codec, continuation, tool-loop and settlement
assertions are retained. Current evidence and remaining acceptance are recorded
in the progress ledger rather than inferred from compilation.

## External consumer: bitrouter-cloud

Read-only inventory on 2026-10-06 at local commit
`184c1f2e9816edc7717ebe8e91d2a5aaf3d95a96` in
`/Users/kelsen/Documents/Code/bitrouter-cloud`:

- `Cargo.toml:15` / `Cargo.lock:345` resolve the published alpha.30 SDK;
  guardrails and telemetry likewise use alpha.30. This checkout has not adopted
  the parent extraction. Parent-specific migration remains separate from this
  Decisions stack.
- `src/main.rs:418` assembles the generation pipeline. Native enablement requires
  explicit shared principal/authorization/policy/settlement requirements and
  coverage before it can safely serve Decisions.
- `src/v1/capability_usage.rs:43`, `src/policy/hook.rs:317` and `:528`,
  `src/server_tools/declarations.rs:25` and `src/server_tools/billing_guard.rs:280`
  use generation prompt access. Mixed shared protections need operation-aware
  logic; generation-only declarations/billing guards remain scoped.
- `src/service/provider_router/request_mode_executor.rs:54` and admission/preset
  executor wrappers need explicit native support if enabled. Keep the existing
  deadline/admission behavior and completed-failure usage contract.
- `src/v1/settlement.rs:944` constructs settlement contexts; generation result
  assertions at `:2455`, `:2568` and `:2700` need typed extraction. Shared
  settlement, receipts and billing snapshots need actual wire/profile evidence.

No Cloud files were changed, and no Cloud compilation or deployment was
performed. The untracked `lib/` tree was preserved. This stack's local gateway
evidence does not establish Cloud migration or protocol availability. The Cloud
upgrade requires a coordinated change after its parent SDK migration; the
generation-scoped default prevents implicit native enablement on upgrade.

Product API prose and the generated public catalog belong in `bitrouter-docs`
(`content/docs/`). Coordinate that repository's English/Chinese and catalog
refresh workflow after the committed catalog is updated. The internal spec and
shipped skill are not published product API documentation.
