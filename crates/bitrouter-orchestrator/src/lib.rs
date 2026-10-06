//! BRO's native durable coding runtime, external-agent ACP stack and workspace
//! resource clients. The external-agent stack is available with the `acp` feature.
//! The `bro` executable assembles database, host configuration and UI adapters.

#[cfg(feature = "acp")]
pub mod acp;
pub mod agent;
mod context;
mod control;
pub mod harness;
pub mod item;
pub mod service;
pub mod store;
pub mod thread;
mod tools;
pub mod turn;
