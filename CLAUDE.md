# Claude.md

## Project Overview

bitrouter is a modular, trait-based LLM routing system written in Rust. It can be used as:

- **A lightweight local LLM aggregator and proxy** — connect to upstream providers (OpenAI, Anthropic, Google) and expose provider-specific API types, running on your local machine.
- **A high-performance, out-of-the-box web server on the cloud** — deploy the binary for production LLM request proxying with config-driven routing, daemon management, and observability.
- **An SDK to build your own service** — import trait-based core components and API routes as library crates. Write your own implementation at any layer, or re-use service components to plug-and-play.

---

### Dependency Logic

The layering follows a strict bottom-up principle — each crate depends only on the layers below it, never sideways or upward:

1. **bitrouter-core** — The foundation. Zero knowledge of HTTP, config files, or any concrete provider. Owns transport-neutral traits (`LanguageModel`, `RoutingTable`, `LanguageModelRouter`), shared model types (prompts, messages, tool schemas, usage stats), and error types. Every other crate depends on this.
2. **Provider crates** (bitrouter-openai, bitrouter-anthropic, bitrouter-google) — Each depends only on `bitrouter-core`. They implement the `LanguageModel` trait for a specific upstream API, handling request/response conversion, streaming, and provider-specific error parsing. Providers are fully independent of each other and of any HTTP framework.
3. **bitrouter-config** — Depends on `bitrouter-core` for routing trait definitions. Owns YAML config parsing, environment variable substitution, built-in provider registry, provider inheritance (`derives`), and the `ConfigRoutingTable` implementation. No knowledge of HTTP or concrete provider SDK types.
4. **bitrouter-api** — Depends on `bitrouter-core` for traits, and optionally on individual provider crates (feature-gated) for API type serialization. Provides reusable Warp HTTP filters for each provider's API surface (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/v1beta/models/`). Filters accept any `RoutingTable + LanguageModelRouter` — they are decoupled from concrete config or provider instantiation.
5. **bitrouter-accounts** — Depends on `bitrouter-core` for server contract types. Provides account and session management backed by sea-orm: entity types (`Account`, `ApiKey`, `Session`, `Message`), database migrations via `Migrator`, `AccountService` / `SessionService` for data operations, and Warp filter builders parameterized by a caller-supplied auth filter. This crate does **not** implement authentication — callers provide a Warp filter that extracts an `Identity`, decoupling auth strategy from account logic.
6. **bitrouter** (binary) — The CLI product. Depends on `bitrouter-core`, `bitrouter-config`, `bitrouter-api`, `bitrouter-accounts`, and all provider crates. Assembles everything: resolves paths, loads config, and provides the user-facing commands (`serve`, `start`, `stop`, `status`, `restart`) and optional TUI.

---

## Key Design Decisions

### 1. Trait-Based Core with Dynamic Dispatch

`bitrouter-core` defines the `LanguageModel` trait using `#[dynosaur]` for object-safe dynamic dispatch. This means:

- Concrete provider types are erased behind `Box<DynLanguageModel>` at runtime.
- The routing and API layers never know which provider they're talking to.
- New providers are added by implementing `LanguageModel` — no changes to routing or API code.

The two routing traits (`RoutingTable` for name → target resolution, `LanguageModelRouter` for target → model instantiation) are similarly trait-based, allowing full replacement of the routing strategy.

### 2. Canonical Intermediate Representation

All providers convert to/from a shared type system in `bitrouter-core`:

- `LanguageModelCallOptions` (request)
- `LanguageModelPrompt` / `LanguageModelMessage` (conversation)
- `LanguageModelGenerateResult` (response)

This means the API layer translates once (HTTP → core types), the provider layer translates once (core types → provider API), and the two are completely independent. Adding a new API surface or a new provider is an isolated change.

### 3. Config-Driven Provider Registry with Inheritance

Built-in provider definitions (OpenAI, Anthropic, Google) are embedded at compile time from YAML files in `bitrouter-config/providers/`. User config merges on top:

- `derives: openai` lets a custom provider inherit all defaults from the built-in OpenAI definition.
- `env_prefix` auto-loads `{PREFIX}_API_KEY` and `{PREFIX}_BASE_URL` from environment variables.
- `${VAR}` substitution works in any YAML string value.

This eliminates boilerplate — a minimal config just needs an API key.

### 4. Reusable HTTP Filters (SDK Mode)

`bitrouter-api` exposes Warp filters as composable building blocks. Each filter:

- Accepts generic `Arc<dyn RoutingTable>` + `Arc<dyn LanguageModelRouter>`.
- Handles deserialization, model routing, generation, and response serialization.
- Is independently mountable — use only the OpenAI-compatible endpoint, or mix and match.

To build your own service, import `bitrouter-api` and supply your own trait implementations. You don't need `bitrouter-runtime` or `bitrouter-config` at all.

---

## Guidelines

1. **NEVER** use `#[allow(xxx)]` to bypass checks.
2. **NEVER** re-export components in a public mod. If you already have a public mod: `pub mod a;`, you never re-export components inside it: `pub use a::A; // Don't do this`.
3. **NEVER** use `.unwrap`, `.expect` or `panic!` to make the Rust program panic.
4. **NEVER** over-design types, functions and methods that is never used in the feature or fix you are working on. We don't allow dead code.

---

## Run Checks Before Submitting Code

Run these checks before submitting to users if you modified source code:

1. `cargo test --workspace`: Ensure all unit tests, integration tests and doc tests pass.
2. `cargo clippy`: Ensure you are following Rust's best practise.
3. `cargo fmt -- --check`: Ensure the source code is correctly formatted.
