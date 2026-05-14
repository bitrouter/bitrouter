//! The `language_model` protocol module.
//!
//! Carries the full seven-hook set: `PreRequestHook`, `RouteHook`,
//! `ExecutionHook`, `StreamHook`, `ChargeStrategy`, `SettlementRecorder`,
//! `ObserveHook`. See design doc 003.
//!
//! Per design doc 003 §0, this module's `Pipeline` / hook traits are **not**
//! shared with `mcp` / `acp` — those protocols define their own. Reuse is via
//! shared library code at the crate root.

pub mod builder;
pub mod context;
pub mod executor;
pub mod hooks;
pub mod pipeline;
pub mod routing;
pub mod settlement;
pub mod stream;
pub mod types;

#[cfg(test)]
mod tests;

// ===== canonical re-exports — `language_model::Pipeline`, etc. =====

pub use builder::PipelineBuilder;
pub use context::{PipelineContext, StreamContext};
pub use executor::{Executor, MockExecutor, MockResponse, StreamPartStream};
pub use hooks::{
    DenyReason, ExecutionHook, FallbackDecision, HookDecision, ObserveHook, Phase, PreRequestHook,
    RequestOutcome, RouteHook, StreamHook,
};
pub use pipeline::{DEFAULT_KEEPALIVE, Pipeline};
pub use routing::{
    DefaultFallbackPolicy, FallbackPolicy, ModelInfo, RoutingPrefs, RoutingTable, SortOrder,
    StaticRoutingTable,
};
pub use settlement::{ChargeOutcome, ChargeStrategy, SettlementContext, SettlementRecorder};
pub use stream::{
    SseFrame, SseKeepaliveStream, StreamAction, StreamInterest, StreamOutcome, StreamProcessor,
    UsageAccumulator,
};
pub use types::{
    ApiProtocol, Content, ExecutionResult, FinishReason, GenerateResult, GenerationParams, Message,
    PipelineRequest, PipelineResponse, Prompt, Role, RoutingTarget, StreamPart, Tool, Usage,
};
