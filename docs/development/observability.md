# Observation and export

This development guide follows the current implementation. Update it with
changes to ownership, adapters and runtime behavior. Format compatibility is
maintained in the [specification index](../specs/README.md).

The machine-readable [observation schema](../../crates/bitrouter-ai/span-schema.json)
defines span names and kinds, parent relationships, attribute names, value
types, conditions, events, metrics and extension restrictions. Its scope name
is `io.bitrouter.observe`. Resource attributes include `service.name` and
`service.version`; scope version identifies the rendering crate. Export uses
[OpenTelemetry Protocol](https://opentelemetry.io/docs/specs/otlp/) where the
optional exporter is enabled. Merely accepting a request does not prove export,
delivery or durable collection.

The schema distinguishes required attributes on normally completed lifecycles
from conditional attributes. An abandoned span reaped after a failure can carry
only the subset available at creation. A consumer must not interpret an absent
conditional value as zero, an observed HTTP 200 as proof of a complete stream,
or a missing exported span as proof that an operation did not run. Spans and
metrics are observability evidence, not a billing or authorization authority.

Consumers should ignore unrecognized optional attributes and must not assume an
unknown type or semantic convention has known meaning. Extension attributes
must respect the schema's reserved prefixes and declared value restrictions.

## Exporter and host boundaries

The application owns OTLP transport and lifecycle. The AI crate owns the
observation declaration; the SDK carries service data without owning an
exporter. Use the exporter instance's tracer and meter. Do not implicitly
install a global provider: a tracer captured from globals before initialization
can remain a no-op, while replacing globals can interfere with another host
consumer.

Parent resolution considers an active OTel context, then the tracing bridge,
then inbound W3C context. Hosts that create tracing ingress spans need the bridge
to retain parentage. ACP context-window occupancy is not per-turn token usage;
keep occupancy attributes separate from usage counters. Unknown metrics and
incomplete span lifecycles must not become successful billing facts.

The [committed schema](../../crates/bitrouter-ai/span-schema.json) is the single
field/name/units catalog. Collector dashboards depend on those names: update the
schema declaration, committed artifact and consuming tests together and review
compatibility before renaming them. A second prose specification would duplicate
that catalog. Structural and lifecycle checks live beside the declaration and
exporter; dated test results do not belong in this guide.
