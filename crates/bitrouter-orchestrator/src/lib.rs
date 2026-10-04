//! BRO's native coding agent engine. Process transports and terminal rendering
//! are assembled by the `bro` executable, not by this crate.

pub mod agent;
mod context;
mod control;
pub mod item;
pub mod service;
pub mod store;
pub mod thread;
mod tools;
pub mod turn;
