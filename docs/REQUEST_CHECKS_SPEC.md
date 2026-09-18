# Router request checks and process-local receipts

Current contract: Beta uses compiled Rust extensions only. This supersedes the
original batch 3–5 HTTP checker design. See [ROUTER_EXTENSION_SPEC.md](ROUTER_EXTENSION_SPEC.md)
for scope, migration and acceptance gates; previous test runs remain historical
in [the acceptance ledger](GUARDRAILS_EXTENSION_ACCEPTANCE.md).

A user selects a router; its configured input checks run before existing model
selection. Checks return allow/deny, not model choices or rewritten requests.
The SDK author API is `bitrouter_sdk::extension::ExtensionApi`; callbacks use
`extension::request_check` business types without an HTTP request envelope.
The custom host compiles, registers and activates implementations. Configuration
and bindings are startup-owned; saved changes require restart. Local and remote
management query the selected daemon's authority, not the client environment.
Custom hosts use `bitrouter::host::serve_with_extensions` for that complete
foreground lifecycle; low-level embedding remains available through
`assemble::build_app_with_extensions`.

## Entry preparation and extension contract

Streaming and non-streaming requests share one entry-preparation path. It owns
stage-specific failures and settlement policy; only execution and delivery
branch by response mode.

| Extension point | Selector/defaults contract |
| --- | --- |
| `pre_resolution_hook` | Local authentication and session normalization may change the ingress selector before its router/check binding is frozen. No registered check has run. |
| `router_preparation_hook` | A checked router may select a candidate recipe here, before effective defaults are applied. Its original logical identity and checks remain fixed. |
| `pre_request_hook`, checked ingress | Receives the effective defaults. Deny/error stops execution; selector mutation is explicitly rejected before checker/model dispatch. |
| `pre_request_hook`, unguarded ingress | Retains the legacy model-rewrite contract. Final selector resolution and defaults follow these hooks, so defaults from the original selector are not mixed into the replacement. A rewrite cannot introduce a checked router after admission. |
| model selector | Runs only after required checks allow and uses the effective selection policy. It cannot replace the original logical router/check binding. |

The unguarded legacy path retains its previous defaults timing; it does not
claim that ordinary pre-request hooks inspected defaults applied afterward.
Use the checked-router preparation path when defaults must participate in the
entry content-check contract. A denied/failed hook does not promise rollback
of its request-local changes.

### Host runner boundary

`RequestCheckerRunner::check(binding, input, reporter)` receives a frozen
`RequestCheckBinding`, the same `extension::request_check::Input` used by callbacks,
and a receipt-owned progress reporter. Request, router and invocation identities
stay in the pipeline/receipt; there is no serialized invocation envelope.
The host returns `CheckerResult { decision, revision }`, reusing the callback's
`Decision` and attaching the registered revision. The pipeline validates the
decision once (denial codes: 1–64 ASCII letters, digits or `. _ - :`) before
recording allow/deny. Invalid decisions fail closed without recording a revision
as evidence of a valid decision. Callbacks cannot supply revisions or write receipts.

This replaces the beta SDK's `CheckerInvocation` and `CheckerDecision` Rust API.
Custom runner implementations must migrate; ordinary `ExtensionApi` callbacks,
configuration and management JSON are unchanged. The checked/unguarded hook
ordering and configuration reload contracts are unchanged.

## Coverage and execution limits

Checks receive effective entry text and explicit coverage: system instructions,
message text/reasoning, existing tool arguments/results and approval reasons.
Media is reported as uncovered. Generated output, subsequent harness tool turns,
file bytes and nested calls are outside this contract. Oversized inputs are
rejected, never truncated. JSON tool results are serialized within the remaining
projection budget. No wire serialization is required to invoke a callback.

At most 16 ordered bindings per router and 32 concurrent callbacks per instance.
Input defaults to 256 KiB, capped at 4 MiB and 4,096 fragments. Per-binding wait
deadlines default to 500 ms and cap at 30 seconds, including semaphore wait and
callback execution wait. Synchronous callbacks use the blocking pool. Timeout
and cancellation cannot kill started work or release its permit before it ends.
Deny, timeout, invalid decisions and execution failures stop model dispatch.
This is trusted in-process code, not a sandbox or process-abort isolation layer.

