//! BitRouter observability crate.
//!
//! Provides spend tracking, metrics collection, and request observation for the
//! BitRouter LLM routing system. All observability flows through the callback
//! traits defined in `bitrouter-core`: [`ObserveCallback`], [`ToolObserveCallback`],
//! and [`AgentObserveCallback`].
//!
//! The primary entry point is [`builder::ObserveStack`], which assembles the
//! full observation pipeline via a builder pattern.

pub mod builder;
pub mod composite;
pub mod entity;
pub mod metrics;
pub mod migration;
pub mod spend;

pub(crate) mod agent_observer;
pub(crate) mod observer;
pub(crate) mod tool_observer;
