# CLAUDE.md

This file is for AI coding agents working in the BitRouter workspace.

## Read This First

Before making changes, read these files in order:

1. [`README.md`](README.md) for the product surface, CLI behavior, and runtime path conventions
2. [`DEVELOPMENT.md`](DEVELOPMENT.md) for workspace architecture and server composition
3. [`CONTRIBUTING.md`](CONTRIBUTING.md) for contribution expectations and validation commands

## Project Summary

BitRouter is a Rust workspace for running an agent-oriented LLM gateway.

- `bitrouter` is the CLI entry point
- `bitrouter-runtime` assembles config, routing, daemon control, and the Warp server
- `bitrouter-api` provides reusable provider-compatible HTTP filters
- `bitrouter-config` loads `bitrouter.yaml`, `.env`, built-in providers, and routing definitions
- provider crates implement concrete model adapters used by the runtime router

## Agent Guidelines

### Do

- keep changes surgical and consistent with existing crate boundaries
- verify any CLI or documentation claims against the source code before editing docs
- preserve the runtime path resolution behavior described in `bitrouter-runtime/src/paths.rs`
- update related docs when changing public behavior, configuration, or provider support
- add or update tests when behavior changes

### Do not

- do not rewrite unrelated parts of the workspace for style alone
- do not change built-in provider YAML files without checking `bitrouter-config/src/registry.rs` and related tests
- do not introduce new tooling or alternate workflows when the workspace already has an established command for the job
- do not document behavior that is not implemented in source

## Validation Requirements

Before finishing, make sure these commands pass:

```bash
cargo fmt -- --check
cargo clippy
cargo nextest run
```

If `cargo-nextest` is not installed in the environment, install it before claiming validation is complete.
