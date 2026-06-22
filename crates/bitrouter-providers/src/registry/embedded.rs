//! Compiled-in snapshot of the provider-registry distribution.
//!
//! The two `dist/` artifacts are vendored under `crates/bitrouter-providers/
//! embedded/` and `include_str!`'d here. This is the **offline default**: a
//! fresh or air-gapped daemon resolves providers from this snapshot, and the
//! runtime fetch ([`super::apply::load_or_cached`]) overlays a fresher copy when
//! the network is reachable.
//!
//! Refresh the snapshot from a released registry tag with
//! `cargo xtask vendor-registry`; CI fails if it drifts from the pinned tag.

use crate::registry::types::{CanonicalModel, Envelope, RegistryData, RegistryProvider};

/// The vendored `providers.json` (provider view).
pub const PROVIDERS_JSON: &str = include_str!("../../embedded/providers.json");
/// The vendored `models.json` (model view — its ids are the canonical set).
pub const MODELS_JSON: &str = include_str!("../../embedded/models.json");

/// Parse the embedded provider list. Returns a parse error only if the vendored
/// snapshot is malformed (a build-time invariant, guarded by the drift check).
pub fn providers() -> Result<Vec<RegistryProvider>, serde_json::Error> {
    let env: Envelope<RegistryProvider> = serde_json::from_str(PROVIDERS_JSON)?;
    Ok(env.data)
}

/// Parse the embedded snapshot into [`RegistryData`] — the offline floor the
/// merge falls back to when no fresher dist has been fetched.
pub fn data() -> Result<RegistryData, serde_json::Error> {
    let providers = providers()?;
    let models: Envelope<CanonicalModel> = serde_json::from_str(MODELS_JSON)?;
    Ok(RegistryData {
        providers,
        canonical: models.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses() {
        let data = data().expect("vendored snapshot must parse");
        assert!(!data.providers.is_empty(), "snapshot has providers");
        assert!(!data.canonical.is_empty(), "snapshot has canonical models");
        // The 8 migrated built-ins are present and auth-bearing.
        let auth_bearing: Vec<&str> = data
            .providers
            .iter()
            .filter(|p| p.auth.is_some())
            .map(|p| p.name.as_str())
            .collect();
        for id in [
            "openai",
            "anthropic",
            "google",
            "openrouter",
            "github-copilot",
            "openai-codex",
            "opencode-zen",
            "opencode-go",
        ] {
            assert!(
                auth_bearing.contains(&id),
                "{id} should be an auth-bearing built-in"
            );
        }
    }
}
