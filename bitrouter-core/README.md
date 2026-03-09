# bitrouter-core

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Transport-neutral contracts and shared types for BitRouter.

This crate defines the common model traits, routing interfaces, and error types
used across the workspace. Provider crates implement these contracts, while the
config, API, and runtime crates depend on them to stay decoupled from any one
upstream provider.

## Includes

- Shared error types in `errors`
- Model abstractions in `models`
- Routing contracts in `routers`
