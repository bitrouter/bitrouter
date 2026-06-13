//! NEAR AI Cloud confidential-inference verifier — the first
//! [`ConfidentialVerifier`](crate::ConfidentialVerifier) impl.
//!
//! Verification is split across focused modules: `report` (wire types),
//! plus the binding / quote / NRAS / DCAP-policy / signature checks added in
//! later tasks. `NearVerifier` (Task 6) composes them.

pub mod binding;
pub mod dcap;
pub mod nvidia;
pub mod report;
pub mod tdx;
