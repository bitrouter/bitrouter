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
