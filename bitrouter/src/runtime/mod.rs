#[cfg(feature = "a2a")]
pub mod a2a;
pub mod app;
pub mod auth;
pub mod daemon;
pub mod error;
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod migration;
pub mod paths;
pub mod router;
pub mod server;

pub use app::{AppRuntime, resolve_database_url};
pub use migration::migrate;
pub use paths::{PathOverrides, RuntimePaths, resolve_home};
pub use router::Router;
