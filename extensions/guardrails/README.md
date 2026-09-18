# Guardrails extension

| Directory | Cargo package | Purpose |
| --- | --- | --- |
| `matcher/` | `bitrouter-guardrails` | Rules and matcher; defaults to no SDK dependency |
| `service/` | `bitrouter-guardrails-service` | Independent `bitrouter-guardrails` executable implementing HTTP request-check v1 |

The shared wire contract remains in `crates/bitrouter-checker-protocol`, because
both the default host and independent implementations consume it.

## Use with the default host

Run the service and bind it explicitly to a named router. It supports entry-text
allow/deny only. See [service configuration](service/README.md) and the
[operation and migration guide](../../docs/GUARDRAILS_EXTENSION.md).

```sh
cargo run --release -p bitrouter-guardrails-service -- --rules rules.yaml
```

This command runs from the workspace root after creating the documented rules
file. The default host does not link the matcher or launch this process.

## Use in a trusted custom Rust host

The matcher is an ordinary Rust library. Explicitly enabling its `sdk` feature
retains the compatibility `GuardrailsPlugin` and hook APIs. Static linking
requires rebuilding the custom host; it does not install code into an existing
`bro` binary.

Those compatibility hooks use the existing global/per-request rule deposit and
stream-hook semantics. They are not a native implementation of router-bound
`checks.request`, and do not automatically acquire its projection, receipt or
failure semantics. The HTTP input service and old stream block/redact behavior
must not be presented as equivalent protection.

## Build and test

```sh
cargo check -p bitrouter-guardrails --no-default-features
cargo test -p bitrouter-guardrails --all-features
cargo test -p bitrouter-guardrails-service
```

The directory move preserves both package names, the executable name, wire v1,
CLI flags, configuration and archive names. Release packaging builds the service
separately from `bro`; source grouping does not change their dependency boundary.
