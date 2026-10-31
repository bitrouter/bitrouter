# Maintained format specifications

Specifications are reserved for formats with significant forward/backward
compatibility consequences. Admission and review follow the
[documentation policy](../DEVELOPMENT.md#specification-admission-and-review).

| Object | Canonical specification | Why it needs a maintained format contract |
| --- | --- | --- |
| Published model/provider registry | [Registry v1](registry-v1/README.md), beta `1.0.0-beta.6` | Independent publishers/readers, persisted caches, identities, schema and revision compatibility |
| Application and routing-policy configuration | [Configuration formats](configuration/README.md), beta `1.0.0-beta.1` | Existing YAML files, defaults, references, strict readers, explicit migration and policy artifact versions |
| BitRouter-owned service messages and APIs | [Service API](service-api/README.md), beta `1.0.0-beta.1` | Independently updated clients/services, authentication and instance binding, errors, replay, cursors and persistent identities |

Each object has one specification. A transport, profile or feature is a section
of that object's contract when it needs the same compatibility review; it does
not get another independently maintained spec. Supporting schemas, examples and
conformance cases are indexed by the canonical entry. Existing build-consumed
schemas may retain their shared contract paths; do not create a duplicate schema. Keep revision/stability
status distinct from URL, message, file-format and software-release versions.

A change requires deliberate review of both compatibility directions, rejected
and unknown values, defaults, migrations and failure/replay semantics. Update
schemas/DTOs, examples and cases with the specification. Beta changes may be
incompatible, but must be explicit and reviewed. Publishing a beta draft does
not assert implementation availability in every feature or deployment profile.

Implementation, module ownership, adapter behavior and architecture live in
[development guides](../development/README.md) and evolve with code. Existing
machine-readable observation declarations remain their single field catalog;
adding a second prose spec would duplicate it. Third-party protocols retain
their upstream standards. Neither a file named `SPEC` nor a list of current
runtime behaviors is sufficient reason to create a new specification.
