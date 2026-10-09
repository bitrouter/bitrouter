//! Application-level provider activation and routing-config projection.
//!
//! Schema, explicit network refresh and injected snapshot storage live in
//! `bitrouter_ai::catalog`. This bridge consumes a supplied snapshot and retains
//! credential gating, user overrides and SDK configuration above AI.

pub mod apply;
