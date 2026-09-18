# Regex request-check extension and guardrails migration

`bitrouter-regex-checker` is the independent HTTP executable. It checks the entry text of
explicitly bound routers and returns allow/deny. It does not inspect or redact
generated output, run inside `bro`, or cover direct model requests automatically.

Source packages are grouped under [`extensions/regex-checker/`](../extensions/regex-checker/README.md):
`matcher/` contains the reusable library and `service/` the executable. The package
name `bitrouter-guardrails` is retained for library compatibility. The new service
package, binary and archives use `bitrouter-regex-checker`. HTTP configuration
and wire v1 are unchanged.

## Build and run

The Cargo package is `bitrouter-regex-checker`; the reusable matcher remains
the `bitrouter-guardrails` library. Build only the executable with:

```sh
cargo build --release -p bitrouter-regex-checker --bin bitrouter-regex-checker
```

Create `rules.yaml`:

```yaml
scope: input
rules:
  - name: restricted-token
    pattern: '(?i)restricted-token'
    action: block
```

Run the service separately from the daemon:

```sh
bitrouter-regex-checker --rules rules.yaml --listen 127.0.0.1:8081 \
  --credential-env COMPANY_CHECKS_TOKEN
```

Set `COMPANY_CHECKS_TOKEN` in both the service and daemon environments. The flag
names an environment variable; it does not accept the token itself. Omitting
the flag disables service authentication. The default listener is loopback
`127.0.0.1:8081`; remote deployments need suitable TLS and access controls.
The endpoint is `POST /check`; `--help` and `--version` describe the executable.
The service reads and compiles rules once at startup. Restart it to change rules.
Invalid regex, unknown fields, unsupported scope, and any action other than
`block` fail startup; the service never silently drops a rule.

## Bind a router

Add this to a daemon configuration with an existing `coding` policy and
`coding-base` selector:

```yaml
checkers:
  company-input:
    endpoint: http://127.0.0.1:8081/check
    credential_env: COMPANY_CHECKS_TOKEN
    contract_version: 1
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: coding-base
    checks:
      request:
        - checker: company-input
          timeout_ms: 500
          max_input_bytes: 262144
```

Restart the daemon after changing bindings. Call `model: bitrouter/coding` to use
this router; physical model requests do not inherit its checks. Check with:

```sh
bro config validate
bro checks
bro checks probe company-input
bro checks receipts
bro checks receipt REQUEST_ID
```

A probe uses synthetic text and is not actual model-request evidence. Request
receipts belong to the current daemon process and remain available without an
exporter; restart clears them. A binding digest identifies local configuration,
not the service's rule file. A reported implementation version is not a rule hash.

## Migrate the former built-in plugin

The default host rejects **any presence** of `plugins.bitrouter-guardrails`,
including `null`, an empty map, and coexistence with new checker bindings.
This prevents a formerly protected deployment from silently starting with its
old rules ignored. The diagnostic does not load the matcher.

Before removing that key:

1. Review every protected entry point. The former plugin was globally installed;
   a checker bound to `coding` covers only requests entering that router.
2. Review output requirements. Former `block` rules also participated in stream
   checks; former `redact` rules affected stream chunks. Neither has an output
   equivalent in this service. Keep the prior deployment if that coverage is
   required until a suitable replacement is available.
3. Convert accepted **input-only** rules to the explicit `rules` schema, then run
   the service and bind each intended router. Do not copy old configuration
   wholesale or change `redact` to `block` as an automatic migration.
4. Validate allow/deny and service failure against the real daemon and inspect
   its receipts. Remove the old key only after reviewing coverage, then restart.

Deleting the old key is not proof of equivalent protection: the host cannot
infer output needs or client paths omitted from configuration. Media bytes,
later tool turns, nested calls, and harness activity outside the checked entry
are outside this contract. See [the design spec](ROUTER_EXTENSION_SPEC.md) for
the boundary and acceptance criteria.

## Development and verification

The SDK host runner owns transport progress and receipts. Service authors consume
the lightweight `bitrouter-checker-protocol` contract and return a decision;
they do not receive a host reporter or mutable pipeline context. The standalone
service depends on the matcher without its SDK integration feature. Custom
trusted hosts may still explicitly enable the library's legacy hook integration.

