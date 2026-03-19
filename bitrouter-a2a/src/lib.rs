//! A2A (Agent-to-Agent) protocol implementation for BitRouter.
//!
//! Implements the [A2A v1.0 specification](https://a2a-protocol.org/latest/)
//! for agent identity, discovery, and communication. This crate provides:
//!
//! - **Types** — Full A2A v1.0 schema: Agent Card, Task, Message, Artifact (`card`, `security`, `task`, `message`)
//! - **Client** — A2A protocol client for discovering and communicating with remote agents (`client`)
//! - **JSON-RPC** — Wire format types for the A2A JSON-RPC 2.0 transport (`jsonrpc`)
//! - **Registry** — Trait and file-based implementation for agent card storage
pub mod card;
pub mod client;
pub mod error;
pub mod file_registry;
pub mod jsonrpc;
pub mod message;
pub mod registry;
pub mod security;
pub mod server;
pub mod task;
