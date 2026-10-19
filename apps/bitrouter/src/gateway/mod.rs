//! Gateway service implementation.

pub mod assembly;
pub mod caller;
pub mod catalog;
pub mod error;
pub mod extensions;
pub mod http;
pub mod mcp;
pub mod metering;
pub mod model;
pub mod observability;
pub mod pipeline;
pub mod providers;
pub mod routing;
#[cfg(feature = "multi-instance")]
pub(crate) mod shared;
