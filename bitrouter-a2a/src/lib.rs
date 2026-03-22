//! A2A (Agent-to-Agent) protocol types and gateway components for BitRouter.
//!
//! Implements the [A2A v0.3.0 specification](https://a2a-protocol.org/latest/)
//! for agent identity, discovery, and communication. This crate provides:
//!
//! - **Types** — Full A2A v0.3.0 schema: Agent Card, Task, Message, Artifact
//! - **Gateway traits** — [`server::A2aDiscovery`] and [`server::A2aProxy`] for downstream serving
//! - **Transports** — JSON-RPC, REST, and gRPC transport implementations (feature-gated)
//! - **Client** — Upstream connection and registry (feature-gated)
#[cfg(feature = "client")]
pub mod client;
pub mod config;
pub mod error;
pub mod server;
#[cfg(feature = "client")]
pub mod transports;
pub mod types;
