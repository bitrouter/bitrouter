//! Parsed shape of the public model catalog at `https://models.dev/api.json`.
//!
//! Lean by design: only the fields the OSS consumes to enrich a `models_dev`
//! auto-sync provider's catalog — the model ids (the per-provider map keys) and
//! each model's per-1M-token cost. Everything else models.dev publishes
//! (limits, modalities, knowledge cutoff, …) is ignored; serde drops unknown
//! fields. Schema reference: <https://models.dev/api>.
//!
//! Source: <https://models.dev/api.json> is a public catalog maintained by SST
//! (the opencode team), shaped as `{ provider_id: { models: { model_id: … } } }`.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Top-level document: `{ provider_key: CatalogProvider }`.
pub type Catalog = BTreeMap<String, CatalogProvider>;

/// One provider's catalog entry. Only the `models` map is read.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogProvider {
    /// Map of native model id → metadata.
    #[serde(default)]
    pub models: BTreeMap<String, CatalogModel>,
}

/// One model's published metadata. The native id is the map key in
/// [`CatalogProvider::models`]; only the cost is read here.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogModel {
    /// Per-1M-token cost, if published.
    #[serde(default)]
    pub cost: Option<CatalogCost>,
}

/// Per-1M-token USD cost (`https://models.dev/api`). USD per 1M tokens equals
/// µUSD per token, which is the unit the SDK's pricing config uses, so the
/// values map across unchanged. Only the base input/output rates are read;
/// cache / reasoning rates are ignored by OSS metering.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogCost {
    /// Base (no-cache) input rate.
    #[serde(default)]
    pub input: Option<f64>,
    /// Text output rate.
    #[serde(default)]
    pub output: Option<f64>,
}
