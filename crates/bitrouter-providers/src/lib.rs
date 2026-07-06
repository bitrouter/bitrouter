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
//!   AAD, …). Async + stateful behaviour is fine — see [`copilot`] for the
//!   GitHub→Copilot token-exchange pattern. This is the slot for **every**
//!   provider whose wire is HTTP+JSON+SSE, which is effectively all of them:
//!   the big clouds included. AWS Bedrock uses the `bedrock-mantle`
//!   OpenAI/Responses/Messages endpoints and Azure OpenAI its `/openai/v1`
//!   surface — both plain `bearer` registry entries, no vendor SDK and no
//!   custom code.
//! - **`Executor`-shaped** (its own crate). The provider replaces the
//!   entire request path — reserved for the rare case where the wire is
//!   *not* HTTP+JSON+SSE that an existing [`OutboundAdapter`] can decode
//!   (e.g. a vendor SDK owning a binary event-stream framing). **There are
//!   no built-in providers in this slot today** — it exists as an escape
//!   hatch, not a template. (Bedrock's native Converse API is such a wire,
//!   but its OpenAI-compatible `bedrock-mantle` endpoints let it live in the
//!   `AuthApplier` slot instead, so the heavy AWS SDK is not vendored.)
//!
//! In short: **implement it as an `AuthApplier` here unless the upstream
//! wire genuinely isn't HTTP+JSON+SSE — which, for current providers, never
//! happens.**
//!
//! ## Layout
//!
//! - [`ProviderEntry`] — the compiled-in auth + transport shape of one built-in
//!   provider (derived from the registry snapshot, or the `bitrouter` TOML).
//! - [`AuthScheme`] — Bearer / Header / OAuth / Native variants. Bearer +
//!   Header cover every static-credential provider (including the big clouds
//!   Bedrock + Azure); OAuth references a handler that runs the device-code
//!   flow + applies a token; Native references a request-time `AuthApplier`
//!   handler by name (no built-in provider uses it today).
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
//!    anything stateful (device-code, token-exchange), add an `AuthApplier`
//!    impl in a sibling module (see [`copilot`]) keyed by the registry
//!    `auth.handler` name, and register it during binary
//!    startup. Regional / per-account base URLs (Bedrock region, Azure
//!    resource) are expressed as `${VAR}` in the registry `api_base` and
//!    resolved from the environment at merge time.
//!
//! An **`Executor`-slot provider** (its own crate) is only needed for a wire
//! that isn't HTTP+JSON+SSE; no built-in provider needs one. See
//! [`Executor`](bitrouter_sdk::language_model::Executor) if you hit that case.

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
