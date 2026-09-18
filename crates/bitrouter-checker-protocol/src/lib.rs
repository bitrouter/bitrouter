//! Shared request-check capability and HTTP wire contract for BitRouter request-check services.
//!
//! This crate deliberately contains no host runtime, transport, receipt, or
//! rule-engine abstractions. Those responsibilities stay with their respective
//! applications.

#![forbid(unsafe_code)]

pub mod capability;
pub mod v1;
