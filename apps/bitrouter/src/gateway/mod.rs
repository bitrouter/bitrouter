//! Gateway service implementation.

pub mod assembly;
pub(crate) mod caller;
pub(crate) mod catalog;
pub mod error;
pub mod extensions;
pub(crate) mod http;
pub(crate) mod mcp;
pub(crate) mod metering;
pub(crate) mod model;
pub(crate) mod observability;
pub(crate) mod pipeline;
pub(crate) mod providers;
pub(crate) mod routing;
#[cfg(feature = "multi-instance")]
pub(crate) mod shared;
pub(crate) mod transport;

pub(crate) mod conversion_report;

/// HTTP adapters for embedding gateway handlers.
pub mod http_adapter;
/// Bounded response serialization and SSE framing.
pub mod wire;
