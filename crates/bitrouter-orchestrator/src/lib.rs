//! BRO's native coding agent engine. Process transports and terminal rendering
//! are assembled by the `bro` executable, not by this crate.

pub mod agent;
mod context;
pub mod service;
mod tools;
