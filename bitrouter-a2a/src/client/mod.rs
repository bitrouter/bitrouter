//! A2A client and gateway upstream components.
//!
//! - [`a2a_client::A2aClient`] — JSON-RPC 2.0 client for A2A servers
//! - [`upstream::UpstreamA2aAgent`] — Live connection to a single upstream agent
//! - [`registry::UpstreamAgentRegistry`] — Single-agent registry implementing gateway traits

pub mod a2a_client;
pub mod registry;
pub mod upstream;