`Plugin`, `AppBuilder::plugin`, and `GuardrailsPlugin` are legacy custom-host
assembly APIs, retained in the current alpha SDK compatibility window. They keep
their existing hook and migration behavior. The earlier `NativeChecker` /
`build_app_with_checkers` entry remains a compatibility wrapper using the same
host assembly and request-check runtime. Removal of these APIs requires an explicitly
announced breaking SDK release with migration notes; there is no scheduled date.
New request-check extensions use `ExtensionApi`. Its restricted surface grants
no migrations, global hooks, credentials or mutable pipeline context. Existing
`PluginId` metadata, `Config::plugins`, and agent-plugin manifests are not renamed.

| Existing protection or responsibility | New request-check capability | Migration consequence |
| --- | --- | --- |
| Entry text blocked by regex | Supported for explicitly bound routers | Review text scope, rules and bindings |
| Process-global input hook, including direct model requests | No implicit global coverage | Keep compatible host behavior for uncovered entry points |
| Per-request rule deposits through Context `extensions` | No automatic translation | Custom host owns any retained legacy deposit logic |
| Generated output block / redact | Unsupported | Retain a compatible deployment until replacement exists |
| Legacy hook diagnostics | Separate from request-check receipts | Do not infer checks or coverage from new receipts |
| Plugin migrations | Remain host assembly responsibility | Do not register migrations through `ExtensionApi` |

Use the reproducible process-level acceptance harness after building `bro` and
the checker. It starts isolated daemons, two real checker processes and a counted
local mock upstream; it never calls a real model provider:

```sh
python3 tools/guardrails_e2e.py --bro target/debug/bro \
  --checker target/debug/bitrouter-regex-checker --output /tmp/guardrails-e2e
```

Async timeout stops the host waiting; it does not prove the remote process or
CPU work stopped. Limits and lifecycle semantics must remain explicit when
adding a different checker implementation. No new selector algorithm, generic
extension registry, process supervisor, or dynamic library loader is provided.

## Distribution boundary

The release plan produces separate service archives; the default `bro` archives
and installers contain no checker executable or matcher dependency. Both may be
published under the same release tag without sharing an installation. The service
has no implicit auto-start behavior.

Local build and packaging results do not prove that a release has been published.
The implementation acceptance report records available local artifacts and the
remaining external release gates separately.

Packaging uses [`precise-builds`](https://axodotdev.github.io/cargo-dist/book/reference/config.html#precise-builds)
so each application is built separately, avoiding workspace feature unification.
See [local acceptance evidence](GUARDRAILS_EXTENSION_ACCEPTANCE.md) for tested scope.

## Native request checks in a custom host

The same `bitrouter_guardrails::checker::callback` is registered through
`ExtensionApi::request_check(id, revision, callback)` in an ordinary registration
function. A custom host supplies that function to
`assemble::build_app_with_extensions(config, config_path, register)`. This is the
recommended Rust author entry; see the
[extension guide and runnable example](../extensions/regex-checker/README.md).
Registration collects implementations without enabling global hooks. Duplicate
IDs, invalid IDs/revisions and registration errors block assembly; configuration,
revision and router bindings must also pass activation checks.
Native configuration uses `checkers.<id>.native.revision`; router bindings remain
`checks.request`. No HTTP endpoint or credential is accepted on a native entry.
Default bro rejects native declarations during activation because it registers no
native implementations. Configuration validation alone validates their shape,
not the availability of a custom host's compiled code.

Management inventory identifies `execution: native` and its revision; there is
no endpoint fingerprint. A successful native probe reports the synthetic
allow/deny decision, with network reachability `not_attempted` and wire protocol
`not_checked`. It never claims an HTTP service is reachable. Real request usage
and probes remain separate. Receipt dispatch `attempted` means native work was
submitted; `response_received` means the callback returned. For HTTP these retain
their existing network meanings.

Native callbacks run under the same per-instance 32-slot admission bound. Timeout
or cancellation cannot kill synchronous Rust computation; a started callback
retains its slot until it returns. Native code shares the host's trust and failure
domain. The input scope, allow/deny decision and zero-upstream-on-failure guarantee
are common to both delivery modes; process isolation is not.
