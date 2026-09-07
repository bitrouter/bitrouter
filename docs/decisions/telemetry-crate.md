# Decision: keep telemetry rendering outside the SDK

Status: **accepted and implemented**

Date: 2026-08-22

## Context

BitRouter has two related but different concerns:

- the observability contract—span names, attributes, events, metrics, and the
  lifecycle events exposed to integrations; and
- an OpenTelemetry implementation that renders that contract and exports it
  over OTLP.

Keeping both in `bitrouter-sdk` made transport libraries and their public API
commitments part of the foundation crate even for consumers that did not use
OpenTelemetry. Keeping the schema only in the renderer would instead let an
optional implementation define BitRouter's interoperability contract.

## Decision

- `bitrouter-sdk::observe` owns the dependency-light observability contract.
  Its schema declaration lives in
  [`crates/bitrouter-sdk/src/observe/schema.rs`](../../crates/bitrouter-sdk/src/observe/schema.rs)
  and renders the committed
  [`span-schema.json`](../../crates/bitrouter-sdk/span-schema.json).
- `bitrouter-telemetry` owns optional OpenTelemetry rendering, OTLP transport,
  exporter configuration, batching, and cardinality controls.
- `apps/bitrouter` composes the SDK contract with telemetry and any other
  `ObserveHook` implementations. Telemetry remains one consumer of the
  contract, not the definition of observability.
- Heavy implementations remain feature-gated. Disabling a feature must remove
  a meaningful dependency set; feature flags are not module-visibility knobs.

## Stable identities

`telemetry` is what an operator configures; `observe` is the contract BitRouter
promises and the identity it exposes on the wire. Keep these names stable:

- `io.bitrouter.observe` is the OTLP instrumentation scope;
- `bitrouter` is the meter name;
- `bitrouter::observe::http`, `bitrouter::observe::cardinality`, and
  `bitrouter::observe::span_attributes` are documented `RUST_LOG` targets; and
- the first line of the removed-Prometheus `/metrics` response is an
  operator-facing compatibility string.

The implementation and configuration use `bitrouter-telemetry` instead:
`plugins.bitrouter-telemetry.*`, `BITROUTER_TELEMETRY_CONTENT_CAPTURE`, and
`BITROUTER_TELEMETRY_CONTENT_ATTR_MAX_BYTES`. Startup diagnostics must report
the old plugin id and unknown plugin ids, because silently ignoring a renamed
or misspelled block can disable telemetry or guardrails without an error.

## Ingress and tracing interoperability

BitRouter's HTTP middleware creates an OpenTelemetry server span directly and
attaches its context to the request future. Its export must not depend on a
`tracing` filter, so suppressing the diagnostic
`bitrouter::observe::http` target cannot orphan request spans.

When choosing a parent for a model-call span, use the first available source in
this order: BitRouter's native request context, a current context provided by
the `tracing-opentelemetry` bridge, then an inbound `traceparent`. The bridge
remains public because embedding hosts may own their ingress spans; a public
multi-tenant edge may also deliberately refuse caller-supplied trace context.

The bridge receives a tracer from the exporter. Do not replace it with a
global tracer lookup: BitRouter intentionally does not install a global tracer
provider, both to avoid clobbering another OpenTelemetry consumer and because a
lookup without one silently returns a no-op tracer.

## Invariants

1. No `opentelemetry*`, `tracing_opentelemetry`, `tracing_core`, or
   `tracing_subscriber` type may enter the public `bitrouter-sdk` API.
2. The SDK's schema module names no OpenTelemetry type and adds no dependency
   beyond the contract's serialization needs.
3. A telemetry renderer must conform to the committed span schema rather than
   re-derive names or meanings from call sites.
4. The application may register multiple observation consumers; code must not
   assume that the telemetry crate is the only `ObserveHook` implementation.

The `sdk-public-api` and `feature-isolation` CI jobs guard the crate boundary.
Schema changes are checked by the committed-artifact test:

```sh
UPDATE_SPAN_SCHEMA=1 cargo test -p bitrouter-sdk committed_artifact
```

Run it without `UPDATE_SPAN_SCHEMA` to verify that the committed artifact is
current.

## Consequences

- SDK consumers that do not export OTLP do not compile or semver-commit to the
  OpenTelemetry stack.
- Deployments choose a renderer without changing the shared event vocabulary.
- Wiring crosses a crate boundary, but that boundary makes accidental coupling
  fail structurally instead of relying on prose.
- User-facing telemetry configuration belongs in `bitrouter-docs` and the
  shippable BitRouter skill, not in this decision record.

## Rejected alternatives

### Put the complete OpenTelemetry stack in `bitrouter-sdk`

Rejected because transport and vendor integration are deployment concerns and
would add permanent public dependency commitments to the foundation crate.

### Put the schema in `bitrouter-telemetry`

Rejected because the schema is an interoperability contract used independently
of a particular renderer.

### Keep the boundary only as comments inside one crate

Rejected because comments cannot prevent a renderer type or dependency from
leaking into the SDK's public surface.

## Reconsideration triggers

Revisit the split only if the observability contract itself requires a heavy
renderer dependency, the telemetry crate ceases to have an independent
consumer-facing purpose, or a different boundary can enforce the same public
API and dependency isolation more directly.
