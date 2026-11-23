# Compiled request-check extensions

Beta extensions are Rust code linked into a custom host and registered at startup.
The official `bitrouter` binary has no custom request-check registrations.
Configuration selects compiled callbacks; it cannot install extension code.

`regex-checker` is an extension implementing the request-check capability. Its
Cargo library remains `bitrouter-guardrails`. It matches supplied regex rules;
it is not a comprehensive PII detector. The common entry is
`bitrouter::gateway::extensions::registry::ExtensionApi::request_check(id, revision, callback)`.
The foreground host calls `bitrouter::host::serve_with_extensions`; see the runnable
`apps/bitrouter/examples/native_regex_checker.rs` example in the source repository.
No separate extension API or checker-protocol crate is required.
The shared entry runs inference, the local management socket and optional remote
control with the same configuration baseline, reload and shutdown as
`bitrouter serve`. Use `assemble::build_app_with_extensions` only when owning a separate
embedding lifecycle.
On Unix, the host keeps a small `.sock.lock` file beside its control socket to
serialize endpoint ownership. The OS lock releases when the host exits; the file
alone does not mean a daemon is running. Socket, owned PID and locator cleanup
still occurs on graceful exit and partial startup failure.

## Configure an already registered implementation

```yaml
checkers:
  company-input:
    native:
      revision: company-rules-v1
```

Register that exact id and revision in the custom host. Add this under the
intended `routers.<id>` without replacing its selection:

```yaml
checks:
  request:
    - checker: company-input
      timeout_ms: 500
      max_input_bytes: 262144
```

Restart the same custom executable after editing bindings. The example is a
foreground process, not a complete `bitrouter` CLI: official `bitrouter restart` would launch
the official binary without your registrations. Use your custom invocation or
service manager to restart, and explicitly target its config/socket for `bitrouter`
management status and `stop`. Clients select `model: bitrouter/<id>`; direct
model requests do not inherit checks. Use `bitrouter config validate` before starting;
activation additionally checks compiled registrations.
The default gateway rejects Native declarations without corresponding registrations.
Missing or mismatched configured registrations fail activation even when unbound.
Valid unconfigured registrations remain inactive with a sorted startup diagnostic;
they create no execution state. Duplicate/invalid
registration invalidates the collection even when an extension ignores its error.

There is no checker probe, inventory or receipt query. Startup diagnostics and
bounded native-invocation tracing do not include request content or matched
text. Deny, timeout, invalid results and oversized input prevent model dispatch.
An allow result does not mean upstream generation or delivery succeeded.

## Rules and protection boundary

Request checks support input block rules. For example:

```yaml
scope: input
rules:
  - name: restricted-token
    pattern: '(?i)restricted-token'
    action: block
```

The custom host loads and validates its rules and passes the matcher callback
into registration. The example demonstrates that assembly. Changing linked code
requires rebuilding the host; changing startup-loaded rules requires restarting
and using a revision that identifies the intended code/rules combination.

Media bytes, generated output, later tool turns, nested calls and harness activity
outside the router entry are not inspected. Revision and binding digests are
identifiers, not attestations. Callbacks are trusted process code. A deadline
stops waiting, but cannot kill a started synchronous callback; its concurrency
slot stays held until it ends. Extensions may call external services internally,
but BitRouter does not supply a remote extension protocol.

## Callback and global-hook ownership

Import callback, fragment and coverage types from
`bitrouter::gateway::extensions::request_check`. They belong to the gateway
application; AI contains shared wire DTOs and the SDK contains HTTP clients.

Global stream/output block and redact hooks use the application adapter in
`bitrouter::gateway::extensions::guardrails`, enabled by the application's
`guardrails` feature. A custom host explicitly installs `plugin::GuardrailsPlugin`
or individual `hooks`. A Cargo feature makes the adapter available but does not
activate protection. Input request checks and global output hooks have different
coverage; configure and verify the intended scope.

The matcher library owns `config` and `rules` without a gateway or SDK dependency.
The gateway adapter converts matcher results into callback values and pipeline
hooks. The official gateway rejects `plugins.bitrouter-guardrails` configuration;
use a custom host with explicit assembly for global hooks.
