# Independent guardrails request checker

`bitrouter-guardrails` is a separate executable. It checks the entry text of
explicitly bound routers and returns allow/deny. It does not inspect or redact
generated output, run inside `bro`, or cover direct model requests automatically.

Source packages are grouped under [`extensions/guardrails/`](../extensions/guardrails/README.md):
`matcher/` contains the reusable library and `service/` the executable. Package
names, CLI, configuration and release artifact names are unchanged by this layout.

## Build and run

The Cargo package is `bitrouter-guardrails-service`; the reusable matcher remains
the `bitrouter-guardrails` library. Build only the executable with:

```sh
cargo build --release -p bitrouter-guardrails-service --bin bitrouter-guardrails
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
bitrouter-guardrails --rules rules.yaml --listen 127.0.0.1:8081 \
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

Use the reproducible process-level acceptance harness after building `bro` and
the checker. It starts isolated daemons, two real checker processes and a counted
local mock upstream; it never calls a real model provider:

```sh
python3 tools/guardrails_e2e.py --bro target/debug/bro \
  --checker target/debug/bitrouter-guardrails --output /tmp/guardrails-e2e
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
