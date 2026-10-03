//! BRO's native coding agent engine. Process transports and terminal rendering
//! are assembled by the `bro` executable, not by this crate.

pub mod agent;
mod context;
pub mod core;
pub mod service;
pub mod store;
mod tools;
