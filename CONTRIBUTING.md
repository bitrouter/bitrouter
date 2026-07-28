# Contributing to BitRouter

Thanks for your interest in contributing to BitRouter.

This guide covers how to report bugs, request features, submit pull requests, and update the provider registry under [`registry/providers`](registry/providers).

## Before You Start

- Read the project introduction in [`README.md`](README.md)
- Read the workspace architecture guide in [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)
- Check existing issues and pull requests before opening a new one

## Reporting Bugs

Please use the bug report issue template so the title starts with `[BUG]`.

Include:

- a clear summary of the problem
- steps to reproduce it
- the expected behavior
- the actual behavior
- your environment details (OS, Rust version, provider/config context when relevant)
- logs, request payloads, or screenshots if they help explain the problem

If the bug is related to configuration or routing, include a minimal redacted `bitrouter.yaml` example when possible.

## Requesting Features

Please use the feature request issue template so the title starts with `[FEATURE]`.

Describe:

- the use case you are trying to solve
- why the current behavior is insufficient
- the expected UX or API shape
- any provider-specific constraints or compatibility requirements

For larger changes, opening an issue before writing code is the fastest way to align on approach.

## Submitting Pull Requests

1. Fork the repository and create a focused branch.
2. Keep the change scoped to a single bug fix, feature, or documentation improvement.
3. Add or update tests when behavior changes.
4. Update docs when user-facing behavior, config, or provider support changes.
5. Run the required validation commands locally before opening the PR:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace --all-features`
6. Open a pull request with a clear summary and note any follow-up work.

The workspace MSRV is **Rust 1.93**; the `msrv` CI job pins that exact toolchain. Don't rely on a feature stabilised after it.

## Review Guidelines

We prefer contributions that are:

- small and easy to review
- consistent with existing naming and crate boundaries
- validated with the existing Rust tooling
- accompanied by docs updates when public behavior changes

## Updating Provider Support

Public providers are defined as YAML files under [`registry/providers`](registry/providers) and generated into `dist/registry/`; BitRouter fetches that registry at runtime and caches it locally. The only compiled-in provider entry under `crates/bitrouter-providers/providers/` is the official hosted gateway, which carries the local zero-config auth/transport defaults. Each registry provider declares how to talk to that upstream: its `api_base`, `api_protocol` data, `auth` scheme (bearer / header / oauth / native), billing class, and served models.

### Updating an existing provider

1. Edit the matching YAML file under [`registry/providers/`](registry/providers/) (for example, `openai.yaml` or `anthropic.yaml`).
2. Update fields such as `api_base`, `api_protocol`, `auth`, billing, pricing, or served model mappings.
3. Regenerate and verify the generated registry artifacts: `cargo run -p dist-helper -- registry build && cargo run -p dist-helper -- check`.
4. Update docs (in `bitrouter-docs`) if the public provider list or onboarding guidance changes — the `supported-*` tables regenerate on the docs site from the committed `dist/registry`.

### Adding a new provider

If the provider uses an already-supported wire protocol (Chat Completions, Responses, Messages, Generate Content):

1. Add a provider definition under [`registry/providers/`](registry/providers/) as `<id>.yaml` (the `name` field must match the stem). Providers are fetched from the registry at runtime, not compiled into the binary — only the `bitrouter` cloud gateway is compiled in.
2. `bearer` / `header` auth needs no Rust. For a regional or per-account base URL, use `${VAR}` in `api_base` (resolved from the environment at merge time, e.g. `${AWS_REGION}`); an unset var with no `:-default` drops the provider from routing.
3. For a model catalog, add `auto_sync: { feed: models_dev, key: <models.dev slug> }` and leave `models: []` — the sync fills pricing. Then regenerate the dist: `cargo run -p dist-helper -- registry sync --write && cargo run -p dist-helper -- registry build` (the docs site's `supported-*` tables regenerate from the committed `dist/registry`).
4. For stateful auth (OAuth, token-exchange), add an `AuthApplier` in `crates/bitrouter-providers/` keyed by the `auth.handler` name and register it in `apps/bitrouter/src/assemble.rs::build_auth_appliers` (see `copilot`).
5. Add or update tests, and update user-facing docs + the `/bitrouter` skill when the provider list or env vars change.

If the provider needs a wire that isn't HTTP+JSON+SSE (a vendor SDK owning a binary framing) — rare, and no current registry provider needs it — see the `ApiProtocol::Custom` escape hatch in [`crates/bitrouter-sdk/src/language_model/protocol/mod.rs`](crates/bitrouter-sdk/src/language_model/protocol/mod.rs): add an `OutboundAdapter` + `Transport` in a standalone crate and register it on the dispatch executor. See [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) for where each layer lives.

## Questions and Discussion

If you want feedback before implementing a larger change, open an issue or join the community on [Discord](https://discord.gg/G3zVrZDa5C).
