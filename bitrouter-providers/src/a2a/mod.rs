//! A2A client runtime — connects to upstream A2A agents and proxies capabilities.
//!
//! This module contains the client, transport, and registry implementations
//! for communicating with upstream A2A agents. Protocol types, server traits,
//! and error definitions live in [`bitrouter_core::api::a2a`].

pub mod client;
pub mod transports;
