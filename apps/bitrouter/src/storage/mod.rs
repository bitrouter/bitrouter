//! Application-owned durable formats and their shared validation.
//! Core and harness share codecs without sharing live execution authority.

pub(crate) mod acp_history;

#[cfg(feature = "core")]
pub(crate) mod scope;

pub mod checkpoint;

pub mod evaluation;

pub mod evolution;
pub mod execution;
pub mod session_identity;

pub mod capture;

pub mod skills;

#[cfg(feature = "core")]
pub(crate) mod operation_budget;
