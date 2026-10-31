# Observation schema profile v2

Status: **beta, specification revision 1.0.0-beta.1**; observation schema
version `2`. This profile describes the checked-in observation declarations at
`80550c9b`. The schema version, instrumentation scope version and this draft
revision are distinct; beta revisions may change incompatibly. MUST, MUST NOT,
SHOULD and MAY express requirements on producers and consumers.

The machine-readable [observation schema](../../../crates/bitrouter-ai/span-schema.json)
defines span names and kinds, parent relationships, attribute names, value
types, conditions, events, metrics and extension restrictions. Its scope name
is `io.bitrouter.observe`. Resource attributes include `service.name` and
`service.version`; scope version identifies the rendering crate. Export uses
[OpenTelemetry Protocol](https://opentelemetry.io/docs/specs/otlp/) where the
optional exporter is enabled. Merely accepting a request does not prove export,
delivery or durable collection.

The schema distinguishes required attributes on normally completed lifecycles
from conditional attributes. An abandoned span reaped after a failure can carry
only the subset available at creation. A consumer MUST NOT interpret an absent
conditional value as zero, an observed HTTP 200 as proof of a complete stream,
or a missing exported span as proof that an operation did not run. Spans and
metrics are observability evidence, not a billing or authorization authority.

Consumers SHOULD ignore unrecognized optional attributes and MUST NOT assume an
unknown type or semantic convention has known meaning. Extension attributes
must respect the schema's reserved prefixes and declared value restrictions.
The committed declaration and [conformance cases](conformance.md) must evolve
together; there is no stable schema compatibility commitment in beta.
