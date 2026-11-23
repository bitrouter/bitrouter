# BitRouter configuration formats

Status: **accepted, `configuration/1.0.0-beta.5`**.
The [schema generator constant](../../../apps/bitrouter/src/config/schema.rs)
and schema annotation `x-bitrouter-spec-version` identify this contract.
The schema URL identifies its software distribution.
This is the format contract for
`bitrouter.yaml`, its shared-process profile and Policy source loading.
The [Policy contract](../policy/README.md) governs portable routing policy files.
MUST, MUST NOT, SHOULD and MAY express requirements on readers and writers.

## Compatibility and validation

Changes to accepted keys, scalar types, defaults, units, references, nullability,
merge/precedence rules or enum values require compatibility review. Review both
an old reader given a new document and a new reader given an old document.
An additive key can break a strict reader. A syntactically accepted configuration
can still be invalid for the selected feature set, authority or deployment.
Unknown required semantics MUST fail visibly instead of selecting a different
mode or weakening a security constraint. Beta permits reviewed incompatible
changes; it does not permit an undocumented change in interpretation.

The [generated application schema](../../../dist/schema/bitrouter.config.schema.json)
is the field/type reference for ordinary application YAML. The configuration
parser performs additional reference, derivation and semantic validation.
The CLI config validator uses that parser. Runtime acceptance also requires
semantic validation. The schema, parser, rejection behavior, examples and this
contract must be reviewed together. Do not hand-maintain another copy of the
complete field catalog in prose.

## Application YAML

