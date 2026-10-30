# Bounded Legacy Eval backfill

This is the source contract for one offline Core-enabled Unix SQLite consumer.
It does not complete M4 retirement, general historical backfill, saturation
recovery, cutover or performance qualification. Runtime qualification is separate.
PostgreSQL/MySQL may create the portable additive provenance table; this import
consumer supports only a stopped file SQLite source bundle and destination.

## Explicit inputs and trust

First create independently retained `offline snapshot` evidence for the stopped
source and freshly provisioned target. The source must already have the additive
Legacy ownership columns; this consumer never runs migrations on either input.
The target is created using the normal owned connector/Service and authenticated
control API/SDK tenant, principal, binding and credential commands. This first
profile requires no configuration/resource/work history. Mapping destinations
must have current tenant-wide tenant-admin bindings; no Scope, Grant or startup
identity is fabricated. Destination names alone are insufficient.

An operator maintains the old-writer fence and retains external evidence anchors.
The canonical lease excludes participating binaries only. The offline command
cannot authenticate an absent historical human or prove that arbitrary external
writers have stopped. An explicitly selected source namespace and owner mapping
are operator decisions. Their exact evidence bytes, database evidence digest and
plan digest are checked and retained; this is not a new authorization endpoint.
Source commit text in the frozen M0 mapping remains an operator claim, not proof
of a historical producer binary. No remote or host-global process action occurs.

The private plan has `format: "bitrouter.offline.eval-backfill.v1"` and these
required fields (no unknown fields):

- `bundle`, `database_evidence_digest`: source evidence path and external anchor.
- `source_kind`: `local_user`, `shared_user` or `native_owner` for named Legacy owners.
- `ownership_evidence`, `ownership_evidence_digest`: a private bounded evidence file and its externally retained SHA256.
- `manifest`, `manifest_digest`: frozen M0 MigrationManifest bytes and external SHA256.
- `target`, `target_bundle`, `target_evidence_digest`: stopped destination file and independently anchored provisioning snapshot.
- `target_installation`: observed real Service installation, different from the source installation.
- `provisioning_operations`: exact unique IDs of all successful fresh provisioning commands, at most4096. No unlisted commands, safety operations, saturation or runtime generation are accepted.

All five input paths are absolute. Plan/evidence files are at most2MiB and must
be private regular single-link files. The M0 manifest retains its existing8MiB
limit. Outputs use a new private directory outside both immutable evidence
bundles. Secrets are never printed. No existing destination row is overwritten,
upserted, deleted, deduplicated or adopted. The schema ledger and all authority
rows/counters must be byte-equivalent under the typed inventory after import.
Only the Eval admission autoincrement watermark advances to the preserved source
sequence maximum; its original target value must be zero/absent.

## Graph, bounds and quarantine

The graph is exactly `eval_subjects`, `eval_results`, `eval_admission_events` and
`eval_snapshots`. Every source row must be Legacy (all three ownership columns
NULL); mixed or already-owned Eval rows refuse the entire conversion. Parent
keys, subject/result digests, idempotency, timestamps, snapshot entries/root and
owner edges use the real codecs. Counts and keyed typed-cell multiset digests
partition every row; mappings must exhaust exactly those partitions. A manifest
with unrelated family mappings is refused by this narrow converter. Examples,
when supplied, must identify exactly one subject/result/snapshot and its keyed
exact-cell digest from this inventory, not an unbound display name.

Each mapped result must have admission history before any destination DML. A
Legacy result insertion can commit before admission is appended; this is a valid
source inventory state, but it cannot be imported into the normal owned reader.
Mapping such an owner partition refuses the import without writing the target.
The operator may instead explicitly quarantine the whole incomplete partition
and import other complete partitions. No admission event is invented and no
source result is dropped or deleted; quarantined counts and digests retain it.

Complete source/target inventories and the transformed target fit4096 total
rows/32MiB typed values/1MiB cells. Schema expressions are qualified and all cell
sizes preflighted in SQL before payload transfer. New ownership/provenance bytes
are included in target sizing. Each imported snapshot must also fit the existing
normal owned-reader row and byte limits, including repeated parent loads and
response serialization; a small stored graph can still fail this stricter read
budget. These limits are not increased for import. M0 merge approvals never excuse a physical-key or
idempotency collision. Every unsupported source table is enumerated with its
original typed digest/count in the external quarantine report. Wildcard snapshots
remain unresolved; no owned worker consumes them and no NULL-owned leftovers are
copied into the destination. The complete original source bundle is unchanged.

## Immutable private provenance (migration038)

`eval_import_provenance` is new, with byte-exact installation/tenant/creator,
physical snapshot key, exact manifest SHA256, provenance digest and versioned
private JSON. It binds the full original installation, explicit source namespace
and owner, original root, mapping/database/ownership evidence anchors, exact
partition digest and target physical identity. The same transaction inserts the
four-table closure and provenance. No public DTO, Scope constructor or user
metadata-to-authority conversion is added.037 is unchanged. A pre-existing038
object is refused rather than adopted; destructive downgrade is refused and
requires a separately verified backup procedure.

Normal tenant lookup still requires the actual current Scope. Only its matching
stored provenance permits validation of an original Legacy root and exact stored
manifest bytes. Missing, partial, foreign or altered provenance fails closed.
Normal tenant roots retain their existing algorithm; Legacy readers stay NULL
scoped. External manifest consumers must match the same stored imported manifest
and full immutable subject/result closure. Imported data does not confer an
evaluator grant or execution permission.

## Custody and incomplete outcomes

One retained offline task acquires the target file lease before database access.
It owns every direct SQLx await. The caller can abandon its reply, not abort the
worker. Validation/readback/DML share the original finite work budget. Known DML
failure or cancellation rolls back explicitly; consuming BEGIN/COMMIT/ROLLBACK
failure or unwind remains unconfirmed and retains the file owner until process
exit, even if close later succeeds. No retry or count observation changes that
classification. A completed commit may still leave an INCOMPLETE packet if
cancellation, readback or report I/O fails. Report publication follows actual
worker close while the same file owner is still retained.

Source tests cover real Service/SDK provisioning, stopped import, restart and
owner-bound readers; exact original JSON spelling and cross-tenant negatives;
partition/collision/orphan/metadata/provenance guards; caller cancellation under
an actual open transaction; and an isolated real deferred-FK COMMIT failure with
pre-COMMIT full rows and file-owner retention. These are test sources until run.
No schema/consumer/artifact/backend/feature/independent-review result is implied.
