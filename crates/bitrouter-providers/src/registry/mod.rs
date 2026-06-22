//! Runtime provider + model catalog, fetched from the bitrouter provider
//! registry's distribution artifacts.
//!
//! The compiled-in [`crate::ProviderEntry`] answers "how do we talk to provider
//! X" (auth + URL shape, rarely changing). This module answers "which providers
//! exist, which canonical models does each serve, and at what price" — data
//! that changes weekly and is curated in the public registry rather than
//! shipped in the binary.
//!
//! ## Source
//!
//! <https://github.com/bitrouter/provider-registry> publishes two deterministic
//! JSON files under `dist/`: `providers.json` (the provider view — each model
//! carrying its dist-resolved protocol + rate limits) and `models.json` (the
//! model view; bitrouter reads its `id`s for the canonical vocabulary). This
//! module fetches both, merges them into a [`types::RegistryData`], and
//! disk-caches the result.
//!
//! ## Layout
//!
//! - [`types`] — the parsed JSON shape ([`types::RegistryData`],
//!   [`types::RegistryProvider`], [`types::CanonicalModel`], …) — pure data.
//! - [`fetch`] — async `reqwest`-driven download of the two dist files.
//! - [`cache`] — file-based on-disk cache under `$XDG_CACHE_HOME/bitrouter/`
//!   with a 24-hour freshness window and a stale-fallback read.
//! - [`apply`] — fetch-or-cache orchestration and the merge of a
//!   [`types::RegistryData`] into a parsed
//!   [`Config`](bitrouter_sdk::config::Config), credential-gated.
//! - [`embedded`] — the compiled-in dist snapshot (offline default + the source
//!   the built-in [`ProviderEntry`](crate::ProviderEntry)s are derived from).

pub mod apply;
pub mod cache;
pub mod embedded;
pub mod fetch;
pub mod types;
