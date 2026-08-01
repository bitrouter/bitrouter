# Task 1 implementation report — durable trajectory ledger

## Implementation

- Added migration 12 with the owner-scoped `trajectory_episodes`, immutable
  `trajectory_events`, request index, and outbox tables. It includes the
  episode-correlation, unique episode-sequence, request-owner-episode, and
  pending-outbox indexes.
- Added versioned Serde contracts, strict RFC3339/identifier/digest validation,
  canonical SHA-256 event content digests, and Eval-Exchange-equivalent
  credential-shaped categorical-attribute rejection.
- Added `TrajectoryStore` transaction methods: `begin_request`,
  `append_route_intent`, `settle_request`, `events_for_episode`, `request`,
  `pending_outbox`, and `mark_outbox_delivered`.
- The store scopes read/write queries by owner; enforces immutable event IDs
  and per-episode sequences; makes exact starts/settlements idempotent; and
  rolls back event/request/episode updates when an outbox insert fails.
- Correlation/full-input values are opaque bounded values. This task neither
  accepts raw message/prompt fields nor hashes their contents. HMAC and key
  lifecycle remain Task 2 responsibilities, per the tracked plan clarification.

## Files

- `apps/bitrouter/src/trajectory/mod.rs`
- `apps/bitrouter/src/trajectory/types.rs`
- `apps/bitrouter/src/trajectory/store.rs`
- `apps/bitrouter/src/db/migration/m20240101_000012_create_trajectory_ledger.rs`
- `apps/bitrouter/src/db/migration/mod.rs`
- `apps/bitrouter/src/lib.rs`
- `docs/superpowers/plans/2026-08-01-trajectory-progress-control-plane.md`

## TDD evidence

### RED — migration

Command:

```text
cargo test -p bitrouter --all-features db::migration::tests::trajectory_ledger_migration_creates_and_removes_only_its_objects -- --exact
```

Output (expected):

```text
error[E0432]: unresolved import `super::m20240101_000012_create_trajectory_ledger`
could not find `m20240101_000012_create_trajectory_ledger` in `super`
error: could not compile `bitrouter` (lib test)
```

### GREEN — migration

Same command output:

```text
running 1 test
test db::migration::tests::trajectory_ledger_migration_creates_and_removes_only_its_objects ... ok
test result: ok. 1 passed; 0 failed
```

### RED — wire validation

Command:

```text
cargo test -p bitrouter --all-features trajectory::types::tests -- --exact
```

Output (expected):

```text
error[E0425]: cannot find value `TRAJECTORY_SCHEMA_VERSION` in this scope
error[E0422]: cannot find struct, variant or union type `TrajectoryEvidence` in this scope
error[E0425]: cannot find function `validate_event` in this scope
error: could not compile `bitrouter` (lib test)
```

### GREEN — wire validation

Command:

```text
cargo test -p bitrouter --all-features trajectory::types::tests:: --lib
```

Output:

```text
running 3 tests
test trajectory::types::tests::canonical_event_digest_is_stable_and_excludes_its_own_digest ... ok
test trajectory::types::tests::evidence_rejects_credential_shaped_categorical_attributes ... ok
test trajectory::types::tests::event_validation_rejects_invalid_wire_values_and_tampering ... ok
test result: ok. 3 passed; 0 failed
```

### RED — durable store

Command:

```text
cargo test -p bitrouter --all-features trajectory::store::tests:: --lib
```

Output (expected):

```text
error[E0425]: cannot find type `TrajectoryStore` in this scope
error[E0425]: cannot find type `BeginRequest` in this scope
error[E0422]: cannot find struct, variant or union type `Settlement` in this scope
error: could not compile `bitrouter` (lib test)
```

### GREEN — focused ledger suite

Command:

```text
cargo fmt --all && cargo test -p bitrouter --all-features trajectory:: --lib && cargo test -p bitrouter --all-features db::migration::tests::trajectory_ledger_migration_creates_and_removes_only_its_objects -- --exact
```

Output:

```text
running 7 tests
test trajectory::types::tests::canonical_event_digest_is_stable_and_excludes_its_own_digest ... ok
test trajectory::types::tests::evidence_rejects_credential_shaped_categorical_attributes ... ok
test trajectory::types::tests::event_validation_rejects_invalid_wire_values_and_tampering ... ok
test trajectory::store::tests::store_is_owner_scoped_and_rejects_cross_owner_episode_parentage ... ok
test trajectory::store::tests::store_rejects_duplicate_sequences_and_mutable_event_replacements ... ok
test trajectory::store::tests::outbox_insert_failure_rolls_back_request_settlement_and_episode_head ... ok
test trajectory::store::tests::identical_starts_and_settlements_are_idempotent_and_outbox_is_owner_scoped ... ok
test result: ok. 7 passed; 0 failed

running 1 test
test db::migration::tests::trajectory_ledger_migration_creates_and_removes_only_its_objects ... ok
test result: ok. 1 passed; 0 failed
```

