//! Terminal rendering and input over supplied presentation values.
//!
//! Views emit shared commands and do not read databases, provider credentials
//! or execution state directly. The application adapters own those operations.
//! Permission policy lives in harness; journal and cost facts in presentation.

pub mod agents;
pub mod agents_menu;
pub mod code;
pub mod composer;
pub mod editor;
pub mod lifecycle;
pub mod native_agent;
pub mod permission;
pub mod render;
pub mod select;
pub mod wrap;
pub mod writer;
