//! Lightweight public contracts shared by BitRouter clients and extensions.
//!
//! Model/protocol data, capability definitions and observation vocabulary live
//! here. Service execution, conversion, authentication and transport live in the
//! application. This crate performs no I/O or runtime initialization.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod conversion;
pub mod decisions;
pub mod error;
pub mod event;
pub mod extension;
pub mod observe;
pub mod protocol;
pub mod types;
