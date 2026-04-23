# bitrouter-api

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Reusable HTTP API surface for BitRouter.

This crate provides Warp filters and shared API helpers for serving
provider-compatible endpoints. It focuses on HTTP request handling and delegates
model resolution and execution to the routing contracts from `bitrouter-core`.

## Includes

- OpenAI-compatible routes
- Anthropic-compatible routes
- Google-compatible routes
- MCP-compatible routes
- Shared API error and utility helpers

## Feature flags

Each feature exists to pull a distinct dependency tree — pure module
toggles are not features. The provider-compatible HTTP surfaces
(OpenAI, Anthropic, Google, MCP) are always available without any
feature.

Optional companion-crate facades:

- `accounts` — re-export [`bitrouter-accounts`] (account/session/key
  Warp filter builders and services). Pulls `sea-orm`.
- `observe` — re-export [`bitrouter-observe`] (`ObserveStack`, spend
  store, metrics). Pulls `sea-orm`.
- `guardrails` — re-export [`bitrouter-guardrails`] (`Guardrail`,
  `GuardedRouter`).

Payment middleware:

- `payments-tempo` — Tempo-chain MPP server payment integration.
- `payments-solana` — Solana-chain MPP server payment integration.

[`bitrouter-accounts`]: https://crates.io/crates/bitrouter-accounts
[`bitrouter-observe`]: https://crates.io/crates/bitrouter-observe
[`bitrouter-guardrails`]: https://crates.io/crates/bitrouter-guardrails
