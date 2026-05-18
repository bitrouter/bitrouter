//! Compiled-in provider catalog.
//!
//! Built-in providers — the ones a fresh bitrouter binary already "knows how
//! to talk to" — are authored as TOML files under `providers/*.toml` and
//! embedded into the binary via [`include_str!`] at compile time. Each entry
//! declares how to authenticate and which wire protocol the provider serves;
//! it does **not** declare model metadata (pricing, context length, …).
//! Model metadata is fetched from <https://models.dev/api.json> at runtime
//! and merged in by a separate fetcher (see future `catalog` module).
//!
//! ## Layout
//!
//! - [`ProviderEntry`] — the parsed schema of one TOML file.
//! - [`AuthScheme`] — Bearer / Header / OAuth / Native variants. Bearer +
//!   Header cover every static-credential provider in the v0 registry;
//!   OAuth and Native reference handlers that live in other crates.
//! - [`builtin`] — the compile-time registry: [`builtin::all`] returns the
//!   parsed entries, [`builtin::find`] looks one up by id.
//!
//! ## Adding a provider
//!
//! 1. Drop a `providers/<id>.toml` file in this crate. Include the
//!    `doc_url` of the provider's official auth + endpoint reference in the
//!    file header — every entry MUST cite its source.
//! 2. Add an `include_str!` line in `builtin.rs`.
//! 3. Add a parsing test.
//!
//! No Rust code is needed for providers that authenticate by Bearer token or
//! a custom header. OAuth / SigV4 / anything stateful still needs a
//! companion handler in its own crate (e.g. `bitrouter-bedrock` ships the
//! `aws_sigv4` Native handler).

#![deny(missing_docs)]

mod apply;
pub mod builtin;
pub mod catalog;
mod entry;

pub use apply::apply_builtin_defaults;
pub use entry::{AuthScheme, ProtocolMapping, ProviderEntry};

/// Errors raised while loading the compile-time registry.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// One of the embedded TOML files failed to parse. The string carries the
    /// provider id + the underlying `toml` error.
    #[error("failed to parse provider entry '{id}': {source}")]
    Parse {
        /// The id (filename stem) of the entry that failed.
        id: String,
        /// The underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },
    /// Two entries share the same `id` field.
    #[error("duplicate provider id '{id}'")]
    DuplicateId {
        /// The duplicated id.
        id: String,
    },
    /// The `id` field inside the TOML didn't match the filename (the
    /// embedded-files registry uses the filename stem as a sanity check).
    #[error("provider entry id '{declared}' does not match filename '{expected}'")]
    IdMismatch {
        /// The id declared inside the TOML file.
        declared: String,
        /// The filename stem (what we registered the entry as).
        expected: String,
    },
}
