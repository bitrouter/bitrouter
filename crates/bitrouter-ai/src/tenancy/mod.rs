//! Resource tenancy value contracts, without authentication, storage or transport.
//!
//! Validate typed values before sending them or accepting them at a trust boundary.
//! Validation checks wire bounds and internal consistency; it never grants access.

pub mod authority;
pub mod configuration;
pub mod execution;
pub mod identity;
pub mod operation;
pub mod resources;
pub mod usage_evidence;
pub mod validation;

mod number;

#[cfg(feature = "schema")]
mod schema;
