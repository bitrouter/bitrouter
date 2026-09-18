# Router request checks and process-local receipts

This increment implements original batch items 3–5 on the named-router and
configuration-state contracts. It retains the existing model/policy selection
algorithms. Independent guardrails packaging (item 6) is a separate change;
this document preserves its original 3–5 scope. The subsequent extraction is
documented in [GUARDRAILS_EXTENSION.md](GUARDRAILS_EXTENSION.md); legacy host
configuration now blocks activation rather than being silently ignored.

## Operator contract

A user selects a router. The router supplies request defaults, performs its
configured checks, and delegates model selection to its existing selection
policy. HTTP checkers can return only allow/deny. They cannot change the request,
select a model, invoke a tool, or override host authorization.

The daemon resolves checker credentials on its own machine. Client environment
variables never supply remote daemon credentials. Checker configuration and
router bindings are startup-owned: saved changes require restart and cannot be
activated by reload. All management transports query the selected daemon.

## Entry preparation and extension contract

Streaming and non-streaming requests share one entry-preparation path. It owns
stage-specific failures and settlement policy; only execution and delivery
branch by response mode.

| Extension point | Selector/defaults contract |
| --- | --- |
| `pre_resolution_hook` | Local authentication and session normalization may change the ingress selector before its router/check binding is frozen. No external checker has run. |
| `router_preparation_hook` | A checked router may select a candidate recipe here, before effective defaults are applied. Its original logical identity and checks remain fixed. |
| `pre_request_hook`, checked ingress | Receives the effective defaults. Deny/error stops execution; selector mutation is explicitly rejected before checker/model dispatch. |
| `pre_request_hook`, unguarded ingress | Retains the legacy model-rewrite contract. Final selector resolution and defaults follow these hooks, so defaults from the original selector are not mixed into the replacement. A rewrite cannot introduce a checked router after admission. |
| model selector | Runs only after required checks allow and uses the effective selection policy. It cannot replace the original logical router/check binding. |

The unguarded legacy path retains its previous defaults timing; it does not
claim that ordinary pre-request hooks inspected defaults applied afterward.
Use the checked-router preparation path when defaults must participate in the
entry content-check contract. A denied/failed hook does not promise rollback
of its request-local changes.

## Check coverage and protocol

Contract version 1 projects effective request text: system instructions, message
text/reasoning, existing tool arguments and results, and human approval reasons.
Non-text content is disclosed as uncovered; no file bytes, provider credentials,
management credentials, or arbitrary internal metadata are sent. The coverage is
entry-request text, not generated output, later tool results, nested requests,
or all activity inside a native coding harness.

Inputs exceeding the binding's limit or 4,096 text fragments are rejected rather
than truncated. JSON tool results are serialized within the remaining byte
budget before being copied into the checker projection. The
HTTP deadline includes waiting for a concurrency slot, connection, and response
body consumption. Redirects and automatic retries are disabled. Service errors,
timeouts, malformed or incompatible responses fail closed before model dispatch.
A checker deny remains distinct from a checker infrastructure/protocol error.

Each checker permits at most 32 concurrent calls. Responses are limited to
16 KiB. A router supports at most 16 request-check bindings, executed in order;
first rejection/error stops evaluation and later checks are not executed.
Input limits default to 256 KiB and cannot exceed 4 MiB. Binding deadlines default
to 500 ms and cannot exceed 30 seconds. These are resource limits, not promises
that a checker will return within the default budget.

## Configuration and wire examples

```yaml
checkers:
  company:
    endpoint: http://127.0.0.1:8081/check
    credential_env: COMPANY_CHECKS_TOKEN
    contract_version: 1
routers:
  coding:
    selection:
      kind: policy
      policy: auto
      base_model: coding-base
    checks:
      request:
        - checker: company
          timeout_ms: 500
          max_input_bytes: 262144
```

The example assumes `auto` and `coding-base` already exist. The existing
policy-lock selection algorithm remains responsible for model selection.
`credential_env` is optional for an unauthenticated checker. A configured but
unbound checker can report a missing credential; a router-bound checker missing
its required credential blocks activation.

The host POSTs to the exact endpoint with JSON content type. The versioned
request contains `contract_version`, `invocation_id`, `request_id`, `router_id`,
`router_binding_digest`, `checker` (checker id, binding digest and limits),
`content` (ordered role/kind/text fragments) and `coverage`. The service echoes
the invocation id:

