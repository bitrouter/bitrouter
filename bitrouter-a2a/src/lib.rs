//! A2A (Agent-to-Agent) protocol types and gateway components for BitRouter.
//!
//! Implements the [A2A v1.0 specification](https://a2a-protocol.org/latest/)
//! for agent identity, discovery, and communication. This crate provides:
//!
//! - **Types** — Full A2A v1.0 schema: Agent Card, Task, Message, Artifact
//! - **Gateway traits** — [`server::A2aDiscovery`] and [`server::A2aProxy`] for downstream serving
//! - **Client** — A2A protocol client and upstream connection (feature-gated)
pub mod admin;
pub mod card;
#[cfg(feature = "client")]
pub mod client;
pub mod config;
pub mod error;
pub mod jsonrpc;
pub mod message;
pub mod request;
pub mod security;
pub mod server;
pub mod stream;
pub mod task;
