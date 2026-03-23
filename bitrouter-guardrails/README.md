# bitrouter-guardrails

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Local firewall for AI agent traffic at the proxy layer.

This crate provides a guardrail engine that inspects content flowing through
BitRouter and enforces configurable rules. It wraps any `LanguageModelRouter`
transparently via `GuardedRouter`, so the rest of the stack remains unaware of
the filtering layer.

## Includes

- Pattern-based content inspection engine in `engine`
- Built-in patterns for API keys, private keys, credentials, PII, and suspicious commands in `pattern`
- Configurable per-pattern actions (warn, redact, block) in `rule`
- User-defined custom patterns with regex and direction control in `config`
- `GuardedRouter` wrapper that applies guardrails to every routed model in `router`

## Configuration

Guardrail rules are configured under the `guardrails` key in `bitrouter.yaml`:

- `upgoing` applies to outbound traffic (`user/tool -> model`)
- `downgoing` applies to inbound traffic (`model -> user/tool`)
- actions can be `warn`, `redact`, or `block`
- `disabled_patterns` turns off built-in detectors you do not want
- `custom_patterns` adds your own regex-based firewall rules

```yaml
guardrails:
  enabled: true
  disabled_patterns:
    - pii_phone_numbers
  custom_patterns:
    - name: internal_ticket
      regex: "INC-[0-9]{6}"
      direction: both
  upgoing:
    api_keys: redact
    private_keys: block
  downgoing:
    suspicious_commands: block
  custom_downgoing:
    internal_ticket: warn
```

This lets you quickly define a local firewall policy for secrets, credentials,
PII, or custom patterns without changing any application code.
