# Copilot Instructions for BitRouter

## Build, test, and lint

- Primary workspace validation:
  - `cargo fmt -- --check`
  - `cargo clippy --workspace --all-targets --all-features`
  - `cargo test --workspace`
- CI also runs `cargo nextest run --all-features` when `nextest` is available.
- Common build commands:
  - `cargo build --workspace`
  - `cargo build -p bitrouter --all-features`
- Feature-gated builds matter in this repo. CI explicitly checks `--no-default-features` and `--all-features` combinations for `bitrouter`, `bitrouter-api`, and `bitrouter-config`, so avoid assuming default features are always present.
- To run a single test, target the crate and use the fully qualified test name. Example:
  - `cargo test -p bitrouter-config registry::tests::builtin_registry_contains_core_providers -- --exact`

## High-level architecture

- This workspace is intentionally layered bottom-up:
  - `bitrouter-core` defines transport-neutral traits and shared types (`LanguageModel`, `RoutingTable`, `LanguageModelRouter`, observability contracts, errors).
  - `bitrouter-providers` implements provider adapters against those traits.
  - `bitrouter-config` loads YAML + `.env`, applies `${VAR}` substitution, merges compile-time embedded built-in providers, resolves provider `derives`, and builds `ConfigRoutingTable`.
  - `bitrouter-api` exposes reusable Warp filters for provider-compatible HTTP surfaces plus admin/discovery/protocol routes.
  - `bitrouter-accounts`, `bitrouter-mcp`, `bitrouter-a2a`, `bitrouter-skills`, `bitrouter-guardrails`, and `bitrouter-observe` add account storage, protocol gateways, skills, firewalling, and spend/metrics tracking.
  - `bitrouter` is the binary that assembles everything.

- The runtime path is:
  1. `bitrouter` resolves the home/config/env/run/log paths.
  2. `AppRuntime::load` reads config and wraps `ConfigRoutingTable` in `DynamicRoutingTable`.
  3. `runtime::Router` turns a resolved provider target into a concrete provider-backed model instance.
  4. `runtime::ServerPlan` composes Warp filters from `bitrouter-api`, wraps the router with guardrails, and attaches observers for spend + metrics.
  5. Requests flow through a `RoutingTable` to resolve the target model, then through a `LanguageModelRouter` to instantiate the provider adapter.

- The server is broader than just model proxying. `ServerPlan` also wires:
  - OpenAI, Anthropic, Google model endpoints
  - model and route discovery endpoints
  - admin route management
  - MCP gateway + MCP bridge routes
  - A2A gateway + agent discovery/admin routes
  - unified tools discovery (`GET /v1/tools`) that can combine MCP tools and config-defined skills
  - DB-backed skills, account, and session routes when a database is configured

- Auth and persistence are assembled at the runtime layer, not deep inside every crate:
  - `bitrouter-accounts` owns entities, migrations, and route builders, but callers supply the auth filter that extracts an `Identity`.
  - In `runtime/server.rs`, having a DB connection enables JWT auth plus account/session/skills routes; without a DB, model routes still serve but auth is disabled and spend storage falls back to memory.

## Key conventions

- Preserve crate boundaries. This repo is organized so crates depend downward only; avoid introducing sideways or upward dependencies between workspace layers.

- Keep `bitrouter-api` reusable. API filters should continue to depend on `bitrouter-core` traits rather than concrete config/runtime types.

- Dynamic model routes are ephemeral and take precedence over config-defined routes. They live in `DynamicRoutingTable`, and config hot-reload swaps only the inner config-backed table so dynamic routes survive reloads but not process restarts.

- Built-in provider definitions live in `bitrouter-config/providers/*.yaml` and are embedded at compile time through `bitrouter-config/src/registry.rs`. When changing built-in providers, update the YAML, registry wiring if needed, and relevant tests/docs together.

- Config loading semantics are important:
  - process environment overrides `.env`
  - `${VAR}` substitution happens before deserialization
  - unresolved `${VAR}` becomes an empty string
  - `env_prefix` can override provider `api_key` and `api_base`
  - provider `derives` is expected to be resolved before runtime routing starts

- Runtime paths are derived from a resolved BitRouter home. Resolution order is: `--home-dir` -> current working directory when `bitrouter.yaml` is present -> `BITROUTER_HOME` -> `~/.bitrouter`. Derived defaults are `<home>/bitrouter.yaml`, `<home>/.env`, `<home>/run`, and `<home>/logs`, with per-path CLI overrides on top.

- Routing has a deliberate fallback mode: if no `models:` map is configured and the built-in `bitrouter` provider exists, bare model names route to that default provider. Do not assume every request must map through an explicit alias.

- Feature flags are part of normal development here. `bitrouter`, `bitrouter-api`, and provider/payment paths have meaningful `no-default-features` and `all-features` builds, so feature-gated code should compile cleanly in both directions.

- Follow the repository rules from `CLAUDE.md` when editing Rust:
  - do not add `#[allow(...)]` to bypass checks
  - do not use `unwrap`, `expect`, or `panic!`
  - do not add unused abstractions or dead code
  - do not re-export items from inside an already-public module

- When generating commits or PR titles for this repo, use conventional commit format. PR titles are validated against the conventional-commit header shape.