`bitrouter.yaml` is a YAML mapping with no top-level format-version field.
The root rejects unknown fields. Each nested object follows its schema and
parser rules, including defaults and nullability. The generated schema is the
catalog of root groups and field units. Environment substitution in the local
profile precedes parsing and semantic validation. See the [CLI reference](../../CLI.md#config-resolution)
for source selection, substitutions and validation.

Named routers use `bitrouter/<id>`. Selection, defaults and ordered request
checks are separate. Policy bindings select literal names by exact match.

The closed `policy` mapping accepts an optional `path` relative to the
configuration file. When omitted, the conventional filename is `policy.yaml`
beside `bitrouter.yaml`. An explicit missing, invalid or unsupported source
fails. Tenant initialization explicitly submits its own source.

Policy publication requires an explicit expected head. The selected running
instance's active view is the default for inspection. The disk view must be
selected explicitly. Filesystem permissions govern local writes. Service
authentication, scope and compare-and-swap govern service writes.

A structural minimal local example is:

```yaml
server:
  listen: 127.0.0.1:4356
registry:
  enabled: false
```

This example only illustrates fields; it does not supply provider credentials
or guarantee a runnable inference route.

## Tenancy and routing

The `TenancyConfig` and `ProviderModel` definitions in the
[generated schema](../../../dist/schema/bitrouter.config.schema.json)
define accepted fields. The [tenancy validator](../../../apps/bitrouter/src/config/tenancy.rs)
also checks deployment, bounds and references.

### Opt-in and startup ownership

Omitting `tenancy` or supplying an empty mapping preserves the disabled default.
Tenancy requires explicit enablement, authenticated inference, an enabled control
listener and an explicit `control:tenancy` operator credential. The supported
profile is one Unix process exclusively owning a file-backed SQLite database
with `gateway_state.mode: local`.
These settings do not enable distributed tenant authority or distributed quotas.

The entire `tenancy` object is startup-only. A reload with changed effective
tenancy settings reports `restart_required` and MUST NOT publish those changes
into the running service. Editing a manifest or environment secret is not a
hot-reload protocol. Restart retains durable resources, revocations and execution
records; it does not erase them merely because a startup option is omitted.
The [tenant service guide](../../development/tenant-service.md) describes
assembly, lifetime and storage ownership.

### Process capacity

`tenancy.capacity` contains startup-only operator resource budgets. Omission
preserves finite defaults; an empty nested mapping preserves that group's
defaults. Unknown fields, null numeric values, fractions, negative values,
arithmetic overflow and values outside the consumer's representable range are
rejected. These settings do not impose limits on the number of persisted tenants,
principals, bindings, credentials, resource revisions or retained history rows.

| Group | Default fields and units |
| --- | --- |
| `execution` | `instance_active: 128`, `tenant_active: 16`, `instance_queue: 256`, `tenant_queue: 32`, `queue_wait_ms: 5000` |
| `ingress` | `requests: 128`, `bytes: 67108864` |
| `management` | `queue: 16`, `safety_queue: 8`, `reads: 8` |
| `caches` | `execution_entries: 64`, `execution_bytes: 134217728`, `credential_entries: 256`, `credential_bytes: 8388608` |
| `authority_views` | `entries: 512`, `bytes: 67108864`, including publication staging headroom |
| `routing` | `entries: 2048`, `bytes: 67108864` |
| `receipts` | `high_water_bytes: 8589934592`, `admission_free_bytes: 805306368`, `maintenance_free_bytes: 268435456`, `unknown_instance: 1000`, `unknown_tenant: 100` |
| `transport` | `connections: 256`, `header_timeout_ms: 30000` |

Only execution queue counts accept zero, meaning no waiting. Other resource
counts, byte budgets and durations are positive. Tenant execution and unknown
budgets MUST NOT exceed their corresponding instance budgets. Admission free
space MUST exceed the maintenance reserve. Count/byte products and sums MUST be
checked before allocation or permit construction. Runtime implementation limits,
such as the semaphore's representable permit count, are checked explicitly;
there is no additional product ceiling on operator counts.

Runtime budgets and public per-object limits are independent. A request can only
narrow its host budget, and increasing host capacity cannot bypass the tenancy
API's configuration, body, attempt or evidence constraints. Increasing directory
size does not increase queues, active slots, connection pools or caches. Cache
and authority-view budgets retain live ownership until its actual consumers
finish; reducing a startup budget does not delete durable authority or evidence.
CPU, allocator overhead and transient codec storage are not interchangeable with
cache payload bytes; the implementation guide describes allocation ownership.

`authority_views.bytes` includes publication headroom: one third is available
for leased and optionally retained facts and two thirds for staged replacements
and construction overlap. The storage is allocated lazily; directory entries are
not preallocated. `authority_views.entries` bounds simultaneously retained views,
not stored identities. Pressure releases optional retention before refusing new
allocation; active leases keep their ownership. Routing send ownership derives from
`execution.instance_active`; there is no separate routing-in-flight setting.

With tenancy enabled, the local inference listener uses
`transport.connections` and `transport.header_timeout_ms`, with an aggregate
request/HTTP/2-stream budget of `ingress.requests` across connections. HTTP/1
connections can use keep-alive. Each request remains subject to its wire limit.

Receipt unknown budgets govern new admission. Retained obligations above a
lowered budget remain valid evidence and MUST NOT be deleted or treated as
corruption merely to meet that budget. Storage probes and maintenance reserves
remain independent admission conditions.

`receipts.high_water_bytes` bounds accounted live receipt bytes plus outstanding
completion reservations. It is not a database-file, retained-history or total
filesystem size cap. Archived history and retained OAuth storage have separate
accounting estimates. Relocation MUST NOT be credited as physical free space.
Free-space floors are checked against the actual database volume together with
outstanding write estimates; they cannot guarantee successful allocation on a
shared filesystem. The [storage guide](../../development/tenant-service.md#receipt-archive-and-physical-storage)
describes ownership, maintenance and failure boundaries.

Changing effective capacity requires a restart under the same rules as the
rest of `tenancy`.

### Absence, nullability and admission limits

The optional import path, wrapping-key environment name and execution timeout accept omission or explicit null as
absence. Boolean, list and required numeric settings do not accept null in
place of their defaults. In particular, an omitted origin list is empty, and an
omitted weight is unknown, not zero. Unknown fields within `tenancy` are rejected.

Submission and stream-idle durations default to 60 and 120 seconds respectively;
an omitted or null total execution timeout imposes no total deadline. All
configured durations are integer seconds in 1–31,536,000: zero, negatives,
fractions, overflow and larger values are rejected, even where schema validation
alone would admit zero. Submission bounds upload and admission. The total
execution duration begins at capacity admission; idle duration independently
bounds a provider read or blocked output handoff and renews with progress.
Expiration does not authorize inference replay or imply that cleanup, rollback
or durable completion succeeded.

Startup-only `tenancy.ordered_attempt_limit` is an integer from 1 through 64
with default 8. It bounds a single ordered execution. A selection above the
limit is rejected before provider work. The limit does not change ordinary
dispatch limits, grants or routing defaults. See the
[ordered dispatch contract](../service-api/README.md#ordered-static-dispatch).

### Private generation adapter

The optional startup-only `tenancy.private_adapter` pairs this installation
with one operator-controlled adapter. It requires enabled tenancy and accepts
an `audience`, one to eight exact `endpoints` at the same HTTPS origin, and a
`key_env` naming an environment variable containing an unpadded base64url
32-byte key. Omission disables pairing. Secrets stay in the environment and
peer memory.

Only actual generation sends using a static credential on profiled ordered
routes to a configured URL receive a signed attempt claim. Provider-side input
counting is unavailable for paired endpoints. The peer must check the audience, destination,
body digest and expiry, and redeem the exact attempt against its own live
authority before making an external call. The private transport does not
follow redirects, retry or use environment proxies. Terminate TLS at the
trusted adapter endpoint so that authenticated transport protects the body.
Core accepts provider
status only after authenticating the outcome against the same attempt. An
adapter refusal, missing proof or indeterminate result stops fallback.
For a streaming success, the authenticated headers admit the stream and the
protocol decoder still requires a valid terminal.

### Provisioning authority and retained settings

Import, wrapping-key configuration and nonempty origin lists require enabled
tenancy. Import additionally requires an
explicit wrapping-key environment name. The name identifies an operator-owned
environment variable containing a stable base64-encoded 32-byte key, not the
key itself; no replacement key is generated implicitly. A missing or wrong key
cannot reopen retained encrypted resources. Setting a different key value is
not an in-place re-encryption or migration procedure.

Resource import is privileged startup input, read before listeners open. A
relative import path is resolved against the process working directory, not the
configuration directory or a tenant-selected directory. Omitting it does not
delete imported revisions. Resource mutations and evidence use the
[canonical tenancy API](../service-api/README.md#resource-provisioning).

Origin approval is a bounded list of at most 256 unique exact HTTP(S) origins,
compared after origin normalization. It is not a URL-prefix or wildcard list:
userinfo, non-root paths, queries, fragments and duplicate normalized origins
are rejected. The empty default permits no tenant-authored route origin;
installation operators retain their explicit upstream authority. The operator
owns DNS and network reachability for approved origins. Approval is checked
when a route revision is provisioned. Removing an origin, including on restart,
does not invalidate a retained revision; withdrawal requires explicit resource
revocation. No compatibility fallback may widen this allowlist.

### Technical routing weights

Per-model `routing_weight` is an optional unsigned 64-bit integer used by the
tenant runtime for `routing.sort: cost`. Lower known weights sort first;
omission or null sorts after known weights, and equal or jointly missing values
preserve retained candidate order. Zero is a valid known weight. Writers MUST
preserve exact integers rather than round through floating-point values.
The weight has no currency, tariff, discount or payable-usage unit; published
prices do not provide its default. It orders only the already authorized
candidate set and cannot add an endpoint or grant credential use.

Changing configuration does not rewrite an immutable tenant route. Publish
and explicitly select a new resource/configuration revision for new weights.
The [tenant routing guide](../../development/tenant-routing.md) describes
dispatch ordering, rate limits, health and fallback.

## Shared-process profile

`gateway_state.mode: shared` must be selected literally before process-local
configuration discovery. A database URL or environment interpolation does not
implicitly select that mode. The selected process YAML has a narrower format
than ordinary application YAML: it accepts `server` (`listen`, `log_level`,
`skip_auth`), `database.url`, `gateway_state`, optional `telemetry` and
`operator`, plus explicit `inherit_defaults: false` and
`registry: {enabled: false}`. Provider routing, policies and credential authority
are published independently; they cannot be injected as extra process keys.
This profile is enforced by the
[shared-process parser](../../../apps/bitrouter/src/config/shared_process.rs),
not by treating the full application schema as sufficient validation.

Environment references in shared YAML must be quoted string values. Bounds
remain literal numbers. Shared-mode selection and field names cannot be
substituted. The deployment key is an environment-variable reference, not its
secret value. The [shared gateway guide](../../../skills/bitrouter/references/shared-gateway.md)
contains a complete deployment example and current feature restrictions.

## Service policy source

Local files and managed requests use the [Policy contract](../policy/README.md).
Shared publication carries typed `model_policy`. Its ACL `policies` and
principal `policy_id` fields govern access control independently.

## Persistent dataset selection

The [service store contract](../service-api/README.md#resource-and-evidence-migration-and-rollback)
selects exact current tenant/shared formats independently of YAML and Policy
SemVer. Old datasets are refused even with Policy-free active heads. Validation
does not authorize migration or adding a current marker to old data. New data
locations/installation identity are explicit; failed opens never clear, rewrite
or replace old data. Binary rollback uses the old binary and its own consistent
data/configuration/key copy, never a downgraded new store.

## Compatibility cases

- Parse the ordinary local example and reject an unknown root key. Check nested
  openness against the corresponding schema rather than applying one rule to
  all objects. Missing values retain declared defaults and units.
- Reject unknown keys and duplicate mapping fields where the schema requires
  unique keys. Preserve source files on validation failure.
- Select shared mode literally; reject an environment-selected mode, unknown
  shared process key or routing authority embedded in that process document.
- Load Policy from the conventional file or an explicit path. Reject invalid
  references, unsupported versions and missing explicitly selected files.
- Verify exact route, unknown-family fallback and default. Compilation preserves
  active source; explicit stale CAS blocks publication.
- A document without tenancy settings leaves tenancy disabled. Omitted and null
  optional values retain their absence semantics; reject null required values,
  unknown tenancy keys and invalid timeout bounds, including schema-valid zero.
- Enabled tenancy rejects anonymous ingress, absent operator authority, shared
  mode, in-memory/non-SQLite databases and non-Unix ownership.
  Changed effective tenancy settings reject reload before publishing a candidate.
- Omitted capacity groups and partial nested mappings preserve the defaults of
  omitted fields. Reject null, negative and fractional numeric budgets, overflow,
  invalid tenant/instance relationships and nonrepresentable runtime permits.
  Zero execution queue counts disable waiting; zero active/byte budgets fail.
  Counts above the defaults are accepted when representation and relationships
  permit them. A lowered unknown budget preserves retained obligations.
- Empty origins deny tenant route provisioning. Normalized duplicates, paths
  and userinfo fail validation; origin comparison never expands wildcards or
  matches URL prefixes. Removing an approved origin leaves retained revisions
  intact until explicit revocation.
- A valid restart preserves imported and managed resources when the import
  path is omitted. Retained encrypted resources fail
  closed with a missing or wrong wrapping key; restart does not generate one.
- Weights 0 and larger exact unsigned integers sort ascending, unknown weights
  sort last and ties retain order. Omitted/null weights never inherit prices.
  Verify exact ordering after restart.
- Reject old/unmarked/partially initialized/foreign/future stores and forged
  markers before authority or recovery writes. Preserve records, source bytes
  and provider counts. Current restart preserves receipts and cumulative budgets.