## Verification

```text
cargo fmt --all -- --check
```

Exit 0.

```text
cargo clippy -q -p bitrouter --all-features --all-targets -- -D warnings
```

Exit 0.

```text
cargo test -p bitrouter --all-features
```

The sandboxed run reached the ledger tests but 28 unrelated existing tests
failed because mock/listener port binding is denied by the sandbox. The same
command was rerun with local test-socket permission: 770 library tests, 20
binary tests, all integration tests, and doc tests passed; 9 documented
real-agent tests remained ignored.

A later parallel retry exposed one existing Fleet MCP shared-state race; its
single test passed in isolation. Final serial verification was green:

```text
cargo test -p bitrouter --all-features -- --test-threads=1
```

It passed the same 770 library tests, 20 binary tests, integration tests, and
doc tests, with the same 9 documented real-agent tests ignored.

## Self-review

- Checked the migration down path preserves an unrelated sentinel table.
- Checked owner filtering occurs in each public store lookup/update path.
- Checked mutations that change event digest, sequence, owner, or duplicate
  outbox identity are caught by the focused tests.
- Checked opaque correlation inputs are stored as provided and are never passed
  to the SHA-256 event digest function as raw prompt/message content.
- `git diff --check`, formatting, Clippy, focused tests, migration tests, and
  the relevant package suite are clean.

## Fix round 1 — review corrections

Addressed the Critical and Important review findings without taking on Task 2's
HMAC/key-lifecycle work. Opaque digest values are now format-validated
`hmac-sha256:<key-id>:<64-lowercase-hex>` values; this validates the wire
contract only and does not compute or rotate keys.

### RED — persistence-boundary contracts

Command:

```text
cargo test -p bitrouter --all-features trajectory::types::tests:: --lib
```

Output (expected before implementation):

```text
error[E0433]: failed to resolve: use of undeclared type `KeyedDigest`
error[E0422]: cannot find struct, variant or union type `OutboxPayload` in this scope
error[E0425]: cannot find function `validate_outbox_payload` in this scope
```

Implemented a typed `OutboxPayload` containing only bounded structural
numeric fields and SHA-256 digest fields. The former free-form JSON payload is
no longer persistable. Added sensitive-key rejection for structural and
evidence attributes, bounded evidence key lengths, and keyed-digest wire
validation for correlation and full-input values.

### GREEN — persistence-boundary contracts

Command:

```text
cargo test -p bitrouter --all-features trajectory::types::tests:: --lib
```

Output:

```text
running 5 tests
test result: ok. 5 passed; 0 failed
```

### RED — idempotency, ownership, and reconciliation

Command:

```text
cargo test -p bitrouter --all-features trajectory::store::tests:: --lib
```

Output (expected before implementation):

```text
duplicate_start_requires_identical_event_and_episode_inputs ... FAILED
duplicate_settlement_rejects_changed_outbox_or_started_status ... FAILED
event_history_rejects_corrupt_index_columns_and_sequence_gaps ... FAILED
native_parent_cannot_reference_another_owners_request ... FAILED
```

The separate Started-status test also failed before the change:

```text
cargo test -p bitrouter --all-features trajectory::store::tests::settlement_rejects_started_status --lib -- --exact
```

Implemented exact duplicate-start comparison (request, episode, and initial
event), canonical outbox comparison for duplicate settlement, owner-scoped
native-parent checks with global collision detection, owner-first event lookup,
stored-row/index reconciliation with contiguous sequence validation, and
rejection of settlement from `Started`.

### GREEN — focused review suite

```text
cargo fmt --all
cargo test -p bitrouter --all-features trajectory::types::tests:: --lib
cargo test -p bitrouter --all-features trajectory::store::tests:: --lib
cargo test -p bitrouter --all-features db::migration::tests::trajectory_ledger_migration_creates_and_removes_only_its_objects -- --exact
```

Output:

```text
types: 5 passed; 0 failed
store: 9 passed; 0 failed
migration: 1 passed; 0 failed
```

### Final verification

```text
cargo fmt --all -- --check
cargo clippy -q -p bitrouter --all-features --all-targets -- -D warnings
cargo test -p bitrouter --all-features -- --test-threads=1
```

All commands exited 0. The serial package suite passed 777 library tests, 20
binary tests, integration tests, and doc tests; the existing 9 documented
real-agent tests remained ignored.