```json
{"contract_version":1,"invocation_id":"<received invocation_id>","decision":"allow","implementation_version":"1.0.0"}
```

A deny response changes `decision` to `deny` and can include a bounded ASCII
`reason_code`. Arbitrary free-text explanations are not accepted as a substitute.
`implementation_version` is optional; absent stays unknown. Unknown response
fields, mismatched invocation id/version and other malformed responses reject
rather than allowing the request. The serialized request envelope has an
additional 8 MiB limit, even when its text is within the binding's input limit.

## Receipts and failure guarantees

A receipt is admitted after local authentication/session normalization and
successful named-router binding, before local policy and external request
checks. Those admitted early rejections are queryable. Malformed ingress,
authentication failure, unresolved routers and direct model requests do not
fabricate a named-router admission receipt.

Receipts are independent of optional telemetry exporters and metering records.
They record immutable router/binding identity, checker invocation and contract
version, bounded coverage/results, whether upstream dispatch began, execution
outcome, and what the server knows about delivery. An allow result does not
mean generation or delivery succeeded. Service-reported implementation versions
are evidence supplied by that service, not attestation.

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

Queries report process incarnation and retention limits. Records from another
process are unavailable. Missing records do not prove non-execution or success;
when expiration cannot be established, the result remains unknown. Restart or
crash discards these receipts. This is not durable workflow storage or recovery.
Receipts do not retain prompts, answers, raw HTTP error bodies, or credentials.

Cancellation and early exit must finish or mark an admitted receipt incomplete.
If execution has already started, bookkeeping failure cannot undo the upstream
call. Delivery states describe server-observable boundaries, never proof that a
client application consumed the output.

## Diagnosis

Configuration validity, saved/running/restart state, connectivity/protocol probe,
and real request usage are separate evidence. A successful synthetic probe does
not populate real usage or request receipts. Probe calls originate at the target
daemon, use a fixed harmless input and the configured credential, and are subject
to the same bounds as normal calls. They never accept an arbitrary caller URL or
prompt. Remote probes require the existing `control:read` administrative scope.

Usage evidence is derived from the receipt store for the running binding and
the latest started invocation that is still retained. Real invocation state
has one owner; the HTTP runtime reports transport progress and does not retain
a second terminal-state machine. Synthetic probes remain separate.
Queued/in-flight calls show pending; cancellation shows interrupted and does
not claim that remote execution stopped. An older completion cannot replace a
newer invocation's observation. Evicting or expiring that latest receipt removes
its inventory evidence; an older retained allow is not substituted. Success for
an earlier binding must not make a saved replacement appear active. Unconfigured checks are shown
as not enabled, not as successful protection.

## Acceptance ledger

The following tests provide the acceptance evidence. App integration tests are
in `apps/bitrouter/tests/request_checks.rs`; the remaining tests live beside the
SDK contracts, HTTP runtime and daemon management implementation.

