//! App-owned native execution evidence, independent of harness session storage.

pub mod accounting;
pub mod claude_hooks;
pub mod claude_sdk;
pub mod codex_proxy;
pub mod collector;
pub mod execution;
pub mod history;
pub mod journal;
pub mod projection;
pub mod service;
pub mod store;
pub mod types;
pub(crate) mod workspace;
