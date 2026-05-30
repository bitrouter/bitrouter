# Contributing to BitRouter

Thanks for your interest in contributing to BitRouter.

This guide covers how to report bugs, request features, submit pull requests, and update the built-in provider catalog under [`crates/bitrouter-providers/providers`](crates/bitrouter-providers/providers).

## Before You Start

- Read the project introduction in [`README.md`](README.md)
- Read the workspace architecture guide in [`DEVELOPMENT.md`](DEVELOPMENT.md)
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

The workspace MSRV is **Rust 1.88**; the `msrv` CI job pins that exact toolchain. Don't rely on a feature stabilised after it.

## Review Guidelines

We prefer contributions that are:

- small and easy to review
- consistent with existing naming and crate boundaries
- validated with the existing Rust tooling
- accompanied by docs updates when public behavior changes

## Updating Built-In Provider Support

Built-in providers are defined as TOML files under [`crates/bitrouter-providers/providers`](crates/bitrouter-providers/providers) — one file per provider — and embedded into the binary at compile time. Each file declares how to talk to that upstream: its `api_base`, `api_protocol` (a single protocol or a glob → protocol map for mixed-protocol gateways), and `auth` scheme (bearer / header / oauth / native). Model metadata (pricing, context length) is **not** in these files; it comes from `models.dev` at runtime.

### Updating an existing built-in provider

1. Edit the matching TOML file under `crates/bitrouter-providers/providers/` (e.g. `openai.toml`, `anthropic.toml`).
2. Update fields such as `api_base`, `api_protocol`, or `auth`.
3. Run `cargo test -p bitrouter-providers` — the catalog tests parse every embedded file and check the declared `id` matches the filename.
4. Update docs if the public provider list or onboarding guidance changes.

### Adding a new built-in provider

If the provider uses an already-supported wire protocol (Chat Completions, Responses, Messages, Generate Content):

1. Add a new TOML file under `crates/bitrouter-providers/providers/`. The filename stem **must** equal the `id` field inside.
2. Register it in the `EMBEDDED` array in [`crates/bitrouter-providers/src/builtin.rs`](crates/bitrouter-providers/src/builtin.rs) and bump the count assertion in that file's tests.
3. If the provider should auto-enable in zero-config mode, confirm its `auth` scheme advertises an env var (`bearer` / `header`); OAuth-only providers stay opt-in.
4. Add or update tests covering the new entry.
5. Update user-facing docs that mention supported providers.

If the provider needs a **new** wire protocol or transport, the work is broader — plan to add a protocol adapter under `crates/bitrouter-sdk/src/language_model/protocol/` (or an outbound-only provider crate like [`crates/bitrouter-bedrock`](crates/bitrouter-bedrock)), wire it into the dispatch executor, and cover it in the protocol-conversion test matrix. See [`DEVELOPMENT.md`](DEVELOPMENT.md) for where each layer lives.

## Questions and Discussion

If you want feedback before implementing a larger change, open an issue or join the community on [Discord](https://discord.gg/G3zVrZDa5C).
