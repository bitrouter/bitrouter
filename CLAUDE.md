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

## Validation Requirements

Before finishing, make sure these commands pass:

```bash
cargo fmt -- --check
cargo clippy
cargo test
```
