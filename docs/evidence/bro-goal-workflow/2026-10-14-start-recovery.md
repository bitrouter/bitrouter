# Original ACP start recovery, 2026-10-14

The real Codex Goal attempt exposed a write window: ACP had started but its Goal Running observation failed to commit under SQLite WAL contention. The Goal retained Unknown/null, correctly avoided replay, and could not stop the otherwise admitted process through its own mapping.

## Database contention

A one-pool control passed and did not reproduce the lock. Sixteen independent connection pools sharing an actual WAL database reproduced Storage/SQLite 5 database locked. Taking SQLite's writer before opening the transaction's read snapshot removes the deferred read-to-write upgrade conflict. The original head CAS, immutable receipt identity, duplicate adoption and rollback remain.

[Control](2026-10-14-goal-wal-one-pool-control.log), [reproduction](2026-10-14-goal-wal-multiple-pools-red.log), [initial green](2026-10-14-goal-wal-original-start-green.log). The latter run `79d15c29-579e-49e9-a27d-4688d45cd2fc` passes the contention case and the readonly durable-start lookup; it does not prove complete runtime recovery.

## Original start identity

Before external startup, the private supervisor ledger records the original action ID and a SHA-256 request fingerprint, preserving them across subsequent snapshot writes. It does not store the prompt or resolved credentials. Peek authorization is required for the internal original-ID query. The query neither dispatches Start nor renews execution intent. Reopening the ledger preserves the mapping and records previously live processes as Interrupted.

The Goal coordinator verifies harness identity and canonical workspace before saving the recovered original run. Running is distinct from an interrupted/stopped process with unknown effects. Paused work remains paused. Old ledger rows without the start identity remain unmapped; directory or label coincidence is not a substitute for an original receipt.

## State transition correction and actual protocol regression

The first [runtime attempt](2026-10-14-start-recovery-v1-failed-nextest.log) fails. Additional boundary diagnostics in the [second attempt](2026-10-14-start-recovery-v2-failed-nextest.log) locate the error at recovered-map persistence: the workflow allowed no observation after Unknown/null, even when the original start lookup succeeded.

The corrected transition permits only Running or Unknown/Some to supply a missing run for an already admitted dispatch. It cannot install a Candidate result, replay Requested, repeat an unassociated Unknown or replace a known run. Owner, version and dispatch guards remain. A separate SQLite regression covers forbidden transitions and an unadmitted dispatch.

[Current three-case regression](2026-10-14-start-recovery-v3-nextest.log), run `160884db-ed2b-4f24-ad88-2d81e2f19529`: 3 passed in 121.132 seconds. The runtime case starts two actual Cargo-built ACP peers, independently covers Requested and Unknown/null, intentionally omits the Goal Running commit, pauses the Goal, recovers each original native/run identity and stops that process. Captured ACP traffic has exactly two session/new requests and no recovery prompt. Execution count stays one per Goal, pause stays saved, effects remain unreconciled and completion remains false. Fixture compilation is included in the runtime case's elapsed time; no commercial harness or provider participates in this regression.

The HTTP negative case still rejects content after a legitimate completed+[DONE] sequence and does not bind a continuation. The durable-start unit verifies cold lookup, scope refusal and absence of prompt material in the ledger.

[128-file current manifest](2026-10-14-start-recovery-v3-source.sha256). Complete Goal database regressions, new binary, fresh full workspace/static checks and commercial harness rechecks are pending. The original commercial Goal and its failure evidence remain preserved and paused, with a legacy unmapped execution. This slice does not complete the BRO MVP.

The [complete Goal database suite](2026-10-14-start-recovery-v3-goals.log), run `10869d44-5a2e-4e0f-9193-b30996a25f34`, passes all 79 tests in 2.272 seconds. This includes independent-pool WAL writes, unknown-map restoration, rejection without an admitted dispatch, Candidate/replay refusal, immutable known mappings and existing completion, permission, artifact and accounting guards. Fresh full workspace/static checks and the commercial workflow remain required.

A subsequent readonly query of the original commercial Goal's actual SQLite head confirms revision 1450, requirements 1, Pause, elapsed-budget halt, numbers Unknown, text Candidate and integration unstarted. The older goal-current-snapshot.json file is stale and is not used as current-state authority.

The rebuilt BRO binary passes its build, and strict workspace/all-target/all-feature Clippy, formatting and diff checks pass. A minimal readonly HTTP probe using the original commercial session's captured reasoning item confirms HTTP 400 with [the content-free diagnostic](2026-10-14-original-reasoning-probe-diagnostic.log): RequestProjection / Responses / NativeReasoningAuthorityUnproven / ReplayAuthority / RejectReplay. [Public error](2026-10-14-original-reasoning-probe-response.json). The original opaque item stays outside the repository. The stateless client lacks the managed origin proof; dropping the item or disabling that admission check would not preserve the context contract.

Actual OpenCode native ACP is a separate compatible Chat Completions experiment, not a claim that the Codex path passes. Its existing commercial failure, legacy mapping and paused elapsed-budget halt remain preserved. The complete source128 workspace run is still pending.

## Terminal complete source128 checks

[Complete nextest](2026-10-14-start-recovery-v3-workspace-nextest.log), run `38a53689-e022-4458-afd3-a64dff03a244`: 4621 passed (11 slow), 24 existing skipped, 941.502 seconds, exit 0. [Doctests](2026-10-14-start-recovery-v3-doctests.log): six passed, one existing ignored. [Strict Clippy](2026-10-14-start-recovery-v3-clippy.log), [format](2026-10-14-start-recovery-v3-fmt.log), [diff](2026-10-14-start-recovery-v3-diff.log), [source hashes](2026-10-14-start-recovery-v3-hashes.log) and [distribution consistency](2026-10-14-start-recovery-v3-dist.log) pass. There were no concurrent owned Cargo checks during this full test run. The earlier failures and original bounds remain preserved.

This is the current local source checkpoint. It is not hosted CI at PR #981, whose published head remains a316, or completion of the unfinished real commercial workflow. New permission/context/recovery changes require new evidence.
