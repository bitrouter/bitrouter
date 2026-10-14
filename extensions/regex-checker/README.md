# Regex checker extension

`request-check` is the capability; `regex-checker` is one implementation.
Configured rules decide its purpose: restricted text, secret-like patterns, or
selected personal identifiers. This is not a comprehensive PII detector.

The `matcher/` directory contains the `bitrouter-guardrails` Cargo package. Its
existing package name is retained for compatibility. Beta extensions compile into
a custom Rust host; there is no standalone checker service or remote extension
protocol. The official `bro` binary does not link the regex implementation.

## Register in a custom Rust host

Link `bitrouter-guardrails` with default features. Extension modules register
`checker::callback(rules)` through the application's restricted `ExtensionApi`:

```rust
use bitrouter::gateway::extensions::registry::ExtensionApi;
use bitrouter_guardrails::{checker, rules::RuleSet};

fn register(api: &mut ExtensionApi, rules: RuleSet) -> anyhow::Result<()> {
    api.request_check("secret-check", "secret-rules-v1", checker::callback(rules))?;
    Ok(())
}
```

The custom host calls this function inside
`bitrouter::host::serve_with_extensions(&source, |api| register(api, rules))`
and awaits shutdown. `source` is an existing `bitrouter::paths::ConfigSource`.
This entry owns the process working directory, tracing subscriber and signal
handlers. Run one host per process and let it initialize tracing.
The low-level `assemble::build_app_with_extensions` remains available for embedding.
One module can register multiple instances with distinct IDs;
registration collects callbacks and does not run checks or install global hooks.
The API does not expose the full builder, pipeline context, credentials, host
diagnostic state or migrations. The configuration is explicit:

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

The registration id is the instance id used by configuration and router bindings.
Its revision must match configuration and must change when code or rules change.
This is a declared identity, not cryptographic attestation. Duplicate IDs and
invalid IDs/revisions fail registration; registration errors prevent activation.
Missing or mismatched registrations for configured instances fail activation,
including instances without router bindings. Valid registrations absent from
configuration remain inactive with a sorted startup diagnostic and allocate no
execution slots. A configured registration without a router binding remains
available but is never invoked.
A native declaration alone cannot install code into standard `bro`: adding or
upgrading extension code requires rebuilding the custom host.

All author input, fragment, coverage and decision types live in
`bitrouter_ai::extension::request_check`. The callback receives `Input` and returns
`Decision::Allow` or `Decision::Deny { reason_code }`. Input contains bounded entry
text and coverage information, without an HTTP envelope. Fragment order and
newline boundaries are preserved by the regex matcher. Denials return a fixed
reason code; rule names and matched text are not exposed in the decision.

Use a strict input rule document with the
[runnable custom-host example](../../apps/bitrouter/examples/native_regex_checker.rs):

```yaml
scope: input
rules:
  - name: credential
    pattern: 'token-[0-9]+'
    action: block
```

```sh
cargo run -p bitrouter --example native_regex_checker -- bitrouter.yaml rules.yaml
```

Set `server.listen: 127.0.0.1:4356` (or another free loopback port) in the
example config. The shared host serves inference, the local daemon control socket,
and the optional authenticated remote control listener. It reuses configuration
baselines, reload, signals and shutdown from `bro serve`.
In a
separate project, explicitly depend on `bitrouter`, `bitrouter-guardrails`,
`tokio`, `serde-saphyr` and `anyhow` as used by the example.
The normal `bro` dependency graph excludes the matcher; example/test dependencies
are development-only.

This is a foreground host, not the complete `bro` CLI. `bro --config
bitrouter.yaml status` and `stop` can target its management socket. Restart by
invoking this same custom executable or its service manager.
Do not use official `bro restart` to replace it: that binary has no registered
extension, and the example does not implement the `serve` subcommand expected by
the default background launcher. Changing bindings or startup-loaded rules
requires restarting the custom host.

The host owns projection, frozen bindings, concurrency admission, deadlines,
result validation and fail-closed execution. Extension code runs in the
blocking pool as trusted process-local code; it is not sandboxed. Timeout or
cancellation stops waiting, but started work retains its concurrency permit until
completion. The request-check capability does not inspect generated output or
provide redaction. Extensions can call external services themselves, but the host
does not provide a remote extension transport.

## Legacy host adapters

Global input hooks and stream block/redact hooks now live in
`bitrouter::gateway::extensions::guardrails`, behind the application's
`guardrails` feature. Import `plugin::GuardrailsPlugin` or individual `hooks`
from that module. The matcher no longer exposes an `sdk` feature or depends on
the server framework; configuration and rule types use their owning `config`
and `rules` modules.

A custom host explicitly installs `GuardrailsPlugin::with_static(rules)` for a
global rule set, or `GuardrailsPlugin::dynamic()` after its request-specific rule
resolver. Enabling the Cargo feature alone installs no hooks and activates no
checker. These adapters preserve global/per-request deposits and streamed
block/redact behavior; a router-bound input check does not replace them.

The host registration API is
`bitrouter::gateway::extensions::registry::ExtensionApi`. Callback inputs and decisions have moved to
`bitrouter_ai::extension::request_check`; use those contracts directly. HTTP
checker declarations remain unsupported. See the
[service migration plan](../../docs/BITROUTER_SERVICE_REFACTOR_PLAN.md).

## Verification

```sh
cargo check -p bitrouter-guardrails --no-default-features
cargo test -p bitrouter-guardrails --all-features
cargo test -p bitrouter --features guardrails gateway::extensions::guardrails
cargo test -p bitrouter --test request_checks
```
