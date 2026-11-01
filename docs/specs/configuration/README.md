# BitRouter configuration formats

Status: **beta, specification revision 1.0.0-beta.1**. This is the single
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
