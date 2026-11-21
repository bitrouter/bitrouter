//! Application-owned scheduling, checkpoint and provider-attempt contracts.

pub mod checkpoint;
pub mod evidence;
pub mod protocol;

pub mod skills;

/// Application-owned harness records.
pub mod harness;

/// Claims exchanged with an operator-paired generation adapter.
pub mod private_adapter;
