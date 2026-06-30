---
title: Build a plugin
description: Write a BitRouter plugin against the trait-based Rust SDK — implement a hook, package it as a Plugin, and install it into the router.
sourceHash: 5c4baeccdc37711fd6c7521c89d26b855671e21d2e7b552c436f0ade975c888e
---

# Build a plugin

BitRouter is a programmable router for LLM API traffic. Inbound requests on any
supported wire protocol are normalised into a canonical pipeline, run through an
ordered chain of **hooks**, dispatched to an upstream provider, and rendered back
in the inbound protocol. A **plugin** is the unit that packages one or more hooks
(plus any database migrations) and installs them into the router in a single call.

Everything here is verified against the `bitrouter-sdk` crate and the three
plugins that ship in the [core repository](https://github.com/bitrouterai/bitrouter):
`bitrouter-guardrails`, `bitrouter-observe`, and `bitrouter-attestation`.

<Callout type="info">
Plugins are a **build-time, Rust** extension mechanism — you compile them into a
router binary against the SDK. There is no dynamic plugin loading or scripting
surface. If you only need to configure providers, routing, or guardrail rules,
use [`bitrouter.yaml`](/docs/guides/self-host) instead — you do not need to write
a plugin.
</Callout>

## The pipeline a plugin hooks into

The SDK exposes three independent protocol pipelines, each with its **own** hook
traits (they are deliberately not generic over a shared trait, so a stage meant
for one pipeline can't be registered on another):

- **`language_model`** — the main LLM pipeline, with the full hook set.
- **`mcp`** — Model Context Protocol routing (pure routing, no settlement).
- **`acp`** — Agent Client Protocol routing (pure routing, no settlement).

Most plugins target `language_model`. A request flows through its stages in order:

1. **Pre-request** — every `PreRequestHook` runs; auth, policy, rate-limit, and
   upstream guardrails can reject early.
2. **Route** — each `RouteHook` may rewrite the ordered chain of routing targets.
3. **Execute** — the executor calls the first target, falling back to the next on
   a retriable failure. Streaming responses run through every `StreamHook`.
4. **Settle** — each `SettlementRecorder` runs against the immutable settlement
   context (metering, charging, receipts).
5. **Observe** — `ObserveHook`s see every phase boundary read-only; they never
   influence the request.

> Source: `crates/bitrouter-sdk/src/lib.rs` (the "Anatomy of a request" docs) and
> `crates/bitrouter-sdk/src/language_model/hooks.rs`.

## The hook traits

These are the `language_model` hook traits, defined in
`crates/bitrouter-sdk/src/language_model/hooks.rs` (and `settlement.rs` for the
recorder). All are `Send + Sync` and use `async_trait`.

```rust
// Stage 1 — auth, policy, rate limit, balance, guardrails. First Deny stops the pipeline.
#[async_trait]
pub trait PreRequestHook: Send + Sync {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision>;
}

// Stage 2 — resolve / mutate the ordered routing chain.
#[async_trait]
pub trait RouteHook: Send + Sync {
    async fn resolve(&self, chain: &mut Vec<RoutingTarget>, ctx: &mut PipelineContext) -> Result<()>;
}

// Stage 3 — execution observation + fallback control.
#[async_trait]
pub trait ExecutionHook: Send + Sync {
    async fn on_success(&self, ctx: &PipelineContext, result: &ExecutionResult) -> Result<()>;
    async fn on_failure(&self, ctx: &PipelineContext, error: &BitrouterError) -> FallbackDecision;
}

// Streaming — intercept canonical stream parts (rewrite / drop / abort).
#[async_trait]
pub trait StreamHook: Send + Sync {
    fn interest(&self) -> StreamInterest;
    async fn on_part(&self, ctx: &mut StreamContext, part: StreamPart) -> Result<StreamAction>;
    async fn on_stream_end(&self, ctx: &mut StreamContext, outcome: &StreamOutcome) -> Result<()>;
}

// Read-only observation at every stage boundary. Errors/panics here never affect the request.
#[async_trait]
pub trait ObserveHook: Send + Sync {
    async fn after_phase(&self, phase: Phase, ctx: &PipelineContext);
    async fn on_stream_part(&self, ctx: &StreamContext, part: &StreamPart);
    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome);
    // plus default-no-op on_hop_start / on_hop_end / stream_interest
}
```

A `PreRequestHook` returns a `HookDecision` — `Allow`, or `Deny(DenyReason)` where
`DenyReason` maps to an HTTP status (`Unauthorized` → 401, `Forbidden` → 403,
`PaymentRequired` → 402, `RateLimited` → 429, `GuardrailViolation` / `BadRequest`
→ 400, or a `Custom(status, message)`).

## The `Plugin` trait

A plugin is a **convenience package** — it bundles a related set of hooks plus any
SQL migrations and installs them in one call. It is *not* the atomic unit: every
plugin can be reproduced by calling the relevant sub-builder's hook methods one by
one. The trait lives in `crates/bitrouter-sdk/src/app.rs`:

```rust
pub trait Plugin {
    /// The plugin's identity (for config mapping and logs).
    fn id(&self) -> &PluginId;

    /// Database migrations carried by this plugin. Empty = no database.
    fn migrations(&self) -> Vec<MigrationItem> {
        Vec::new()
    }

    /// Install this plugin's hooks into the builder.
    fn install(&self, app: &mut AppBuilder);
}
```

Inside `install`, you reach the `language_model` sub-builder via
`app.language_model_builder()` and register hooks with `pre_request_hook(...)`,
`route_hook(...)`, `execution_hook(...)`, `stream_hook(...)`,
`settlement_recorder(...)`, or `observe_hook(...)`. Hooks run in registration
order.

## A minimal annotated example

The smallest real plugin in the repo is `bitrouter-attestation`: it registers a
single `RouteHook`. Here is a minimal plugin in the same shape — a
`PreRequestHook` that denies any request carrying a banned substring in its
system prompt. The structure (an `id`, an `install` that registers one hook)
mirrors `plugins/bitrouter-attestation/src/lib.rs` and
`plugins/bitrouter-guardrails/src/plugin.rs` exactly.

```rust
use async_trait::async_trait;
use bitrouter_sdk::{AppBuilder, Plugin, PluginId, Result};
use bitrouter_sdk::language_model::{
    DenyReason, HookDecision, PipelineContext, PreRequestHook,
};

/// A pre-request hook that denies requests whose system prompt contains
/// a banned phrase.
struct BannedPhraseHook {
    phrase: String,
}

#[async_trait]
impl PreRequestHook for BannedPhraseHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        if let Some(system) = &ctx.prompt().system {
            if system.contains(&self.phrase) {
                return Ok(HookDecision::Deny(DenyReason::GuardrailViolation(
                    "request blocked by banned-phrase policy".into(),
                )));
            }
        }
        Ok(HookDecision::Allow)
    }
}

/// The plugin: one id, registers one hook. No migrations, so we lean on
/// the trait's default `migrations()`.
pub struct BannedPhrasePlugin {
    id: PluginId,
    phrase: String,
}

impl BannedPhrasePlugin {
    pub fn new(phrase: impl Into<String>) -> Self {
        Self {
            id: PluginId::new("banned-phrase"),
            phrase: phrase.into(),
        }
    }
}

impl Plugin for BannedPhrasePlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        app.language_model_builder()
            .pre_request_hook(BannedPhraseHook { phrase: self.phrase.clone() });
    }
}
```

The `Cargo.toml` depends on the SDK and `async-trait` (the same two
dependencies the shipped guardrails plugin uses):

```toml
[dependencies]
bitrouter-sdk = "..."   # the BitRouter SDK
async-trait = "0.1"
```

## Installing the plugin into a router

A plugin is installed through `AppBuilder::plugin`, which extends the migration
set and calls your `install`. This is the same `App::builder()` flow shown in the
SDK crate docs:

```rust
use std::sync::Arc;
use bitrouter_sdk::App;
use bitrouter_sdk::language_model::{HttpExecutor, StaticRoutingTable};

let app = App::builder()
    .language_model(|lm| {
        lm.routing_table(Arc::new(StaticRoutingTable::new()))
          .executor(Arc::new(HttpExecutor::with_defaults()?));
    })
    .plugin(BannedPhrasePlugin::new("forbidden"))
    .build()?;
```

With the SDK's `server` feature enabled, `app.serve("127.0.0.1:4356")` wires the
HTTP router and runs it until SIGTERM.

## How the shipped plugins use these hooks

The three plugins in the repo are the canonical worked examples:

- **`bitrouter-guardrails`** — registers a `PreRequestHook` (`GuardrailPreHook`,
  denies request content on a `Block` rule) and a `StreamHook`
  (`GuardrailStreamHook`, redacts `Redact` matches and aborts on `Block` in the
  response stream). Both read the active `RuleSet` from the pipeline's typed
  extensions. See `plugins/bitrouter-guardrails/src/`.
- **`bitrouter-attestation`** — registers a single `RouteHook` that looks up a
  TEE-attestation verdict per confidential routing target and either records it or
  drops unverified targets (fail-closed). See `plugins/bitrouter-attestation/src/lib.rs`.
- **`bitrouter-observe`** — an OpenTelemetry exporter (OTLP traces + metrics) that
  installs an `ObserveHook`; the same handle is also wired as the app's
  `metrics_renderer` so `GET /metrics` can serve it. It is feature-gated behind a
  transport (`otel-http` / `otel-grpc`). See `plugins/bitrouter-observe/src/`.

<Callout type="warn">
The SDK is the public, stable extension surface, but its supporting types
(`PipelineContext`, `StreamContext`, the streaming and settlement types) are
richer than this guide shows and continue to evolve. Treat the trait signatures
above as the contract and read the rustdoc on each trait for the exact, current
method semantics before relying on them. Anything ambiguous — keep your hook
minimal rather than guessing.
</Callout>

## What belongs in a plugin vs deployment code

The SDK is opinionated about pipeline-data correctness, not business logic. Auth,
policy, charging, and metering are **deployment-specific** — the open-source
`bitrouter` binary provides its own implementations of those traits, and a hosted
deployment writes its own. Shared, reusable cross-cutting behaviour (guardrails,
observability, attestation) is what ships as a plugin.

## Next steps

<Cards>
  <Card title="Self-host BitRouter" href="/docs/guides/self-host" description="Run the router you compiled your plugin into, in production." />
  <Card title="Guardrails" href="/docs/features/guardrails" description="The content-firewall plugin, as a worked reference." />
  <Card title="OpenTelemetry" href="/docs/features/opentelemetry" description="OTLP trace + metric export, built on the observe plugin." />
  <Card title="Core repository" href="https://github.com/bitrouterai/bitrouter" description="The bitrouter-sdk crate and the three reference plugins." />
</Cards>
