//! Presentation values shared by command adapters and terminal views.

pub mod agents;
#[cfg(feature = "harness")]
pub mod cost;
#[cfg(feature = "harness")]
pub mod journal;
pub mod log_tail;
pub mod native_agent;
pub mod selection;

#[cfg(feature = "harness")]
pub(crate) mod transcript;
