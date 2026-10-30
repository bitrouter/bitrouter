# Read-only verification of fresh installation recovery

`bro tenancy offline recover --plan FILE --report NEW_DIRECTORY` checks an explicitly retired SQLite archive and a stopped, newly provisioned installation. It does not start Service, migrate a database, issue credentials, import resources, reset counters, copy history or authorize dispatch. The private plan/report formats are not public tenancy DTOs or runtime authority.

This first profile verifies independently approved **new static provisioning**. Historical Eval conversion remains the separate `eval-inventory` / `eval-backfill` procedure. Source-equivalent definition transfer, OAuth generations, checkpoints, learned/Choice/paid routes and automatic learning are outside this verifier's profile. Source history stays in its exact archived namespace, including unknown effects. There is no cross-tenant deduplication or implicit adoption.

## Operator procedure

1. Maintain an external retirement/write-exclusion fence over the old process, restart sources, database and selected external files. Preserve unresolved upstream effects as unknown. Record the exact retired writer/selection inventory and producer evidence. A canonical advisory lease only excludes participating binaries; this command cannot prove that a pre-lock writer or external restart mechanism has stopped.
2. Produce and retain an anchored `offline snapshot` bundle. Keep its four-file layout, original identity, counter/latch values, keyed table digests and `complete=false` / `cutover_ready=false` unchanged. The source schema must match this verifier's fresh target schema exactly; a different historical schema is refused, not rewritten.
3. Under a separate maintained target-ingress fence, use normal owning Service startup with a new state directory and installation. Use the real tenancy API/SDK to create approved tenants, principals, bindings and fresh credentials. Re-provision only explicitly approved new static definitions through normal `tenancy.resource_import`, using the new installation and its explicitly configured wrapping key. Do not copy old ciphertext, key epochs, tokens or operation records.
4. Use the actual tenant credential to apply a configuration, read it back, activate it with the explicit CAS values and read the active pointer. Retain every actual operation GET result with its executing principal. Stop/drain Service and close its actual database owner. The target must be a private regular file in a private directory with no WAL, SHM or rollback-journal sidecar; this command does not checkpoint or repair a live database.
5. Write the private plan and anchored evidence below. Invoke `recover` with explicit absolute paths. No context, host-config discovery, network access, process management or environmental routing fallback is used. Only the named wrapping-key environment variable is read for original retained-resource authentication.
6. Preserve the output, archive and maintained fences. Target restart/readback, old-receipt audit and an operator's later ingress-release decision remain separate gates. Current dispatch/strict-receipt capabilities are unchanged. New target work invalidates this fresh-state report; automatic history merge or failback is not supported.

## Private input format

The plan is a private regular JSON file (no symlink leaf, no hard link, no group/world permissions), at most 2 MiB. Its `format` is `bitrouter.offline.recovery-plan.v1`; unknown fields are rejected. The CLI records its exact SHA256. All paths below are explicit absolute paths. An **anchor** is `{ "path": "/absolute/private/file", "sha256": "<64 lowercase hex>" }` and must match actual bytes.

| Field | Required contents |
| --- | --- |
| `source` | `bundle`, `evidence_digest`, original `installation_id`, `producer`, and `tables`. |
| `source.producer` | Original 40-hex `commit`, `artifact_sha256`, and anchored `evidence`. The evidence format is `bitrouter.offline.source-producer-attestation.v1` with the exact `installation_id`, `commit`, `artifact_sha256`, and `database_evidence_digest`. Unknown producer evidence must not be replaced with the verifier's identity. |
| `source.tables` | Exactly one entry per measured source table: `table`, `rows`, `value_bytes`, `digest`, and `disposition` (`retained_archive` or `external_quarantine`). Counts and keyed digests must match the verified bundle. There is no mapped/live disposition. |
| `target` | Existing `database`, new `installation_id`, anchored `provisioning`, `wrapping_key_env`, and anchored `static_manifest`. The latter is the exact approved normal resource-import manifest, at most 4 MiB, with the new installation. No source wrapping-key input or preserved-definition mode exists. |
| `retirement` | Four required true assertions: `fence_maintained`, `target_ingress_fenced`, `selection_exhaustive`, `unresolved_effects_remain_unknown`; anchored `evidence`; and 1–32 `writers`. Each writer records `id`, `process_identity`, `restart_source`, `retired: true`. |
| `selected_files` | At most 128 explicitly retired opaque files, each with `path`, `writer` (an inventoried writer ID), `bytes`, and `keyed_digest`. No ambient file search, token parsing, refresh or live-store transfer occurs. |

Retirement evidence uses `format: bitrouter.offline.retirement-attestation.v1`, `source_installation_id`, `database_evidence_digest`, the same four true fence/selection assertions, and exactly the plan's `writers` and `selected_files` arrays. The verifier checks these anchors and bindings. They remain operator attestations; matching JSON and acquiring a lease are not independent proof that the stated process ran or retired.

