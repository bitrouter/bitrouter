//! Resource tenancy value contracts, without authentication, storage or transport.
//!
//! Validate typed values before sending them or accepting them at a trust boundary.
//! Validation checks wire bounds and internal consistency; it never grants access.

pub mod admission;
pub mod admission_v2;
pub mod authority;
pub mod configuration;
pub mod directory;
pub mod execution;
pub mod execution_v2;
pub mod generation_defaults;
pub mod identity;
pub mod operation;
pub mod ordered;
pub mod resources;
pub mod resources_v2;
pub mod usage_evidence;
pub mod usage_evidence_v2;
pub mod validation;

mod number;

#[cfg(feature = "schema")]
mod schema;
