# bitrouter-observe

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Per-request observability for BitRouter.

This crate provides spend tracking, metrics collection, and request observation
for the BitRouter LLM routing system. All observability flows through the
`ObserveCallback` trait defined in `bitrouter-core`, which fires from API
handlers with full request context (route, provider, model, account, latency,
usage/error).

## Includes

- `SpendObserver` for per-request cost calculation and spend log persistence
- `MetricsCollector` for in-memory per-route and per-endpoint metrics aggregation
- `CompositeObserver` for fanning out events to multiple callbacks
- `InMemorySpendStore` and `SeaOrmSpendStore` for pluggable spend persistence
- `spend_logs` database migration with account and timestamp indexing
- Cost calculation with granular token bucket support
