# System One JSON format extension

`evaluation_format` is the typed capability; `system-one/json@1` is one
upstream wire contract. The `format/` Cargo package registers only request
rendering and successful-response parsing. It owns no TypeSafe credentials,
endpoint, transport, pricing, or model catalog entry.

The TypeSafe `typesafe/jev-1.13` provider route is the first verified binding.
Its provider configuration selects `/v1/systemone`, supplies the actual wire
model id and limits, and binds the exact format revision. Similar names or
question types do not establish another provider's wire compatibility; each
new binding needs independent conformance evidence. Divergent wire behavior
requires a separate format identity or revision, not a provider-id branch in
this adapter.

The opt-in `bitrouter-evaluation-host` package under `apps/` links this format
and runs the shared foreground host lifecycle. Stock `bro` does not register
it or expose `/v1/evaluate`. Both use the same `ExtensionApi` author entry;
the host owns authentication, HTTP execution, deadlines, retries, metering,
and graceful shutdown. Native code is trusted and must be rebuilt with the
host; this directory is not a runtime plugin or sandbox.

For operator setup and the public request shape, see
[`skills/bitrouter/references/evaluation.md`](../../skills/bitrouter/references/evaluation.md).
