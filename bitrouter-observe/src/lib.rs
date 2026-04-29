//! BitRouter observability crate.
//!
//! Provides spend tracking, metrics collection, and request observation for the
//! BitRouter LLM routing system. All observability flows through the callback
//! traits defined in `bitrouter-core`: [`ObserveCallback`] and
//! [`ToolObserveCallback`].
//!
//! The primary entry point is [`builder::ObserveStack`], which assembles the
//! full observation pipeline via a builder pattern.

pub mod builder;
pub mod composite;
pub mod entity;
pub mod metrics;
pub mod migration;
pub mod spend;

#[cfg(feature = "otlp")]
pub mod otlp;

pub(crate) mod model_observer;
pub(crate) mod tool_observer;
