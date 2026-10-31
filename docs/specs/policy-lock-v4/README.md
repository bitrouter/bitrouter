# Routing policy lock format v4

Status: **beta, specification revision 1.0.0-beta.1**; file format
`lockfileVersion: 4`. The document revision and lockfile version are distinct.
This profile describes the parser and serving boundary at `80550c9b` and may
change incompatibly during beta. MUST, MUST NOT, SHOULD and MAY express
requirements on producers and consumers.

## Envelope and targets

The YAML document contains `lockfileVersion`, required `artifact` lineage,
`policies` keyed by policy name and optional `certificates` keyed by policy and
route. The artifact identifies `evidence_root`, `source_snapshot_time_unix_ms`,
and `compiler` (`id`, `version`, `config_digest`); optional lineage fields
include `parent_digest` and `eval_snapshot_root`. Each explicit route requires
an appropriate certificate. Nested schema and accepted fields are defined by the
[policy types and validator](../../../apps/bitrouter/src/policy_lock.rs); the
[bundled lock](../../../apps/bitrouter/policy/default.yaml) is a complete
illustrative artifact whose model IDs are not a support guarantee.

A policy defines named `tiers`, routes and a default; a tier is a structured
target with `model`, required `context` (`preserve` or `evidence`) and optional
`effort`. A semantic route key has the form
`semantic_route/v1|<family>|<role>|<risk>`. Resolution tries the exact cell,
then the unknown-family cell, then the default. A selected tier expresses route
intent; the provider catalog, context owner and execution authority independently
admit the actual request. Classification is not authorization to omit context.

## Validation and publication

Version 4 is the only accepted version; versions 1–3 and unknown fields in the
strictly deserialized structures are rejected. Scalar tier targets and removed
legacy aliases MUST NOT be interpreted as v4. Producer and consumer MUST retain
certificate and classifier cohort relationships rather than silently applying
a certificate to a different route or cohort. Unknown prices or evidence do not
mean zero cost or positive quality.

Compilation produces a candidate; it does not itself activate serving. Guarded
publication checks the expected parent and atomically replaces the artifact; a
concurrent edit fails rather than being overwritten. No compatibility promise
is made for unknown future enum values or version numbers; a reader MUST fail
closed instead of guessing their meaning. See [conformance](conformance.md).
