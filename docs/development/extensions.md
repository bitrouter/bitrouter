# Compiled extensions and request checks

This guide describes the current author and host interfaces. Serialized router
and checker configuration belongs to the
[configuration spec](../specs/configuration/README.md); operator setup and
breaking import migration are in the [guardrails guide](../GUARDRAILS_EXTENSION.md).

## Author and host ownership

An extension is trusted Rust code linked into a custom host. Register a request
check through `bitrouter::gateway::extensions::registry::ExtensionApi` and
`request_check(id, revision, callback)`. Callback value types live in
`bitrouter_ai::extension::request_check`. Registration collects implementations;
a configured named router decides which bindings execute. Neither registration
nor a Cargo feature alone activates a callback. The default `bro` binary does
not contain custom registrations.

Use `bitrouter::host::serve_with_extensions` to share foreground service startup,
control endpoints, configuration baseline, reload and shutdown. Use
`assemble::build_app_with_extensions` when the embedding host owns its own
lifecycle. The [runnable custom host](../../apps/bitrouter/examples/native_regex_checker.rs)
and [regex author guide](../../extensions/regex-checker/README.md) show the current
API. A configured ID/revision must match a registration even if unbound; valid
unconfigured registrations remain inactive and produce a sorted diagnostic.
Duplicate or invalid registration poisons activation even if its immediate error
is ignored.

There is no remote extension protocol, dynamic loader or permission sandbox.
Changing linked code requires rebuilding the same custom host; changing
startup-bound code/rules or bindings requires its explicit restart. An official
CLI restart must not replace a custom executable and silently lose its
registrations. Legacy application Plugin/global/output hooks retain their own
explicit host assembly and are not replaced by input-only request checks.

## Request entry and checking

The user selects a named router. Checks return allow/deny, not a model choice or
rewritten request. The host freezes router identity and checks before model
selection, and follows the preparation and execution rules below.

## Entry preparation and extension contract

Streaming and non-streaming requests share one entry-preparation path:

| Extension point | Selector/defaults contract |
| --- | --- |
| `pre_resolution_hook` | Local authentication and normalization may change the ingress selector before its router/check binding is frozen. |
| `router_preparation_hook` | A checked router may select a candidate recipe before effective defaults are applied. Its original logical identity and checks remain fixed. |
| `pre_request_hook`, checked ingress | Receives effective defaults. Deny/error stops execution; selector mutation is rejected before checker/model dispatch. |
| `pre_request_hook`, unguarded ingress | Retains the legacy rewrite contract. A rewrite cannot introduce a checked router after admission. |
| model selector | Runs only after all required native checks allow. It cannot replace the frozen logical router/check binding. |

The unguarded path retains its previous defaults timing. Use router preparation
when defaults must participate in the checked entry content.

### Host runner boundary

`RequestCheckerRunner::check(binding, input)` receives a frozen
`RequestCheckBinding` and the same `extension::request_check::Input` used by
callbacks. Request and router identities remain in the pipeline; there is no
serialized invocation envelope or progress reporter. The host returns
`CheckerResult { decision, revision }`, attaching the registered revision rather
than accepting one from callback output.

The pipeline validates denial codes as 1–64 ASCII letters, digits or `. _ - :`.
Invalid decisions fail closed. Callbacks cannot mutate the request, route,
credentials or lifecycle state through this capability.

## Coverage and execution limits

Checks receive effective entry text and explicit coverage: system instructions,
message text/reasoning, existing tool arguments/results and approval reasons.
Media is reported as uncovered. Generated output, later harness turns, file
bytes and nested calls are outside this contract. Oversized inputs are rejected,
never truncated. JSON tool results are serialized within the projection budget.

At most 16 ordered bindings are allowed per router and 32 concurrent callbacks
per instance. Input defaults to 256 KiB, capped at 4 MiB and 4,096 fragments.
Per-binding deadlines default to 500 ms and cap at 30 seconds, including
semaphore wait and callback wait. Synchronous callbacks use the blocking pool.
Timeout and cancellation cannot kill started work or release its permit before
it ends. Deny, timeout, invalid decisions and execution failures stop model
dispatch.

This is trusted in-process Rust code, not a sandbox. The restricted API reduces
coupling but does not prevent an extension from using ambient process or library
capabilities. WASM isolation, dynamic loading and permission manifests are not
implemented by this contract.

## Configuration and activation

```yaml
checkers:
  company:
    native:
      revision: company-rules-v1

routers:
  coding:
    selection:
      kind: model
      model: fixture:model
    checks:
      request:
        - checker: company
          timeout_ms: 500
          max_input_bytes: 262144
```

Register the same id and revision through `ExtensionApi`. Missing or mismatched
registrations block activation even for configured instances without router
bindings. Duplicate or invalid registrations poison the collection even if the
extension ignores the immediate error. Valid registrations absent from
configuration remain inactive, allocate no execution state and produce a sorted
startup diagnostic.

`bro config validate` verifies declarations but cannot prove which callbacks a
custom binary contains. Former HTTP `endpoint`, `credential_env` and
`contract_version` fields fail explicitly with migration guidance. Checker
declaration and router-binding changes require restart; reload does not swap
compiled code or registrations.

Author types `ContentRole`, `ContentFragmentKind`, `ContentFragment`,
`RequestCheckCoverageScope`, `RequestCheckCoverageStatus` and
`RequestCheckCoverage` live in `bitrouter_ai::extension::request_check` alongside `Input`.
Projection, frozen bindings and runner results remain in the application gateway.

## Diagnostics and observability

Activation errors and sorted inactive-registration IDs are emitted during
startup. Each native invocation emits bounded tracing with checker id, registered
revision, elapsed time and a fixed outcome/failure class; prompt text, matched
text, rule names and callback-supplied error text are not logged by this layer.

There is deliberately no request-check inventory or receipt query API. Use
configuration review, startup logs, normal daemon status/restart-required state,
provider dispatch observations and application telemetry appropriate to the
deployment. Absence of a log or telemetry record is not proof that a check did
not run. An allow result also does not prove that model generation or response
delivery succeeded.

Legacy application Plugin/global and stream/output hooks remain explicit custom-host
assembly interfaces. This input-only capability does not replace their broader
scope.
