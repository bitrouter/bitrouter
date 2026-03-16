//! BitRouter observability crate.
//!
//! Provides spend tracking, metrics collection, and request observation for the
//! BitRouter LLM routing system. All observability flows through the
//! [`ObserveCallback`](bitrouter_core::observe::ObserveCallback) trait defined
//! in `bitrouter-core`.

pub mod composite;
pub mod cost;
pub mod entity;
pub mod metrics;
pub mod migration;
pub mod observer;
pub mod spend;
