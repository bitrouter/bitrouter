//! Application-owned durable formats and their shared validation.
//! Gateway persistence never derives execution authority from stored observations.

pub(crate) mod partition;

pub(crate) mod scope;

pub mod evaluation;

pub(crate) mod execution;
pub(crate) mod session_identity;

pub(crate) mod operation_budget;

pub(crate) mod write;

pub(crate) mod digest;
pub(crate) mod gateway_records;
