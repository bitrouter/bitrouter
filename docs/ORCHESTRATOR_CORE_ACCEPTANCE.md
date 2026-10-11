# Orchestrator Core Acceptance Evidence

This audit maps every requirement in
[specification v1.0 §13](ORCHESTRATOR_CORE_SPEC.md#13-acceptance-matrix) to
source and executable evidence at `23008f8d5d4f810215c42eebf6b616f29692a6a8`.
The specification remains the contract. This document records demonstrated
behavior and finite exit conditions; it does not certify production readiness.
The [implementation record](ORCHESTRATOR_CORE_IMPLEMENTATION.md) retains the
stage history, commands, fault cases and measurements.

## Evidence matrix

Tests using scripted providers, injected failures or a fixture harness establish
their asserted core behavior. Process tests really terminate `bro serve` and
retain an on-disk journal, but still use a fixture harness/provider. Neither kind
establishes production harness conformance or measured remote activity.

| ID | Existing evidence | Remaining boundary |
| --- | --- | --- |
| A01 | [Transport collaboration][collaboration], `local_and_remote_collaboration_preserve_concurrency_context_and_attribution`, compares in-process and HTTP/WebSocket decisions, context and execution receipts. | The ordinary execution path is demonstrated. Remote restoration of Running tools is missing; see R1. |
| A02 | [Managed API tests][api-tests], `unmanaged_inference_stays_available_and_unknown_extensions_fail_before_execution`. | No additional core implementation gap identified for ordinary inference isolation. |
| A03 | [Managed API tests][api-tests], `remote_api_binds_auth_and_normalizes_channel_http_results`; [authentication tests][authentication] cover revocation, expiry, continuation and revocation during upload; [contract tests][contract] reject unsupported bindings. | Production clients must use the negotiated identity and authorization path. |
| A04 | [Transport collaboration][collaboration] uses provider barriers to observe two overlapping child requests and compare parent attribution; [core execution][execution], `children_overlap_with_bounded_slots_and_root_joins_their_results`. | Real overlap is already asserted, rather than inferred from two scheduled futures. Production integration remains R2/R5. |
| A05 | [Core execution][execution], `children_overlap_with_bounded_slots_and_root_joins_their_results` asserts three agents after two explicit spawns; `delegate_reuses_the_stable_idle_candidate_and_joins_its_model_receipts`, `ambiguous_delegate_uses_fresh_context_with_recorded_reuse_rejections` and `current_run_followups_reuse_old_agent_context_in_fifo_order` cover delegation and follow-up. | [Allocation][allocation] only offers reuse to delegate; [spawn][core-collaboration] creates a new identity for both fresh and inherited context. No new core gap identified. |
| A06 | [Core execution][execution], `current_run_followups_reuse_old_agent_context_in_fifo_order`, `model_collaboration_waits_release_slots_and_tools_keep_agent_attribution`, `assignment_cycles_include_idle_intermediate_ancestors` and `implicit_assignment_cycles_reject_before_acceptance`. | Tests assert send without starting a turn, single message consumption, waits with one model slot, and cycle rejection. |
| A07 | [SDK tests][sdk-tests], `managed_fixed_model_skips_policy_and_manual_effort_survives_policy` and `managed_fixed_model_rejects_hook_rewrite_before_dispatch`; [core execution][execution] asserts fixed child overrides and zero calls for all-infeasible candidates; [reconstruction][reconstruction] keeps fixed context unchanged when automatic rebuild would be necessary. | Fixed model/effort/context and feasibility have explicit assertions. [Input-count tests][input-count], `managed_protocol_filters_lossy_candidate_before_count_or_attempt`, also reject incompatible candidates before count dispatch. |
| A08 | [Core execution][execution], `changed_material_version_rejects_old_content_before_model_dispatch`, `reuse_activation_checks_requirements_pinned_after_the_original_decision` and collaboration pairing assertions; [reconstruction tests][reconstruction]. | No newly identified core gap. Additional reconstruction permutations require a concrete missing invariant or failure to justify them. |
| A09 | [Protocol matrix][protocol-matrix], `common_native_constraints_and_accounting_match_four_by_four_http_protocols`; [native/HTTP tests][native-http] compare constraints, fallback and accounting. | Native SDK execution is covered; this does not establish legacy `ThreadService` delegation to `CoreSession` (R2). |
| A10 | [Core execution][execution], `abandoned_driver_cannot_start_a_duplicate_model_step` and collaboration ownership assertions; [process recovery][process] redelivers an unstarted command with its original identity. | Production tool execution needs the harness ledger and local admission barrier (R3). |
| A11 | [Contract tests][contract], `no_dispatch_before_matching_atomic_ack`; [core execution][execution] tests failed attempt/output/terminal commits, partial output and incorrect ACKs. | Core barriers have evidence; the production store must actually satisfy atomic commit/ACK semantics (R3). |
| A12 | [Contract tests][contract] and [response tests][responses] cover exact ACK reconciliation and atomic continuation; [remote controls][controls], `queue_steer_cancel_reconnect_preserves_receipts_and_paused_work`, covers 24 ACK-loss boundaries. | Further arbitrary fault permutations are not a separate deliverable. Production persistence conformance remains R3. |
| A13 | [Process recovery][process], `process_crash_reconciles_model_tool_and_terminal_boundaries`, plus unstarted-command and unknown-write cases; [incomplete provider bodies][inflight] exercise two process-death boundaries. | Actual process termination and file effects are covered. Activity handoff is scripted, so production measurement remains R1/R5. |
| A14 | [Recovery tests][recovery], `recovery_replays_committed_output_and_never_confirmed_effects` and `recovery_unknown_writes_and_shells_block_until_harness_supplies_result`; [contract tests][contract] and [start fences][fences]. | Core fencing and unknown-effect blocking have evidence. Production local start serialization and effect reconciliation remain R3. |
| A15 | [Incomplete provider bodies][inflight]; [recovery tests][recovery], `recovery_closes_interrupted_work_with_new_attempt_and_preserved_unknown_spend`; [reconnect tests][reconnect]. | Partial output and unknown spend are covered. Actual provider reconciliation remains R5. |
| A16 | [Root queue][queue], [steering][steering], [start fences][fences] and [remote controls][controls] cover identities, pause/resume, cancellation, ACK loss and restore. | Production approval/start atomicity remains R3; same-owner reconnect is not Running-tool process takeover. |
| A17 | [Pressure tests][pressure], `held_http_output_does_not_block_head_queries_or_cancellation`; [managed API tests][api-tests], `abandoned_sse_consumer_keeps_execution_and_replays_attributed_frames`; core disconnect barriers. | Held response data and separate control progress have fixture evidence. Host resource measurements under declared load remain R5. |
| A18 | [Response tests][responses], `exchange_terminal_ack_blocks_tool_delivery_and_visible_completion` and `exchange_verification_waits_for_completion_and_a_new_exchange`; [managed API tests][api-tests] verify client-tool verification. | Exchange/run separation, child joins and terminal ACK barriers have evidence. |
| A19 | [Core execution][execution] asserts concurrent tool attribution, permission changes and verification admission; [start fences][fences] models local execution races. | Actual workspace read/write/shell barriers and denied approval without effects need the production harness (R3). |
| A20 | [Capacity tests][capacity], [tree limits][tree-limits], [expanded prompts][expanded], [artifact storage][artifacts], [budgets][budgets] and [pressure tests][pressure] cover logical admission and retained cleanup. | Physical storage, retained history, legacy cleanup and measured host memory require R4/R5. Logical quotas alone do not complete those obligations. |
| A21 | [Accounting][accounting], [provider work][provider-work], [preparation work][preparation], [material work][materials] and [native costs][costs] retain child/retry/preparation attribution, late evidence and unknown spend. | Real provider reconciliation remains R5. Missing measurements must remain unknown. |
| A22 | [Capabilities][api] explicitly declare unsupported multi-agent items, encrypted state, injection and stateless replay; [managed API tests][api-tests] reject unsupported extensions. | No full provider beta or encrypted-item compatibility is claimed or required by this bounded surface. |
| A23 | Existing independent HTTP/WebSocket clients and process fixtures exercise the protocol, but no production-harness run is recorded. | **Not demonstrated.** A production harness and independent client must complete, disconnect, restore and continue a real task (R2–R5). |

## Remaining work and exit evidence

### R1. Measured remote Running-tool restoration

The [remote bind handler][channel] rejects restoration containing
`ToolStatus::Running`; [capabilities][api] declare `running_restore_handoff`
unsupported. This is an implementation gap, not just a missing test.

The in-process [HarnessPort][session] already has `observe_restoration` and
`synchronize_restoration`. The remote bridge must provide a trusted measurement
domain, active-work coverage through replacement-core entry, and stop-event
delivery/draining through restore validation and ACK waits. The current
`Restore.active_time` run/head/cumulative-milliseconds fields do not by
themselves supply that bridge.

Exit evidence: document the actual measurement authority and clock mapping;
implement the negotiated remote bridge; then exercise a Running tool that
stops during restoration and a delayed stop report crossing its final drain.
Assert the activity union, budget outcome, exact restored head and no loss or
double counting at transition to live reporting. Unknown coverage, a foreign
measurement domain or an inconsistent lifecycle boundary must fail closed.
Arrival time, RTT, summed attempt durations and whole downtime are not
substitutes for the measurement required by spec §6.3.

### R2. One scheduler at the production harness entry point

The [service host][host] constructs both `ManagedCoreApi` and legacy
`ThreadService`; [legacy execution][legacy] still runs `Agent::run_context`.
Merging the native base therefore does not establish its managed-core adapter.
This source fact does not show both schedulers owning the same managed session,
and the specification does not require converting all transparent ACP clients.

Exit evidence: identify the production harness repository, revision and launch
entry point; wire its managed sessions to `CoreSession` or the managed API;
reuse its tools/store behind the harness interface; and show that inputs,
delegation, queueing, steering and recovery have one core scheduling owner.
Any retained legacy mode must remain separate from those managed sessions.
This satisfies spec §11 without inventing a universal legacy-CLI migration.

### R3. Production execution and durability conformance

Exit evidence from the chosen harness:

- Atomic checkpoint append and matching ACK, including lost ACK after append,
  reconnect, process restart and exact operation/result replay.
- Local start admission serialized with committed fences and approval
  revocation; duplicate delivery and stale epochs never restart a known effect.
- Real concurrent reads, exclusive writes/shell effects and resource release;
  denied approval executes no effect, with observable workspace/ledger evidence.
- Confirmed effects survive process replacement without rerun; an unknown
  write/shell effect blocks dependent work until authenticated reconciliation.

Core fixtures provide the expected protocol behavior, not proof that a
production store, permission system or workspace executor implements it.

### R4. Production storage and retained-history guarantees

Exit evidence: document and exercise the harness's concrete capacity admission,
artifact staging, checkpoint retention/reclamation and recovery migration
policy. The test must preserve acknowledged dependency closures while a new
root is staged, survive storage refusal before/after append, and retain room
for accepted invocations' required evidence and terminal cleanup. Include
supported legacy invocations without prospective reservations, or demonstrate
their safe rejection before ownership transfer as allowed by spec §10.

A physical lease API is one possible implementation, not a required API name.
Logical current-state reservations alone do not establish this exit. Conversely,
the spec does not require unlimited history or unlimited recovery growth:
additional optional growth may be refused without losing accepted state.

### R5. Integrated task, providers and host pressure

Exit evidence: run the selected production harness and an independent client
through A23 using a real provider, recording configuration/revisions without
credentials. Include completion, durable disconnect, process restoration and
continued work, with parent/child attribution and provider usage/cost evidence.
Record uncertain and later reconciled spend separately.

Measure the configured host/session limits under concurrent model work,
bounded output/control pressure and a slow UI consumer. Record peak resident
memory, retained transport/checkpoint/artifact bytes, refusal behavior and
cancellation/control progress. State the supported executor/hook configuration
and its resource contract; core buffer limits do not bound arbitrary allocations
inside external callbacks. Test concrete missing interactions or failures,
rather than requiring all possible pressure permutations.

After R1–R5, rerun required repository checks, audit all A01–A23 against the
integrated evidence, obtain an independent whole-change review and verify CI
on the final submitted head. These remain completion gates; this audit and
the earlier stage reviews do not replace them.

## Validation baseline

At the audited source revision, local Rust 1.99.0 workspace/all-feature nextest
reported **4183 passed, 22 skipped**, with four test threads. Strict clippy,
rustdoc, formatting and Rust 1.93.0 checks passed; doctests reported five passed
and one ignored. Exact commands and timings are in the implementation record.
The acceptance-ledger change is documentation-only and does not rerun or expand
that executable evidence. Current CI status must be read from the matching
revision's run, not inferred from a parent commit or these local results.

[execution]: ../crates/bitrouter-orchestrator/tests/core_execution.rs
[contract]: ../crates/bitrouter-orchestrator/tests/core_contract.rs
[api-tests]: ../apps/bitrouter/tests/orchestrator_core/managed_api.rs
[authentication]: ../apps/bitrouter/tests/orchestrator_core/managed_api/authentication.rs
[collaboration]: ../apps/bitrouter/tests/orchestrator_core/managed_api/collaboration.rs
[native-http]: ../apps/bitrouter/tests/orchestrator_core/native_http.rs
[protocol-matrix]: ../apps/bitrouter/tests/orchestrator_core/native_http/protocol_matrix.rs
[reconstruction]: ../crates/bitrouter-orchestrator/tests/core_execution/reconstruction.rs
[responses]: ../crates/bitrouter-orchestrator/tests/core_execution/responses.rs
[controls]: ../apps/bitrouter/tests/orchestrator_core/managed_api/recovery/control.rs
[process]: ../apps/bitrouter/tests/orchestrator_core/managed_api/recovery/process.rs
[inflight]: ../apps/bitrouter/tests/orchestrator_core/managed_api/recovery/process/inflight.rs
[recovery]: ../crates/bitrouter-orchestrator/tests/core_execution/recovery.rs
[reconnect]: ../crates/bitrouter-orchestrator/tests/core_execution/reconnect.rs
[fences]: ../crates/bitrouter-orchestrator/tests/core_execution/cancellation/start_fences.rs
[queue]: ../crates/bitrouter-orchestrator/tests/core_execution/root_queue.rs
[steering]: ../crates/bitrouter-orchestrator/tests/core_execution/steering.rs
[pressure]: ../apps/bitrouter/tests/orchestrator_core/managed_api/pressure.rs
[capacity]: ../crates/bitrouter-orchestrator/tests/core_execution/capacity.rs
[tree-limits]: ../crates/bitrouter-orchestrator/tests/core_execution/capacity/tree_limits.rs
[expanded]: ../crates/bitrouter-orchestrator/tests/core_execution/capacity/later_prompt/expanded.rs
[artifacts]: ../crates/bitrouter-orchestrator/tests/core_execution/artifact_storage.rs
[budgets]: ../crates/bitrouter-orchestrator/tests/core_execution/budget.rs
[accounting]: ../crates/bitrouter-orchestrator/tests/core_execution/accounting.rs
[provider-work]: ../crates/bitrouter-orchestrator/tests/core_execution/provider_work.rs
[preparation]: ../crates/bitrouter-orchestrator/tests/core_execution/preparation_work.rs
[materials]: ../crates/bitrouter-orchestrator/tests/core_execution/material_work.rs
[costs]: ../apps/bitrouter/tests/orchestrator_core/native_http/costs.rs
[api]: ../apps/bitrouter/src/orchestrator_api/mod.rs
[channel]: ../apps/bitrouter/src/orchestrator_api/channel.rs
[session]: ../crates/bitrouter-orchestrator/src/core/session.rs
[host]: ../apps/bitrouter/src/host.rs
[legacy]: ../crates/bitrouter-orchestrator/src/service.rs
[allocation]: ../crates/bitrouter-orchestrator/src/core/allocation.rs
[core-collaboration]: ../crates/bitrouter-orchestrator/src/core/collaboration.rs
[sdk-tests]: ../crates/bitrouter-sdk/src/language_model/tests.rs
[input-count]: ../crates/bitrouter-orchestrator/tests/core_execution/input_count.rs