For a selected file, use the source bundle's `inventory.key` and the existing offline HMAC framing: HMAC-SHA256 over length-framed `bitrouter.offline.sqlite-evidence.v1`, then `recovery-selected-file.v1`, then the exact file bytes (each length is a big-endian u64). Each file is at most 16 MiB; all selected files together are at most 32 MiB. Inputs must be private regular non-hard-linked files and cannot overlap either database/archive. Files are reread after actual SQLite close before publication. The report records keyed digests and opaque private copies, never plaintext in CLI output.

The provisioning evidence uses `format: bitrouter.offline.fresh-provisioning.v1` and contains:

- `installation_id`: the actual new capabilities identity.
- `operations`: `{principal_id, result}` for every actual operation GET response. This first profile requires unique operation IDs and applied authority revisions covering exactly 1 through N, no failed/safety operations, and exact durable counter/history agreement. Issuance secrets must never be placed in this evidence.
- `configurations`: all actual `ConfigurationRevision` readbacks, at most 32.
- `active`: `{tenant, revision, authority_revision}` for each activation. The revision is confirmed by active GET; the authority revision is the corresponding activation result's revision, not a later global revision sampled after unrelated commands.
- `resources`: the exact qualified resource references in the approved manifest, at most 256, all reachable from the declared configurations.

The target's original authority decoder validates its actual current records.
Every declared operation also passes the same row decoder used by normal
operation lookup and idempotent command replay, including its read context,
creation timestamp and command digest; checking only the result JSON is
insufficient. These fields share the same cumulative target read budget. This narrow profile additionally requires active fresh credentials and current tenant-wide admin bindings for the configuration creators, with resource read/draft and policy-activate permissions and the actual intersection of credential/binding resource scope. It does not manufacture Caller, Grant, Scope or StartupOwner. All stored configurations and active pointers must be declared. The original resource decoder authenticates the key epoch, encrypted artifacts, exact approved definitions and retained compiled configuration under one cumulative operation budget. Only fixed static route/policy/API-key closures and learning `off` with zero evaluations are accepted.

## Bounds, custody and output

Source limits remain the offline profile: 1,000,000 rows, 256 MiB of measured typed/schema work, 512 MiB SQLite files, 16 MiB cells, 32 MiB rows and the original 120-second work deadline. Target payload sizes/counts are measured before transfer and capped at 4096 rows/32 MiB. Original authenticated-decoder copies/expansion also consume the existing 4096-row/32-MiB operation budget; these maxima are ceilings, not a guarantee that every combination at the ceilings fits. Schemas, JSON, evidence and selected-file counts remain finite.

The same `offline::Connections` owns direct read-only SQLx workers and the file-first canonical target lease. No SeaORM pool or second writer is created. One target read transaction is explicitly rolled back; no target COMMIT or DML exists. Cancellation stops admission/publication but retains in-flight establishment/read/rollback/close until actual completion. Unwind or unconfirmed worker close retains the original lease until process exit and cannot publish a successful report. The borrowed audit/resources inspection functions own no transaction, lease, Service or scheduler.

The new report directory is mode0700 with create-new mode0600 files. It contains the exact private plan, bound producer/retirement evidence, opaque selected-file copies, `target-inventory.json`, `report.json`, and finally `VERIFIED` (SHA256 of `report.json`) only after actual driver close and input/target rechecks. `INCOMPLETE` is removed last. Both a matching marker and absence of `INCOMPLETE` are required; output failure is never repaired by overwriting an existing report. A lost stdout response does not undo already published evidence: inspect the existing directory rather than blindly rerunning into it.

Successful output means the declared fresh provisioning and static closure passed this read-only profile. It always retains `complete=false`, `cutover_ready=false`, `external_writers_independently_excluded=false`, `history_transferred=false`, and `upstream_effects_resolved=false`. Source saturation/counters are reported as actually observed; a small fixture is not evidence of recovery from the production 100,000-operation boundary.

## Qualification status

Targeted local Unix tests have passed with the actual built CLI and normal
connector/Service/SDK producers. They cover fresh static verification, ordinary
restart/readback, operation context/time corruption, substituted definitions,
copied old credentials, held-read cancellation and original file ownership.
Selected-file regressions reject changed content, symlinks, hard links and public
permissions. Changing a selected file during an actual held SQL read prevents
publication after worker close; restoring its original bytes permits a new
control invocation without repairing the failed report.

The saturated-source campaign produced all 99,488 ordinary and 512 safety
operations through the HTTP SDK, observed explicit refusal beyond both ceilings,
closed the original Service, archived its actual database and ran the verifier.
The fresh target retained its own independent history. Its normal restart
rejected old credentials and operation IDs and accepted a new authorized
command. Source and archive evidence stayed unchanged, and all cutover/history
transfer assertions remained false. This local campaign took about 74 seconds;
it is not a throughput or production recovery-time benchmark.

All seven public parents have dedicated serial Unix CI entrypoints. The CLI
consumer requires an explicitly built binary and fresh fixture key; absence or
a missing test match is failure. These results do not qualify Windows, external
writer exclusion, every filesystem/mount/fsync failure, broader transfer profiles
or operator cutover. Final-artifact and platform qualification remain separate.
