# BitRouter guardrails service

`bitrouter-guardrails` is an independent HTTP request-checker v1 service. It
loads immutable input-block rules before binding its listener and exposes
`POST /check` for a BitRouter checker binding.

```yaml
scope: input
rules:
  - name: organization-secret
    pattern: 'secret-[0-9]+'
    action: block
```

Run it with:

```console
bitrouter-guardrails --rules ./guardrails.yaml
```

The complete CLI is `--listen <ADDR>` (default `127.0.0.1:8081`), required
`--rules <PATH>`, and optional `--credential-env <NAME>`. When a credential
environment variable is configured, clients must send its value as a bearer
token. `--help` and `--version` are also supported.

Rules are strict: `scope: input` and each `action: block` must be explicit.
Unknown fields, invalid regular expressions, `redact`, output scopes, missing
credentials, and empty rules prevent startup. Rules do not reload while the
process runs.

The matcher flattens received text fragments in order and appends one newline
after each fragment, including the last. A match across fragments must account
for that newline. The host projection covers system/message text, reasoning,
tool-call arguments, textual or JSON-rendered tool results, and approval
responses. It excludes media and file bytes, source metadata, later model
output, tool-loop activity after the entry request, and nested requests.

This input checker is not equivalent to the legacy in-process guardrail plugin:
it does not redact, inspect streamed output, or apply globally to direct-model
requests. Operators must review router bindings and any output or global
coverage requirements before removing the legacy configuration.

Matching runs on a blocking worker pool with at most 32 concurrent callbacks.
An HTTP caller timing out does not forcibly cancel synchronous regex work that
has already started.
