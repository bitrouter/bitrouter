# BitRouter configuration formats

Status: **accepted M0 revision, implementation pending, `configuration/1.0.0-beta.5`**.
This is the format contract for
`bitrouter.yaml`, its shared-process profile and Policy source loading.
The beta.5 cutover is pending parser/schema/DTO and conformance implementation.
Portable service routing policies have their own
[Policy contract](../policy/README.md). This document's revision does not add
a version field to existing application YAML.
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
`bro config validate` uses that parser; JSON Schema validation alone is not
runtime acceptance. The schema, parser, rejection behavior, examples and this
contract must be reviewed together. Do not hand-maintain another copy of the
complete field catalog in prose.

## Application YAML

`bitrouter.yaml` is a YAML mapping. There is currently no top-level format-version
field. The top-level object rejects unknown fields; nested objects follow their
own schema and parser rules. Absence selects declared defaults, rather than a
universal null value. Opaque `plugins` entries are not evidence that the host
implements or activates the named plugin. In-memory derived registry records
are not local YAML fields.

The root groups are `server`, `control`, `tenancy`, `agent_api`, `decision_model`,
`chat`, `upstream`, `database`, `gateway_state`, `eval`, `trajectory`,
`acp_recording`, `continuation`, `providers`, `models`, `routers`, `checkers`,
`plugins`, `mcp`, `mcp_servers`, `skills`, `server_tools`,
`agents`, `inherit_defaults`, `registry` and `policy`. Their exact shapes and
field units are defined by the generated schema. Environment substitution in
the local profile precedes parsing and semantic validation. Operator instructions
for source selection, substitutions and validation are in the
[CLI reference](../../CLI.md#config-resolution).

Named routers use `bitrouter/<id>`. Selection, defaults and ordered request
checks are separate. `presets`, `variants`, old router addresses and aliases
are rejected without conversion. Ordinary provider/model selectors and upstream
protocol versions keep their meanings. Policy bindings select literal names;
there is no variant-derived selection or base-name fallback.

`policy` is closed and accepts only optional `path`. Unknown/deleted keys,
including `mode`, `writeback`, `locked` and `evolve`, fail even with values
that previously disabled them. Paths resolve relative to the configuration;
the conventional omitted path is `policy.yaml` beside `bitrouter.yaml`.
There is no implicit discovery of `policy-lock.yaml`. Every path accepts only
public Policy source. An explicit missing, invalid or unsupported file fails
without default fallback. Local zero-configuration may use the maintained bundled
source when no user source is selected; no user file is overwritten. Tenant
initialization explicitly submits source and never inherits host files/defaults.

Policy init/check/show/status/diff and explicit publish/rollback use that source.
Active is the default for the selected running instance; disk is explicitly
selected for offline checks and never replaces failed active/remote reads.
Publication takes explicit expected-head/CAS. Filesystem/process permissions
control local writes; service authentication/scope/CAS control service writes.
There is no automatic learning publication, v4 migration or legacy compile/evolve/
evidence-verification entry point in mature Policy commands.

A structural minimal local example is:

```yaml
server:
  listen: 127.0.0.1:4356
registry:
  enabled: false
```

This example only illustrates fields; it does not supply provider credentials
or guarantee a runnable inference route.

## Tenancy and routing compatibility

Revision beta.3 adds startup runtime budgets independent of directory size.
Revision beta.2 records the startup tenancy and per-model routing-weight
semantics below. The `TenancyConfig` and `ProviderModel` definitions in the
[generated schema](../../../dist/schema/bitrouter.config.schema.json) remain
their field catalogs; this revision adds no YAML version field. The
[tenancy validator](../../../apps/bitrouter/src/config/tenancy.rs) also checks
deployment, bounds and references that the generated schema alone cannot prove.

### Opt-in and startup ownership

Omitting `tenancy` or supplying an empty mapping preserves the disabled default.
Tenancy requires explicit enablement, authenticated inference, an enabled control
listener and an explicit `control:tenancy` operator credential. The supported
profile is one Unix process exclusively owning a file-backed SQLite database
with `gateway_state.mode: local`. In-memory SQLite, non-SQLite stores, shared
gateway mode and the host coding-task API are not accepted for this profile.
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
keep-alive remains available. The per-request wire limits remain unchanged.

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
rest of `tenancy`. Older strict readers reject the new keys. Operators MUST use
a compatible binary; removing a configuration key is not a database downgrade.

### Absence, nullability and admission limits

The schema's optional import path, learning-registration path, wrapping-key
environment name and execution timeout accept omission or explicit null as
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

Revision beta.3 adds startup-only `tenancy.ordered_attempt_limit`, an integer
from 1 through 64, default 8. It bounds one explicitly negotiated ordered v2
execution, not the tenant directory or ordinary v1 requests. A submitted
selection above the limit is rejected before provider work; it is never
truncated. Raising the limit does not widen any grant, change routing defaults,
or reserve 64 attempts for ordinary executions. Old configurations retain the
default. An old reader can reject this new key; omission is the compatible
configuration for a v1-only binary, whose store compatibility must also be
checked before rollback. See the
[ordered dispatch contract](../service-api/README.md#ordered-static-dispatch-v2).

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

Import, learning-registration and wrapping-key options require tenancy to be
enabled; nonempty origin lists do as well. Import additionally requires an
explicit wrapping-key environment name. The name identifies an operator-owned
environment variable containing a stable base64-encoded 32-byte key, not the
key itself; no replacement key is generated implicitly. A missing or wrong key
cannot reopen retained encrypted resources. Setting a different key value is
not an in-place re-encryption or migration procedure.

Resource import is privileged startup input, read before listeners open. A
relative import path is resolved against the process working directory, not the
configuration directory or a tenant-selected directory. Omitting it does not
delete previously imported revisions. Omitting learning registrations likewise
preserves durable schedules; that facility requires the `core` and `harness`
features. Resource mutations and evidence use the
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

The compiler's retained target identity and encrypted envelope format are
unchanged. Existing retained routes without weights keep their missing-weight
semantics. Changing a configuration file does not rewrite an immutable tenant
route; publish and explicitly select a new resource/configuration revision for
new weights. Runtime ordering, rate limits, health and fallback are described in
the [tenant routing guide](../../development/tenant-routing.md).

### Reader compatibility and rollback

A new reader of old YAML uses the declared defaults and does not rewrite the
file or opt into tenancy. An older strict reader can reject new tenancy keys;
an older permissive model reader may accept `routing_weight` while ignoring its
meaning. Syntactic acceptance is therefore not proof of the requested routing
behavior. Writers MUST target a reader known to implement required settings;
they MUST NOT strip origin, lifetime or ownership constraints just to make an
older binary accept the file.

Binary rollback requires an explicitly reviewed compatible configuration and
the [service data rollback rules](../service-api/README.md#resource-and-evidence-migration-and-rollback).
Preserve a verified database/configuration/wrapping-key set before upgrading.
Removing new YAML keys does not downgrade retained heads, revocations or evidence,
and an older binary is not qualified to reopen that store merely because its
configuration parser succeeds.

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

File, API, default, SDK and export inputs use the
[Policy contract](../policy/README.md), exactly `policy/1.0.0-beta.1`.
Shared publication format 2 carries typed `model_policy`; format 1,
`policy_lock` and mixed fields are rejected before mutation. ACL `policies`
and principal `policy_id` remain independent access-control objects. BRO
experiments need an explicit harness owner and cannot reuse the retired lock
reader or submit context/learning state as model-only source.

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
- Reject old presets/variants, addresses and Policy aliases from raw YAML,
  including duplicates and removed nested keys; leave files unchanged.
- Select shared mode literally; reject an environment-selected mode, unknown
  shared process key or routing authority embedded in that process document.
- Load public source from policy.yaml, explicit paths and bundled defaults.
  Reject old lock discriminators, invalid references and unsupported versions.
  Explicit missing/invalid paths never discover defaults or an old lock file.
- Verify exact route, unknown-family fallback and default. Compilation preserves
  active source; explicit stale CAS blocks publication.
- An old document without tenancy settings stays disabled. Omitted and null
  optional values retain their absence semantics; reject null required values,
  unknown tenancy keys and invalid timeout bounds, including schema-valid zero.
- Enabled tenancy rejects anonymous ingress, absent operator authority, shared
  mode, in-memory/non-SQLite databases, non-Unix ownership and the host task API.
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
- A valid restart preserves imported/managed resources and durable schedules
  when import/registration paths are omitted. Retained encrypted resources fail
  closed with a missing or wrong wrapping key; restart does not generate one.
- Weights 0 and larger exact unsigned integers sort ascending, unknown weights
  sort last and ties retain order. Omitted/null weights never inherit prices.
  Test old-reader rejection or ignored-field behavior without treating it as
  support; new-reader defaults preserve old unweighted routes.
- Reject old/unmarked/partially initialized/foreign/future stores and forged
  markers before authority or recovery writes. Preserve records, source bytes
  and provider counts. Current restart preserves receipts and cumulative budgets.
