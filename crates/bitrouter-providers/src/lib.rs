//! Compiled-in provider catalog.
//!
//! Built-in providers — the ones a fresh bitrouter binary already "knows how
//! to talk to" — are **derived from the compiled-in registry snapshot**
//! ([`registry::embedded`]): every provider in the snapshot that declares an
//! `auth` scheme becomes a [`ProviderEntry`] (auth + wire protocol + base URL).
//! The lone exception is the hosted `bitrouter` cloud gateway, kept as a TOML
//! (`providers/bitrouter.toml`) because its id shadows the registry's pool
//! entry. The provider list and per-provider model catalog (pricing, context
//! length, …) come from the same registry distribution, fetched at runtime and
//! merged in by [`registry`].
//!
//! ## Where a new provider lives — `AuthApplier` vs `Executor`
//!
//! Bitrouter has two integration slots for providers, and the slot a
//! provider needs decides whether its code lives here or in its own crate:
//!
//! - **`AuthApplier`-shaped** (this crate). The provider reuses the
//!   `HttpExecutor` + `OutboundDispatch` pipeline and only overrides
//!   credential placement (Bearer, custom header, OAuth + token-exchange,
//!   AAD, …). Async + stateful behaviour is fine — see [`copilot`] for
//!   the GitHub→Copilot token-exchange pattern. Implementations are small
//!   and share the OAuth + token-store + registry infrastructure that
//!   already lives in this crate, so an additional dep is rarely needed.
//! - **`Executor`-shaped** (its own crate). The provider replaces the
//!   entire request path — typically because a vendor SDK owns the binary
//!   event-stream framing, the auth + signing + retry only makes sense as
//!   a unit, or the wire format isn't HTTP+JSON+SSE. `bitrouter-bedrock` is
//!   the prototype: `aws-sdk-bedrockruntime` + `aws-config` are heavy
//!   transitive deps and folding them in (even feature-gated) would resolve
//!   them in every `Cargo.lock` that touches `bitrouter-providers`.
//!
//! In short: **if you can implement it as an `AuthApplier`, put it here. If
//! it needs its own `Executor`, give it its own crate.**
//!
//! ## Layout
//!
//! - [`ProviderEntry`] — the compiled-in auth + transport shape of one built-in
//!   provider (derived from the registry snapshot, or the `bitrouter` TOML).
//! - [`AuthScheme`] — Bearer / Header / OAuth / Native variants. Bearer +
//!   Header cover every static-credential provider; OAuth references a
//!   handler that runs the device-code flow + applies a token; Native
//!   references a handler in a sibling crate (e.g. `aws_sigv4` in
//!   `bitrouter-bedrock`).
//! - [`builtin`] — the compile-time registry: [`builtin::all`] returns the
//!   parsed entries, [`builtin::find`] looks one up by id.
//! - [`oauth`] — RFC 8628 device-code flow + on-disk token store.
//! - [`copilot`] — `CopilotAuthApplier` + GitHub→Copilot token exchange,
//!   the worked example of an OAuth-driven `AuthApplier`.
//! - [`registry`] — runtime fetch + on-disk cache of the provider registry's
//!   `dist/` artifacts (the provider list + canonical model catalog), and the
//!   merge into a parsed config.
//!
//! ## Adding a provider
//!
//! For a **static-credential or OAuth provider** (the `AuthApplier` slot):
//!
//! 1. Add it to the provider registry with its `auth` block (and `doc_url`),
//!    then refresh the compiled-in snapshot: `cargo xtask vendor-registry
//!    --from <registry>/dist`. It becomes a built-in automatically.
//! 2. For Bearer / Header schemes, no Rust code is needed. For OAuth or
//!    anything stateful, add an `AuthApplier` impl in a sibling module
//!    (see [`copilot`]) keyed by the registry `auth.handler` name, and
//!    register it during binary startup.
//!
//! For an **SDK-driven provider** (the `Executor` slot): create a new
//! `bitrouter-<name>` crate following the `bitrouter-bedrock` template —
//! depend on `bitrouter-sdk`, implement [`Executor`](bitrouter_sdk::language_model::Executor),
//! register it on the `DispatchExecutor` at binary startup. Do not add it
//! to this crate.

#![deny(missing_docs)]

#[cfg(feature = "pkce")]
pub mod anthropic;
mod apply;
pub mod builtin;
#[cfg(feature = "pkce")]
pub mod codex;
pub mod copilot;
mod entry;
#[cfg(feature = "pkce")]
pub mod import;
pub mod oauth;
pub mod registry;

pub use apply::{apply_builtin_defaults, zero_config, zero_config_env_var_providers};
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
    /// The compiled-in registry snapshot failed to parse, or a provider in it
    /// declared an auth scheme missing a required field (e.g. a `bearer` scheme
    /// with no `env`). A build-time invariant — the vendored snapshot is
    /// registry-validated and drift-checked.
    #[error("invalid embedded registry snapshot: {message}")]
    Snapshot {
        /// What was wrong with the snapshot.
        message: String,
    },
}
