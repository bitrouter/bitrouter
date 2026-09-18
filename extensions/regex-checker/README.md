# Regex checker extension

`request-check` is the capability; `regex-checker` is one implementation.
Configured rules decide its purpose: restricted text, secret-like patterns, or
selected personal identifiers. This is not a comprehensive PII detector.

| Directory | Cargo package | Purpose |
| --- | --- | --- |
| `matcher/` | `bitrouter-guardrails` | Reusable rules and `checker::callback`; old library name retained for compatibility |
| `service/` | `bitrouter-regex-checker` | Optional HTTP delivery; executable also named `bitrouter-regex-checker` |

Both delivery modes use the same ordinary function:
`Fn(&v1::Request) -> CheckDecision + Send + Sync`. The bounded input and decision
contract live in `bitrouter-checker-protocol`; there is no general extension
registry, manifest or dynamically loaded Rust code.

## Default bro: HTTP delivery

Create the [rules file](service/README.md), then run from the workspace root:

```sh
cargo run --release -p bitrouter-regex-checker -- --rules rules.yaml
```

Bind the endpoint through `checkers.<id>` and the intended router's
`checks.request`. The official bro binary neither links the matcher nor starts
this service. See the [operation and migration guide](../../docs/GUARDRAILS_EXTENSION.md).

## Custom Rust host: native delivery

Link the matcher with default features and register `checker::callback(rules)`
using `bitrouter::request_checks::NativeChecker`. Assemble through
`bitrouter::assemble::build_app_with_checkers`. The configuration is explicit:

```yaml
checkers:
  secret-check:
    native:
      revision: secret-rules-v1
# Under the intended routers.<id>:
# checks:
#   request:
#     - checker: secret-check
#       timeout_ms: 500
```

The map key, `secret-check`, is the same instance id used by router bindings.
The registration revision must match configuration and must change when code or
rules change. It is a declared identity, not cryptographic attestation. Missing,
mismatched and unused registrations fail activation. A configured registration
without a router binding stays inert; a probe invokes it explicitly with
synthetic text. A native declaration alone cannot install code into standard bro.

See the [runnable custom-host example](../../apps/bitrouter/examples/native_regex_checker.rs):

```sh
cargo run -p bitrouter --example native_regex_checker -- bitrouter.yaml rules.yaml
```

Set `server.listen: 127.0.0.1:4356` (or another free loopback port) in the
example config. It serves model HTTP requests at that address and does not start
the daemon control socket or implement the complete bro CLI lifecycle. In a
separate project, explicitly depend on `bitrouter`, `bitrouter-guardrails`,
`bitrouter-sdk`, `axum`, `tokio`, `serde-saphyr` and `anyhow` as used by the example.
The normal bro dependency graph excludes the matcher; example/test dependencies
are development-only.

Both modes share projection, frozen bindings, concurrency admission, deadlines,
result validation, fail-closed execution and receipts. Native code runs in the
blocking pool, is trusted process-local code, and is not a sandbox. Timeout or
cancellation stops waiting; started CPU work retains its concurrency permit until
completion. Neither mode inspects generated output or provides redaction.

## Legacy SDK compatibility

The matcher's optional `sdk` feature retains `GuardrailsPlugin` and old
input/output hook APIs. Those hooks have global/per-request rule deposits and
stream block/redact semantics. They are distinct from the new router-bound native
checker and do not inherit its receipts. New native request checks do not require
this feature.

## Verification

```sh
cargo check -p bitrouter-guardrails --no-default-features
cargo test -p bitrouter-guardrails --all-features
cargo test -p bitrouter-regex-checker
cargo test -p bitrouter --test request_checks
```

The old matcher package stays named `bitrouter-guardrails`. The newly introduced
service package, executable and archives use `bitrouter-regex-checker`; this is a
WIP naming change, not a rename of the legacy SDK plugin or its configuration key.
HTTP v1 and existing HTTP checker configuration remain compatible.
