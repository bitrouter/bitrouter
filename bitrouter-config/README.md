# bitrouter-config

GitHub repository: [bitrouter/bitrouter](https://github.com/bitrouter/bitrouter)

Configuration and routing-table crate for BitRouter.

This crate loads BitRouter YAML configuration, resolves environment-variable
substitutions, exposes builtin provider definitions, and builds the
config-backed routing table used by the runtime.

## Includes

- Config schema and loading logic in `config`
- Environment expansion in `env`
- Builtin provider registry in `registry`
- Routing resolution in `routing`
