# bitrouter-runtime

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Runtime assembly crate for BitRouter.

This crate connects configuration, routing, provider adapters, and the HTTP
server into a runnable application. It also owns daemon lifecycle management,
runtime paths, and the concrete model router used by the CLI binary.

## Includes

- `AppRuntime` for loading and serving a configured runtime
- `Router` for instantiating provider-backed language models
- Server, control, daemon, and runtime path modules
