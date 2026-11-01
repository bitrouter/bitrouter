//! Resource tenancy value contracts, without authentication, storage or transport.
//!
//! Validate typed values before sending them or accepting them at a trust boundary.
//! Validation checks wire bounds and internal consistency; it never grants access.

pub mod authority;
pub mod configuration;
pub mod directory;
pub mod execution;
pub mod execution_v2;
pub mod identity;
pub mod operation;
pub mod ordered;
pub mod resources;
pub mod usage_evidence;
pub mod usage_evidence_v2;
pub mod validation;

mod number;

#[cfg(feature = "schema")]
mod schema;
