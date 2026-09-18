# Independent input checks

The default `bro` no longer includes the guardrails matcher. The separate
`bitrouter-guardrails` executable implements HTTP request-check v1 on `POST /check`.
Obtain the service archive separately from the matching BitRouter release; it is
not installed or started by the default bro installer. Do not assume an
unreleased source change is already available as a downloadable release.

Rules must explicitly declare input scope and block actions:

```yaml
scope: input
rules:
  - name: restricted-token
    pattern: '(?i)restricted-token'
    action: block
```

Save as `rules.yaml`, set a token in the service environment, and run:

```sh
bitrouter-guardrails --rules rules.yaml --listen 127.0.0.1:8081 \
  --credential-env COMPANY_CHECKS_TOKEN
```

`--rules` is required. `--listen` defaults to `127.0.0.1:8081`; omitting
`--credential-env` disables service authentication. That flag takes an environment
variable name, not a token. Keep it on loopback unless the deployment supplies
appropriate TLS and access controls. `--help` and `--version` are supported.
Rules are fixed at startup; invalid regex, unknown fields, empty rules, output
scope, and `redact` fail startup.

In the daemon config, add a checker and bind it to an existing router:

```yaml
checkers:
  company-input:
    endpoint: http://127.0.0.1:8081/check
    credential_env: COMPANY_CHECKS_TOKEN
    contract_version: 1
```

Add this under the intended `routers.<id>` without replacing its selection:

```yaml
checks:
  request:
    - checker: company-input
      timeout_ms: 500
      max_input_bytes: 262144
```

The daemon must have the same token in its own environment. Restart after
editing bindings. Clients select `model: bitrouter/<id>`; direct model requests
do not inherit those checks. Use `bro config validate`, `bro checks`,
`bro checks probe company-input`, and `bro checks receipt REQUEST_ID`.
Probe success is synthetic evidence; receipts show actual use and last only
within the current daemon process. Deny, timeout, malformed response, and
unavailable service prevent model dispatch for checked requests.

## Legacy migration must be reviewed

Any `plugins.bitrouter-guardrails` key, including null/empty or alongside the
new checker, blocks host activation. Before removing it, review input fragment
ordering (each received text fragment ends with a newline), all client entry
points, and output protection requirements. The old plugin was global and its
block/redact rules also affected streamed output. This input-only service does
not replace that coverage. Keep the prior deployment if such protection is still
required; deleting the key alone does not prove migration is complete.

Do not convert redact to block automatically. Media bytes, later tool turns,
nested calls, generated output, and harness activity outside the router entry
are not inspected. Binding digests do not attest service rules or code.
