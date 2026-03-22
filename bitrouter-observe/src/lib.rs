//! BitRouter observability crate.
//!
//! Provides spend tracking, metrics collection, and request observation for the
//! BitRouter LLM routing system. All observability flows through the callback
//! traits defined in `bitrouter-core`: [`ObserveCallback`], [`ToolObserveCallback`],
//! and [`AgentObserveCallback`].

pub mod agent_observer;
pub mod composite;
pub mod cost;
pub mod entity;
pub mod metrics;
pub mod migration;
pub mod observer;
pub mod spend;
pub mod tool_observer;
