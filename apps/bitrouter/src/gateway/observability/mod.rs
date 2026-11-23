//! Internal request events and metric-rendering seams.

pub mod decision;
pub mod event;
pub mod metrics;
pub mod prediction;
pub mod settlement;

pub mod inventory;

pub mod requests;

pub(crate) mod binding;

/// Gateway observation vocabulary and exporter invariants.
#[cfg(any(test, feature = "otel-http", feature = "otel-grpc"))]
pub mod schema;
/// Application and extension observation attributes.
pub mod span_attributes;