## Configuration

```yaml
checkers:
  company:
    native:
      revision: company-rules-v1
```

Bind the id through `routers.<id>.checks.request` and register the same id and
revision through ExtensionApi. Missing or mismatched registrations block
activation even for unbound configured instances; an ignored registration error
invalidates the registry. Valid registrations absent from configuration remain
inactive with sorted startup diagnostics, no execution state and no inventory
entry. Configured unbound instances appear in inventory without usage. Default bro
has no custom registrations. `bro config validate` verifies declarations, not
which extension code a custom binary contains. Former HTTP connection fields
fail explicitly with migration guidance, including when mixed with Native data.

Author types `ContentRole`, `ContentFragmentKind`, `ContentFragment`,
`RequestCheckCoverageScope`, `RequestCheckCoverageStatus` and
`RequestCheckCoverage` now live in `extension::request_check` alongside `Input`.
Their former `language_model::request_checks` import paths are removed without
aliases. Projection, host bindings and execution results remain in the language
model module; serialized fields are unchanged.

## Receipts and failure guarantees

A receipt is admitted after local authentication/session normalization and
successful named-router binding, before local policy and registered request
checks. Those admitted early rejections are queryable. Malformed ingress,
authentication failure, unresolved routers and direct model requests do not
fabricate a named-router admission receipt.

Receipts are independent of optional telemetry exporters and metering records.
They record immutable router/binding identity, checker invocation and registered revision, bounded coverage/results, whether upstream dispatch began, execution
outcome, and what the server knows about delivery. An allow result does not
mean generation or delivery succeeded. Implementation revisions are supplied by the custom host, not cryptographic attestation.

Only the current daemon process is covered. The default store holds at most
4,096 records and expires completed records after 15 minutes; capacity pressure
may remove completed records earlier. Active records are never evicted. Admission
reserves capacity before checker/model dispatch and fails closed when no slot
can be reserved. Completion does not require allocating another record.
Each admission receives a unique receipt id. Transport retries may reuse a
request id without overwriting prior receipts. Lookup by request id returns the
newest retained attempt with an explicit retained-match count; list queries
retain the separate attempt records. Lookup by the unique receipt id returns
that exact retained attempt.

Admission order is the single source for newest-first listing and request-id
matching. The latest-started-check index points into retained receipts; it is
preserved separately so eviction cannot revive an older observation. Running
checker bindings are stored once, with management inventory projected at query
time rather than cached as a second configuration snapshot. Repeated bindings
retain their declared order and remain distinct invocations.

Queries report process incarnation and retention limits. Records from another
process are unavailable. Missing records do not prove non-execution or success;
when expiration cannot be established, the result remains unknown. Restart or
crash discards these receipts. This is not durable workflow storage or recovery.
Receipts do not retain prompts, answers, or credentials.

Cancellation and early exit must finish or mark an admitted receipt incomplete.
If execution has already started, bookkeeping failure cannot undo the upstream
call. Delivery states describe server-observable boundaries, never proof that a
client application consumed the output.

## Diagnosis

`bro checks` reports running instances, declared revision, bindings and actual
use, alongside saved/running/restart state. There is no connectivity/protocol
probe, endpoint fingerprint or credential readiness for compiled extensions.
`bro checks receipts` and `bro checks receipt <request-id-or-receipt-id>` retain
their process-local meaning and existing access controls.

Usage comes from the latest started invocation still represented in the receipt
store for this binding. Queued/in-flight calls are pending; cancellation is
interrupted and does not claim the callback stopped. An older completion cannot
replace newer evidence; expiring the latest receipt cannot revive an older allow.
No binding means no content check, not successful protection. Registration is
startup readiness evidence; only an invocation receipt proves actual use.

Legacy SDK Plugin/global and stream/output hooks remain explicit custom-host
assembly interfaces. This input capability does not replace their broader scope.
