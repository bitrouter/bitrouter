# BitRouter configuration formats

Status: **beta, specification revision 1.0.0-beta.2**. This is the single
format contract for `bitrouter.yaml`, its shared-process profile and the
referenced `policy-lock.yaml`. These are related configuration artifacts; this
document's revision does not add a version field to existing YAML.
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
runtime acceptance. The schema, parser, migration behavior, examples and this
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
`presets`, `variants`, `plugins`, `mcp`, `mcp_servers`, `skills`, `server_tools`,
`agents`, `inherit_defaults`, `registry` and `policy`. Their exact shapes and
field units are defined by the generated schema. Environment substitution in
the local profile precedes parsing and semantic validation. Operator instructions
for source selection, substitutions and validation are in the
[CLI reference](../../CLI.md#config-resolution).

Named routers are selected by `bitrouter/<id>`. Router selection, defaults and
ordered request checks are separate fields. Legacy preset/variant forms have
explicit compatibility resolution; readers MUST NOT reinterpret them as an
unrelated second execution pipeline. Configuration inspection or execution
MUST NOT implicitly rewrite an old document. File migration is an explicit
operator action. The `policy.path` reference is resolved relative to the
configuration file; when omitted, the file-backed default is `policy-lock.yaml`
beside `bitrouter.yaml`.

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

## Policy lock format 4

`policy-lock.yaml` has its own discriminator, `lockfileVersion: 4`. Only version
4 is accepted; versions 1–3, unknown fields in strict structures and scalar tier
targets are rejected. The document contains required artifact lineage, named
`policies` and optional route `certificates`. The artifact contains
`evidence_root`, `source_snapshot_time_unix_ms` and `compiler` (`id`, `version`,
`config_digest`), with optional `parent_digest` and `eval_snapshot_root`.
Each explicit route requires an appropriate certificate.

A policy contains `key_strategy`, `tiers`, `routes`, default/tool tier selection
and its declared guard/predictor/optimization fields. Each tier is a structured
action with `model`, required `context` (`preserve` or `evidence`) and optional
`effort`. Context is part of action identity. A semantic key has the form
`semantic_route/v1|<family>|<role>|<risk>`; lookup uses the exact cell,
unknown-family cell, then the default. Unsupported versions, enum values or
required semantics MUST NOT be inferred from familiar field names.

The [lock types and validator](../../../apps/bitrouter/src/policy_lock.rs)
define nested fields, certificate constraints and admitted combinations. The
[bundled lock](../../../apps/bitrouter/policy/default.yaml) is a complete example;
its model membership is illustrative, not a format guarantee. Existing
certificates cannot be relabeled for another action or classifier cohort.
Publication requires the expected parent and preserves concurrent edits;
compiling a candidate does not itself activate it. The learning workflow and
implementation ownership are documented in the
[evidence guide](../../development/evidence-learning.md).

## Compatibility cases

- Parse the ordinary local example and reject an unknown root key. Check nested
  openness against the corresponding schema rather than applying one rule to
  all objects. Missing values retain declared defaults and units.
- Validate an old preset document through its explicit compatibility resolver;
  inspection does not rewrite the file. A migration requires explicit input.
- Select shared mode literally; reject an environment-selected mode, unknown
  shared process key or routing authority embedded in that process document.
- Load a valid v4 lock with artifact lineage. Reject versions 1–3, scalar tier
  targets, invalid references and an explicit route without its certificate.
- Verify exact route, unknown-family fallback and default in order. A candidate
  compilation leaves the active lock unchanged; a changed parent blocks publish.
- An old document without tenancy settings stays disabled. Omitted and null
  optional values retain their absence semantics; reject null required values,
  unknown tenancy keys and invalid timeout bounds, including schema-valid zero.
- Enabled tenancy rejects anonymous ingress, absent operator authority, shared
  mode, in-memory/non-SQLite databases, non-Unix ownership and the host task API.
  Changed effective tenancy settings reject reload before publishing a candidate.
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
- Migration leaves immutable route revisions unchanged. Rollback uses a verified
  compatible configuration and store/key backup, without deleting new evidence
  or revocation metadata to satisfy an older reader.
