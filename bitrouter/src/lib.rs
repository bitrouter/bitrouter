//! BitRouter runtime library.
//!
//! This crate hosts the long-running BitRouter proxy: HTTP server, router,
//! payment middleware, daemon lifecycle, configuration paths, and database
//! migrations. It is consumed by the `bitrouter-cli` binary; it produces no
//! binary of its own.

#![recursion_limit = "256"]

#[cfg(not(any(feature = "tempo", feature = "solana")))]
compile_error!(
    "bitrouter requires at least one payment chain feature: enable `tempo` and/or `solana`"
);

pub mod auth;
pub mod runtime;
