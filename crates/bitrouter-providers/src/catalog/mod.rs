//! Runtime model-metadata catalog, fetched from <https://models.dev/api.json>.
//!
//! The compiled-in [`crate::ProviderEntry`] is "how do we talk to provider X" —
//! the catalog is "what models does provider X serve, what do they cost, what
//! are their limits". The two are kept separate on purpose: catalog metadata
//! changes weekly (new models, price drops); auth + URL shape changes rarely
//! and needs a binary release.
//!
//! ## Source
//!
//! <https://models.dev/api.json> is a public catalog maintained by SST (the
//! opencode team). Schema reference: <https://models.dev/api>. The JSON is
//! shaped as `{provider_id: ProviderCatalogEntry}` at the top level; under
//! each provider, `models` maps `model_id → ModelMetadata`.
//!
//! ## Layout in this module
//!
//! - [`types`] — the parsed JSON shape ([`Catalog`], [`ProviderCatalogEntry`],
//!   [`ModelMetadata`], …) — pure data, no I/O.
//! - [`fetch`] — async `reqwest`-driven download of the API document.
//! - [`cache`] — file-based on-disk cache under `$XDG_CACHE_HOME/bitrouter/`
//!   with a 24-hour freshness window. Stale entries are still readable as a
//!   network-failure fallback.

pub mod cache;
pub mod fetch;
pub mod types;

pub use cache::{CacheError, DiskCache};
pub use fetch::{FetchError, fetch_catalog};
pub use types::{Catalog, Modalities, ModelCost, ModelLimit, ModelMetadata, ProviderCatalogEntry};
