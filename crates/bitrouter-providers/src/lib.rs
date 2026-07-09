//! Provider integration glue.
//!
//! The providers a fresh bitrouter binary "knows how to talk to" come from the
//! provider registry, **fetched at runtime** (and disk-cached) and merged into
//! the routing table by [`registry`] — auth scheme, wire protocol, base URL,
//! model catalog, and pricing all travel in that data. Nothing is vendored into
//! the binary. The lone compiled-in built-in is the hosted `bitrouter` cloud
//! gateway, kept as TOML (`providers/bitrouter.toml`) for zero-config defaults
//! and its local OAuth/API-key auth applier. The public registry also declares
//! provider id `bitrouter`; the merge uses that public metadata without treating
//! it as a synthetic "all canonical models" provider. The same registry mapping
//! is reused for a fetched provider on demand via [`builtin::entry_from_registry`]
//! (e.g. `bitrouter providers login` resolving an OAuth handler + its public
//! params).
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
//! - [`builtin`] — the lone compiled-in built-in (the `bitrouter` cloud
//!   gateway): [`builtin::all`] / [`builtin::find`]. [`builtin::entry_from_registry`]
//!   maps a fetched registry provider to the same shape on demand.
//! - [`oauth`] — RFC 8628 device-code flow + on-disk token store.
//! - [`copilot`] — `CopilotAuthApplier` + GitHub→Copilot token exchange,
//!   the worked example of an OAuth-driven `AuthApplier`.
//! - [`registry`] — runtime fetch + on-disk cache of the provider registry's
//!   `dist/` artifacts (the provider definitions + canonical model catalog),
//!   and the merge into a parsed config.
//!
//! ## Adding a provider
//!
//! For a **static-credential or OAuth provider** (the `AuthApplier` slot):
//!
//! 1. Add it to the provider registry with its `auth` block (and `doc_url`).
//!    Once released, it is fetched + merged automatically — nothing is vendored
//!    into this binary.
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
pub mod claude_code;
#[cfg(feature = "pkce")]
pub mod codex;
pub mod copilot;
mod entry;
#[cfg(feature = "pkce")]
pub mod import;
pub mod oauth;
pub mod registry;
#[cfg(feature = "pkce")]
pub mod supergrok;

pub use apply::{
    activate_stored_credential_providers, apply_builtin_defaults, zero_config,
    zero_config_env_var_providers,
};
pub use entry::{AuthScheme, ProtocolMapping, ProviderEntry};

/// Errors raised while loading the compile-time registry.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The compiled-in `bitrouter.toml` failed to parse. The string carries
    /// the provider id + the underlying `toml` error.
    #[error("failed to parse provider entry '{id}': {source}")]
    Parse {
        /// The id (filename stem) of the entry that failed.
        id: String,
        /// The underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },
    /// A registry provider could not be mapped to a [`ProviderEntry`] — e.g. it
    /// declared no `auth` block, or an auth scheme missing a required field (a
    /// `bearer` scheme with no `env`). Surfaced by [`builtin::entry_from_registry`].
    #[error("invalid registry provider: {message}")]
    Snapshot {
        /// What was wrong with the provider.
        message: String,
    },
}
