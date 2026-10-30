# Owned learner executable profile

This profile is implemented in the service and composes learned Checkpoint format 3 with native experiments and the Choice/paid/native ownership foundations. Tenancy and learning remain opt-in. Source-pinned runtime evidence and remaining aggregate, platform and performance qualification are tracked in [the implementation ledger](RESOURCE_TENANCY_IMPLEMENTATION.md#remaining-acceptance).

## Consumer and shipping entry

`host.rs` calls the existing `EvolutionScheduler::start` and joins its single returned worker before Service shutdown. Tenancy selects the owned worker; otherwise the existing Legacy worker is selected. An atomic Service guard prevents duplicate owned workers. The optional `tenancy.learning_registrations` file is loaded only after the ordinary H/crypto bootstrap and before the HTTP continuation reconciler/listeners. Failure awaits shutdown of that actual Service. No manifest on restart leaves durable schedules available to the same worker.

Manifest format 1 contains at most 32 `{credential_env, registration}` entries and is bounded at 1 MiB. Registration fields are documented in `skills/bitrouter/references/managed-core.md`. Tokens are authenticated by Service and dropped before H admission. No new RPC, registry, database writer, executor, or commercial policy is introduced.

## Authority and ownership

H stores the original credential ID/revision, stable principal and binding, ResourceRead/ResourceDraft permissions, scopes, and expiry ceilings. Every wake and publication intersects these with current authority, active configuration, and exact retained dependencies. Whole-tenant rights cover newly created resource keys; operator-pool input dependencies still need both original and current coverage. Successor policy and affected routes must belong to the registered tenant; read coverage never mints pool revisions. Rotation never retargets a schedule. Explicit registration updates use authority, active-revision, and schedule CAS. Ordinary operation retention and installation saturation account for registration and publication.

Background acquisition has its own checked read identity, not a closed session capability. It holds the existing native host semaphore through calculation/publication. Unknown native cleanup retains its original slot. Production claimed Services require the original native Released metadata stamp, checked against the same StartupOwner/Proof and current acquisition; unclaimed prepared fixtures are an internal test path only. The complete stored journal must still decode with its acknowledged checkpoint, artifacts, session/manifest, terminal run, source input, reconstruction chain and ACK facts. Unit, step, counter, original receipt and trajectory/Eval publication edges are independently checked. Metadata and complete journal contents participate in the source fence.

The original queued work and lease cell remain outside H's caught continuation. Ambiguous consuming COMMIT retains that work, lease and stopped-source fence in the existing quarantine owner. A dropped response waiter does not free an unknown owner. Numerical CPU work retains its acquired source and learning lease until its actual blocking task exits; after an unconfirmed cleanup deadline the Service fences rather than claiming success.

## Bounds and calculation

The shared existing admission counts remain at most 2 learning leases per tenant and 8 per instance. Discovery returns at most 32 registrations per pass. A selected job has at most 1024 source units, 4096 cumulative storage rows and 8 MiB cumulative charged work; publication carries acquisition charges forward. Source/config/resource payload lengths are preflighted before loading their bodies. The independent manual experiment cohort retains its 4096-row/32-MiB/5-second profile.

Queue/storage deadlines are 5 seconds. Computation has 30 seconds, cleanup has 30 seconds, and the original overall deadline is retained across continuation. Expired/cancelled work cannot publish. H releases SQL before CPU calculation and reacquires every source fence under a fresh borrowed transaction before publication.

The native history optimizer and cohort algorithms, ordering, versions and gates are unchanged. Options come from the retained active experiment. Operational native entries remain Inconclusive. A real challenger cap can yield the existing conservative Retreat; no fabricated assessment or positive promotion is inserted.

## Immutable publication and activation

One H transaction revalidates current authority and the original source fence, creates Checkpoint → successor Policy → every transitively affected Route → ConfigurationRevision, advances the schedule's completed fence/sequence, records the ordinary operation, and commits the authority counter. Each new immutable artifact is retained through the existing authenticated resource store and compiler. The configuration is prepared by the ordinary create-configuration function as a borrowed participant. All affected direct/hidden route references are substituted; old revisions remain intact. Format 3 is selected only by the learned Checkpoint; old formats remain unchanged otherwise.

No worker activation exists. A later explicit activation or rollback uses the existing current PolicyActivate, ResourceRead, dependency/pool coverage, authority revision and active revision CAS. R1 ownership remains pinned by existing execution/session logic. Schedules do not silently follow an activated R2.

## Actual-consumer test sources and remaining dependencies

New tests use a real file database and connector-produced OwnedDatabase claims, actual NativeSession runs with real loopback HTTP provider requests, exact receipt/ACK/Eval producers and bounded cleanup. They cover restart after native close and no registration file, the exact host single-mode scheduler launch function, actual owned loop, duplicate-launch rejection and no Legacy fallback, a real operational Retreat, both routes sharing a changed policy, no implicit activation, explicit activation/rollback, no provider replay, credential rotation between acquisition/publication and later wake, deletion of the actual source journal after calculation, and bounded discovery of 33 durable registrations as pages of 32 and 1.

Targeted local runs cover these consumers and the current native Human assessment contribution. Existing native ACK/recovery, format-3/legacy compatibility, feature, startup-manifest, transaction-custody and CPU-cleanup checks remain part of final candidate qualification. Earlier source-pinned results do not establish a final aggregate pass; schema generation, platform checks and performance evidence are recorded separately in the implementation ledger.

Native Human/paid producer attribution is specified in [the native experiment profile](RESOURCE_TENANCY_NATIVE_EXPERIMENT_PROFILE.md); ACP Block enrollment and effective-head reconciliation are specified in [the capture profile](RESOURCE_TENANCY_CAPTURE_PROFILE.md#explicit-retained-acp-block-profile). Matching labels, copied roots, or successful operational records do not supply attributed quality evidence. A successful operational Retreat does not prove conclusive promotion. Removed operational source rows deny fresh publication; they do not become zero-valued contributions. Existing frozen roots stay immutable audit data.
