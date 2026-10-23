//! Product adapters for explicit service clients and local discovery.

pub mod agent_local;
pub mod control;
pub(crate) mod local_gateway;
#[cfg(feature = "harness")]
pub mod sessions;