| ID | Acceptance | Automated evidence |
| --- | --- | --- |
| RC01 | Original router/checker identity survives candidate selection. | `routers_apply_distinct_checks_to_effective_text_before_model_dispatch`; `checked_preparation_freezes_checks_nonstream`; `checked_preparation_freezes_checks_stream` |
| RC02 | Effective defaults are checked; media exclusions and resource limits are explicit. | `routers_apply_distinct_checks_to_effective_text_before_model_dispatch`; `projection_counts_top_level_and_tool_result_media`; `projection_caps_empty_fragments`; `projection_bounds_json_serialization_by_remaining_bytes` |
| RC03 | Rejection or checker failure prevents model dispatch. | `timeout_and_protocol_failure_never_dispatch_a_model`; `oversize_text_is_rejected_without_checker_or_model_dispatch`; `hostile_response_body_is_bounded_and_never_exposed`; `total_deadline_covers_the_response_body` |
| RC04 | Early rejection, cancellation and delivery failure remain queryable without an exporter. | `e2e_full_stack_policy_denies_disallowed_tool_before_request_checker`; `checker_denial_stops_later_checker_and_model_dispatch`; `cancelled_pending_checker_finalizes_receipt_without_executor_dispatch`; `stream_disconnect_and_error_finalize_truthful_receipts`; `failed_receipts_identify_route_upstream_and_delivery_stages` |
| RC05 | Capacity, retry and process boundaries cannot fabricate success. | `active_receipts_are_never_evicted`; `unavailable_store_rejects_admission_and_never_reports_success`; `completed_receipt_is_evicted_for_new_admission`; `old_incarnation_is_unknown_even_when_request_id_matches`; `zero_ttl_expires_completed_but_not_active_receipts`; `transport_retries_keep_separate_receipts_under_one_request_id` |
| RC06 | Invalid references/limits fail validation; missing required credentials block activation. | `request_checker_config_rejects_unknown_refs_and_invalid_limits`; `request_checker_config_rejects_invalid_static_bindings`; `required_missing_checker_credential_blocks_host_activation` |
| RC07 | Checker changes require restart before reload mutates running state. | `checker_connection_edits_require_restart_before_any_reload_mutation` |
| RC08 | Local/remote queries share daemon authority and access controls. | `local_and_remote_checker_receipts_share_one_runtime_authority`; `checker_management_uses_read_authorization` |
| RC09 | Probe success is separate from real use and generation success. | `real_use_and_probe_are_observed_separately`; `cancelled_and_older_invocations_cannot_leave_stale_allow_evidence`; `probe_has_no_request_receipt_and_allow_does_not_mask_upstream_failure` |
| RC10 | Existing router, policy and continuation behavior remains intact. | `named_router_migration_protocol_matrix`; `named_candidate_keeps_preset_defaults_and_tool_safety_selection`; `transport_retry_identity_is_idempotent_without_becoming_a_route_key`; existing continuation suite |

| RC11 | Ordinary hooks retain unguarded rewrite semantics; checked mutations stop before checker/model dispatch in both response modes. | `unguarded_bare_and_legacy_rewrites_converge_nonstream`; `unguarded_bare_and_legacy_rewrites_converge_stream`; `checked_ordinary_mutation_stops_nonstream`; `checked_ordinary_mutation_stops_stream` |
| RC12 | Real-use views share receipt lifecycle and never revive older evidence after eviction. | `reporter_is_monotonic_and_cannot_mutate_terminal_check`; `evicting_latest_started_check_does_not_revive_older_evidence`; `cancelled_and_older_invocations_cannot_leave_stale_allow_evidence` |

## Local validation

The converged implementation was validated on macOS on 2026-09-16:

- Workspace all-feature nextest: 3,456 passed, 22 skipped.
- Workspace all-feature clippy, including tests, with warnings denied: passed.
- Workspace doctests: 5 passed, 1 ignored; strict rustdoc: passed.
- SDK no-default-feature checks: minimal, `config_file`, `server`, and `acp` passed.
- Formatting, diff whitespace, and generated distribution consistency: passed.
- Pinned nightly SDK public-API check: dependency set unchanged; no OTel exposure.

The config schema and shipped CLI/diagnosis references are updated. These are
local results; GitHub CI validates the published PR head separately. Production
deployment verification is outside this local suite. Regression coverage includes
ordinary-hook rewrites, shared stream/non-stream preparation, receipt-owned
transport progress, terminal immutability, and latest-evidence eviction.


## Native execution follow-up

The capability is `request-check`; `regex-checker` is an extension implementing it.
`RequestCheckRuntime` now accepts explicit native registrations alongside HTTP
services. See [the extension guide](../extensions/regex-checker/README.md) for
`checkers.<id>.native.revision` and `assemble::build_app_with_checkers`.
The same id is used for registration, config and router binding. Missing or
mismatched registrations fail activation. Default bro does not register native code.
HTTP config and wire v1 remain unchanged; native code does not use HTTP credentials.

Projection, frozen bindings, concurrency admission, failure handling and receipts
remain host-owned for both modes. Each native callback is synchronous and runs in
the blocking pool; timeout/cancellation does not kill it and cannot free its slot
until it finishes. Native returns use the same reason/version validation as HTTP.
The 8 MiB encoded-envelope bound is HTTP-specific; both modes enforce the same
text/fragment/identity bounds. Native avoids wire serialization.

Inventory includes execution mode and native revision. Native endpoint fingerprint
is null; probes report network not_attempted and protocol not_checked, with a
synthetic decision or error. Receipt dispatch attempted means native work was
submitted, response_received means it returned. Existing HTTP meanings are
unchanged. Probe results remain separate from actual request evidence.
