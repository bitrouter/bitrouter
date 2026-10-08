# Classifier API migration

Date: 2026-10-08. This alpha source migration implements the accepted
[classifier spec](CLASSIFIER_API_SPEC.md). Native wire names and paths remain
`decisions` and `systemone`. The semantic operation is `classification`.

## Public source changes

| Previous surface | Current surface |
| --- | --- |
| `bitrouter_ai::decisions::Decision*` | `bitrouter_ai::classifier::Classifier*` |
| `ModelClient::decide` | `ModelClient::classify` |
| `ModelOperation::Decisions` | `ModelOperation::Classification` |
| `bitrouter_sdk::language_model::*` | `bitrouter_sdk::model_call::*`, importing each type from its owning submodule |
| `AppBuilder::language_model`, `AppState.language_model` | `model_call` |
| `PipelineInput::Decisions`, `PipelineOutput::Decisions` | `Classification` variants |
| `new_decisions`, `decision_request`, `decisions` | `new_classification`, `classifier_request`, `classification` |
| Executor Decisions preflight/execution | Classification preflight/execution |
| `OperationScope::Decisions` | `OperationScope::Classification` |
| Checker `ContentFragmentKind::Decision*` | `Classifier*` |

The SDK's lifecycle module exports submodules rather than their components.
Import, for example, `model_call::pipeline::Pipeline`,
`model_call::builder::PipelineBuilder`, `model_call::routing::RoutingTable` and
`model_call::types::PipelineRequest`. Generation semantic types remain owned by
`bitrouter_ai::types`; there is no unused `LargeLanguageModel` wrapper.
Root convenience exports for `RoutingTable`, `RoutingTarget`, `FallbackPolicy`,
`PreRequestHook` and `HookDecision` are removed in favor of owning modules.

Classifier's serialized canonical representation is distinct from either native
HTTP schema. `ClassifierInput` is tagged to preserve structured arrays that
resemble user-message arrays. Use the selected classifier codec to produce an
HTTP body. Optional instructions and criteria preserve missing/null distinctions.
System One map keys remain distinct from optional OpenAI names and canonical
question position. `source_protocol` and the SDK inbound protocol must agree.

## Stored and configured values

| Surface | Migration rule |
| --- | --- |
| AI `ModelOperation` | Deserialize legacy `decisions`; serialize `classification` |
| SDK `OperationScope` in hosts/checkers | Deserialize legacy `decisions`; serialize `classification` |
| Checker fragment kinds | Deserialize legacy `decision_*`; serialize `classifier_*` |
| Protocol configuration, frozen tariffs and outbound telemetry | Continue using native `decisions`; add native `systemone` |
| Semantic request/hop telemetry | Span schema v3 emits `classification`, classifier content keys and explicit partial-usage availability; query consumers must include historical `decisions` records |
| Usage evidence in settlement and exported metering records | Optional `UsageAvailability` distinguishes unavailable breakdowns; absence retains existing full-breakdown semantics |
| Charge evidence stored in the existing JSON column | Add optional `billable_input_tokens` and availability; no SQL schema migration |
| Workflow archive charge verification | Recompute input-only evidence using its explicit billable input units and frozen rates |

`OperationScope::Both` explicitly includes only Generation and Classification.
Default registrations remain Generation. Hosts must opt shared protections into
Classification rather than broadening generation-only hooks accidentally.

Readers of partial usage must not interpret numeric zero placeholders as
reported cache/reasoning counts. `Usage::normalized_buckets` rejects unavailable
breakdowns. System One tariffs use their reported billable input total separately;
they do not inherit generation rates or ignore unsupported nonzero rates.

## Workspace and external consumers

Workspace AI, SDK, app, telemetry, TUI and regex-checker consumers migrate in this
change. Current development documentation and executable examples use the new
source paths. Earlier Decisions progress/acceptance files describe their dated
baseline and remain historical evidence.

The read-only Cloud inventory was refreshed at
`/Users/kelsen/Documents/Code/bitrouter-cloud`, commit
`184c1f2e9816edc7717ebe8e91d2a5aaf3d95a96`. Its manifest pins SDK alpha.30 and
its `src/server_tools`, policy and route-hook consumers still import
`language_model`, including old generation-owned convenience exports. It needs
coordinated dependency/import/payload/hook-scope migration before adopting this
SDK. This change does not edit, compile or deploy Cloud, and its earlier
Decisions inventory is not evidence of current classifier readiness.

Product API documentation, translations and public catalog publication belong
in `bitrouter-docs`. The internal spec, updated shipped skill and committed OSS
catalog do not establish hosted Cloud availability.
