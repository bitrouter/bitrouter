# Contributing to BitRouter

Thanks for your interest in contributing to BitRouter.

This guide covers how to report bugs, request features, submit pull requests, and update the built-in provider registry under [`bitrouter-config/providers`](bitrouter-config/providers).

## Before You Start

- Read the project introduction in [`README.md`](README.md)
- Read the workspace architecture guide in [`DEVELOPMENT.md`](DEVELOPMENT.md)
- Check existing issues and pull requests before opening a new one

## Reporting Bugs

Please open an issue with:

- a clear summary of the problem
- steps to reproduce it
- the expected behavior
- the actual behavior
- your environment details (OS, Rust version, provider/config context when relevant)
- logs, request payloads, or screenshots if they help explain the problem

If the bug is related to configuration or routing, include a minimal redacted `bitrouter.yaml` example when possible.

## Requesting Features

When proposing a feature, describe:

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
   - `cargo fmt -- --check`
   - `cargo clippy`
   - `cargo nextest run`
6. Open a pull request with a clear summary and note any follow-up work.

## Review Guidelines

We prefer contributions that are:

- small and easy to review
- consistent with existing naming and crate boundaries
- validated with the existing Rust tooling
- accompanied by docs updates when public behavior changes

## Release Process

BitRouter releases are prepared locally and published from GitHub Actions after the release PR lands on `main`.

1. Run `cargo-release` locally to bump the workspace version and create the release commit.
2. Regenerate `CHANGELOG.md` with `git-cliff -o CHANGELOG.md` and include it in the release PR.
3. Open the release PR and rebase it onto `main` before merging.
4. After the release commit reaches `main`, `.github/workflows/release.yml` recreates the `bitrouter-v<version>` tag on the merged `main` commit, builds the `bitrouter` binary for Linux, macOS, and Windows, and publishes a GitHub Release using `git-cliff` notes.

The GitHub Release workflow expects the merged commit subject to be `chore(release): publish v<version>`, with an optional GitHub-appended PR suffix like `(#33)`.

## Updating Built-In Provider Support

Built-in providers are defined in YAML files under [`bitrouter-config/providers`](bitrouter-config/providers) and loaded by `bitrouter-config/src/registry.rs`.

### Updating models or defaults for an existing built-in provider

1. Edit the matching YAML file:
   - `bitrouter-config/providers/openai.yaml`
   - `bitrouter-config/providers/anthropic.yaml`
   - `bitrouter-config/providers/google.yaml`
2. Update fields such as:
   - `api_protocol`
   - `api_base`
   - `env_prefix`
   - `models`
3. Run the config and workspace tests.
4. Update docs if the public provider list or onboarding guidance changes.

### Adding a new built-in provider definition

If the provider uses an already-supported protocol, you usually need to:

1. Add a new YAML file under `bitrouter-config/providers`.
2. Register it in `bitrouter-config/src/registry.rs`.
3. Add or update tests that cover the built-in registry and config loading behavior.
4. Update user-facing docs that mention supported providers.

If the provider introduces a new protocol or transport surface, the work is broader. In that case, plan to update:

- `bitrouter-config` for config schema and registry wiring
- `bitrouter-runtime/src/router.rs` so the runtime can instantiate the provider
- `bitrouter-api` if you need public provider-compatible endpoints
- workspace manifests and feature flags where the provider needs to be compiled conditionally
- tests and docs across the affected crates

## Questions and Discussion

If you want feedback before implementing a larger change, open an issue or join the community on [Discord](https://discord.gg/G3zVrZDa5C).
