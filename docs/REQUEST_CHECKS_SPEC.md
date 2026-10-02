# Router request checks

Current contract: Beta uses compiled Rust extensions only. The former HTTP
checker transport, request-check receipts, checker inventory, dedicated control
routes and `bro checks` command are not part of this contract. See
[ROUTER_EXTENSION_SPEC.md](ROUTER_EXTENSION_SPEC.md) for scope and migration;
historical implementation results remain in
[the acceptance ledger](GUARDRAILS_EXTENSION_ACCEPTANCE.md).

A user selects a named router; its configured input checks run before model
selection. Checks return allow/deny, not model choices or rewritten requests.
The SDK author API is `bitrouter_sdk::extension::ExtensionApi`; callbacks use
`extension::request_check` business types without an HTTP envelope. A custom
host compiles, registers and activates implementations. The official `bro`
binary has no custom registrations.

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
`RequestCheckCoverage` live in `extension::request_check` alongside `Input`.
Projection, frozen bindings and runner results remain in the language-model
module.

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

Legacy SDK Plugin/global and stream/output hooks remain explicit custom-host
assembly interfaces. This input-only capability does not replace their broader
scope.
