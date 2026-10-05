# Compiled request-check extensions

Beta extensions are Rust code linked into a custom host and registered at startup.
The default `bro` does not include the regex matcher and cannot install code from
configuration. The HTTP checker executable and protocol are no longer supported.
Do not launch the former `bitrouter-regex-checker` service as a migration path.

`regex-checker` is an extension implementing the request-check capability. Its
Cargo library remains `bitrouter-guardrails`. It matches supplied regex rules;
it is not a comprehensive PII detector. The common entry is
`bitrouter_sdk::extension::ExtensionApi::request_check(id, revision, callback)`.
The foreground host calls `bitrouter::host::serve_with_extensions`; see the runnable
`apps/bitrouter/examples/native_regex_checker.rs` example in the source repository.
No separate extension API or checker-protocol crate is required.
The shared entry runs inference, the local management socket and optional remote
control with the same configuration baseline, reload and shutdown as
`bro serve`. Use `assemble::build_app_with_extensions` only when owning a separate
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
foreground process, not a complete `bro` CLI: official `bro restart` would launch
the official binary without your registrations. Use your custom invocation or
service manager to restart, and explicitly target its config/socket for `bro`
management status and `stop`. Clients select `model: bitrouter/<id>`; direct
model requests do not inherit checks. Use `bro config validate` before starting;
activation additionally checks compiled registrations.
Default bro rejects Native declarations without corresponding registrations.
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

## Breaking migration

- Replace app-local `bitrouter::extension::ExtensionApi` imports with the SDK entry.
- Replace Native map assembly with `assemble::build_app_with_extensions`.
- Replace hand-built foreground HTTP serving with `host::serve_with_extensions`
  when the full BitRouter daemon lifecycle is wanted.
- Import fragment and coverage author types from SDK `extension::request_check`;
  the former `language_model::request_checks` paths have no compatibility aliases.
- Replace former protocol callback inputs with SDK request-check business inputs.
- Former HTTP `endpoint`, `credential_env` and `contract_version` fields are rejected.
  Link and register an implementation, then explicitly migrate the declaration.
- No replacement executable is installed or launched automatically.

Any `plugins.bitrouter-guardrails` key, including null/empty or alongside a new
check, still blocks default-host activation. Review input fragment ordering,
all entry points and output requirements before removing it. Old Plugin hooks
were global and supported stream/output block/redact. They remain available for
custom hosts through the library's explicit `sdk` feature; request checks do not
replace that scope. Do not convert redact to block automatically or claim that
removing an old key proves the protection has migrated.
