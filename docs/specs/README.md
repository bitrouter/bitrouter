# Maintained format specifications

Specifications are reserved for formats with significant forward/backward
compatibility consequences. Admission and review follow the
[documentation policy](../DEVELOPMENT.md#specification-admission-and-review).

| Object | Canonical specification | Why it needs a maintained format contract |
| --- | --- | --- |
| Published model/provider registry | [Registry v1](registry-v1/README.md), `registry/1.0.0-beta.7` | Independent publishers/readers, persisted caches, identities, schema and revision compatibility |
| Application configuration | [Configuration formats](configuration/README.md), `configuration/1.0.0-beta.5` | Existing YAML files, defaults, startup tenancy, routing weights and strict readers |
| Portable model-routing policy | [Policy](policy/README.md), `policy/1.0.0-beta.1` | Independently authored policy files, SDK submissions, retained source and model-only execution semantics |
| BitRouter-owned service messages and APIs | [Service API](service-api/README.md), `service-api/1.0.0-beta.6` | Independently updated clients/services, authentication and instance binding, resource revisions, usage evidence, errors, replay, cursors and persistent identities |

The M0 Configuration, service and Policy cutover clauses remain accepted
requirements for implementation and conformance qualification. Policy beta.1
retains its unchanged unpublished source; Registry beta.7 is unchanged.

## Contract identifiers and revision dimensions

A normative contract is identified as `<namespace>/<SemVer>`, for example
`policy/1.0.0-beta.1` or `registry/1.0.0-beta.7`. The complete string is a
qualified contract identifier; only its suffix is a SemVer version. Namespaces
are lowercase, exact and centrally listed here. Versions from different
namespaces are unrelated. Build metadata MUST NOT select different semantics.
Beta support is explicit: accepting beta.1 does not imply beta.2 or stable 1.0.0.
An incompatible beta change increments `beta.N`; published semantics are not
replaced under an existing identifier. Stable contracts state their compatible
extension rules and follow SemVer. Editorial clarification does not change the
contract's accepted data or behavior.

New independently consumed artifacts carry a required `spec_version` with that
qualified identifier. Missing, wrong-namespace and unsupported versions fail
visibly. This convention does not silently change existing wire fields:
Registry beta.7 still carries integer `version` and unqualified `spec_revision`;
`registry/1.0.0-beta.7` is its qualified reference in prose. Changing its envelope
requires its own reviewed beta revision, publisher, reader and cache tests.

Keep these dimensions separate:

- Contract identity/version specifies interpretation and compatibility.
- Resource revisions, head CAS, authority revisions and configuration generations
  identify mutable-management state; they are not SemVer versions.
- Content digests identify bytes or a defined canonical representation. Each API
  states whether it hashes public source or compiled internal representation.
- Software and SDK packages retain their independent package-manager SemVer.
- URL namespaces and message discriminators retain their own scoped semantics;
  third-party protocols keep upstream versioning.
- JSON Schema `$schema` identifies the validation dialect, while `$id` identifies
  the schema resource. Neither replaces the artifact's `spec_version`.

Do not duplicate a contract discriminator with another independently editable
major/version field. A transport or feature is not a new contract namespace
unless it owns a genuinely independent compatibility boundary.

Each object has one specification. A transport, profile or feature is a section
of that object's contract when it needs the same compatibility review; it does
not get another independently maintained spec. Supporting schemas, examples and
conformance cases are indexed by the canonical entry. Existing build-consumed
schemas may retain their shared contract paths; do not create a duplicate schema.
The service specification indexes the resource-provisioning schemas and packaged
usage-evidence schemas at their existing paths, with compatibility cases under
[resource provisioning](service-api/README.md#resource-provisioning) and
[execution usage evidence](service-api/README.md#execution-usage-evidence).
Those supporting files are not separate specifications. Keep revision/stability
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
