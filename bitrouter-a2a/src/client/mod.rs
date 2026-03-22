//! A2A client and gateway upstream components.
//!
//! - [`upstream::UpstreamA2aAgent`] — Live connection to a single upstream agent
//! - [`registry::UpstreamAgentRegistry`] — Single-agent registry implementing gateway traits

pub mod registry;
pub mod upstream;
