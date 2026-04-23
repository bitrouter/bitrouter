//! ACP registry fetcher with on-disk TTL cache.
//!
//! The public registry is ~20KB of JSON listing ACP-capable agents and
//! their distribution methods.  We cache it at `<home>/cache/acp-registry.json`
//! with a TTL so cold starts hit the network at most once per TTL window.
//!
//! Precedence for the URL (resolved by [`resolve_registry_url`]):
//!
//! 1. Environment variable [`REGISTRY_URL_ENV`]
//! 2. `acp_registry_url` field in `bitrouter.yaml`
//! 3. [`DEFAULT_REGISTRY_URL`]

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use bitrouter_config::AgentConfig;
use bitrouter_config::acp::{
    DEFAULT_REGISTRY_URL, REGISTRY_URL_ENV, RegistryIndex, registry_agent_to_config,
};
use serde::{Deserialize, Serialize};

use super::state::now_unix_seconds;

/// Default TTL for the on-disk registry cache (one hour).
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// HTTP timeout for registry fetches.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// On-disk cache envelope — adds a fetch timestamp to the raw index.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRegistry {
    fetched_at: u64,
    index: RegistryIndex,
}

/// Resolve the effective registry URL using env > config > default
/// precedence.  Always returns a trimmed, non-empty string.
pub fn resolve_registry_url(config_override: Option<&str>) -> String {
    if let Ok(env) = std::env::var(REGISTRY_URL_ENV) {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(cfg) = config_override {
        let trimmed = cfg.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    DEFAULT_REGISTRY_URL.to_owned()
}

/// Fetch the registry, serving the cache when it is fresher than
/// `ttl_secs`.  On network failure, falls back to any existing cache
/// regardless of age.
pub async fn fetch_registry(
    cache_file: &Path,
    ttl_secs: u64,
    registry_url: &str,
) -> Result<RegistryIndex, String> {
    let now = now_unix_seconds();

    // Fast path: fresh cache.
    if let Some(cached) = read_cache(cache_file).await
        && now.saturating_sub(cached.fetched_at) < ttl_secs
    {
        return Ok(cached.index);
    }

    // Otherwise: network.
    match fetch_from_network(registry_url).await {
        Ok(index) => {
            write_cache(
                cache_file,
                &CachedRegistry {
                    fetched_at: now,
                    index: index.clone(),
                },
            )
            .await;
            Ok(index)
        }
        Err(e) => {
            // Network failed: fall back to stale cache if present.
            if let Some(cached) = read_cache(cache_file).await {
                tracing::warn!(
                    url = %registry_url,
                    error = %e,
                    "registry fetch failed; serving stale cache"
                );
                return Ok(cached.index);
            }
            Err(e)
        }
    }
}

/// Additively merge the registry into `agents`: for every registry
/// entry whose id is *not* already present, insert the converted
/// [`AgentConfig`].  Existing ids (from built-ins or user config) are
/// left untouched so the user's on-disk configuration always wins.
///
/// Returns the number of new agents added.
pub fn merge_registry_into_agents(
    index: &RegistryIndex,
    agents: &mut HashMap<String, AgentConfig>,
) -> usize {
    let mut added = 0;
    for agent in &index.agents {
        if !agents.contains_key(&agent.id) {
            agents.insert(agent.id.clone(), registry_agent_to_config(agent));
            added += 1;
        }
    }
    added
}

/// Force-refresh: bypass the cache and hit the network.  On success the
/// cache is updated; on failure the error propagates (callers should
/// treat a `--refresh` failure as fatal for that command).
pub async fn fetch_registry_fresh(
    cache_file: &Path,
    registry_url: &str,
) -> Result<RegistryIndex, String> {
    let index = fetch_from_network(registry_url).await?;
    write_cache(
        cache_file,
        &CachedRegistry {
            fetched_at: now_unix_seconds(),
            index: index.clone(),
        },
    )
    .await;
    Ok(index)
}

async fn fetch_from_network(url: &str) -> Result<RegistryIndex, String> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build http client: {e}"))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("registry fetch failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "registry fetch returned HTTP {}",
            response.status()
        ));
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| format!("failed to read registry body: {e}"))?;

    serde_json::from_slice::<RegistryIndex>(&body)
        .map_err(|e| format!("failed to parse registry json: {e}"))
}

async fn read_cache(cache_file: &Path) -> Option<CachedRegistry> {
    let raw = tokio::fs::read(cache_file).await.ok()?;
    match serde_json::from_slice::<CachedRegistry>(&raw) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(
                path = %cache_file.display(),
                error = %e,
                "corrupt registry cache — ignoring"
            );
            None
        }
    }
}

async fn write_cache(cache_file: &Path, cached: &CachedRegistry) {
    if let Some(parent) = cache_file.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        tracing::warn!(
            path = %parent.display(),
            error = %e,
            "failed to create registry cache dir"
        );
        return;
    }

    match serde_json::to_vec_pretty(cached) {
        Ok(bytes) => {
            if let Err(e) = tokio::fs::write(cache_file, bytes).await {
                tracing::warn!(
                    path = %cache_file.display(),
                    error = %e,
                    "failed to write registry cache"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to serialise registry cache"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_config::acp::{RegistryAgent, RegistryDistribution, RegistryNpx};
    use tempfile::TempDir;

    fn make_index() -> RegistryIndex {
        RegistryIndex {
            version: "1.0.0".to_owned(),
            agents: vec![RegistryAgent {
                id: "alpha".to_owned(),
                name: "Alpha".to_owned(),
                version: "0.1.0".to_owned(),
                description: None,
                repository: None,
                website: None,
                authors: Vec::new(),
                license: None,
                icon: None,
                distribution: RegistryDistribution {
                    npx: Some(RegistryNpx {
                        package: "alpha@0.1.0".to_owned(),
                        args: Vec::new(),
                        env: Default::default(),
                    }),
                    uvx: None,
                    binary: Default::default(),
                },
            }],
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_url_precedence() -> Result<(), String> {
        // Env, config, default precedence is covered in a single test to
        // keep the process-global env var manipulation serialized.
        //
        // SAFETY: single-threaded runtime, no other test touches this
        // env var concurrently.
        unsafe { std::env::remove_var(REGISTRY_URL_ENV) };
        assert_eq!(resolve_registry_url(None), DEFAULT_REGISTRY_URL);
        assert_eq!(
            resolve_registry_url(Some("https://cfg.example/registry.json")),
            "https://cfg.example/registry.json"
        );

        unsafe { std::env::set_var(REGISTRY_URL_ENV, "https://env.example/registry.json") };
        assert_eq!(
            resolve_registry_url(Some("https://cfg.example/registry.json")),
            "https://env.example/registry.json"
        );

        unsafe { std::env::remove_var(REGISTRY_URL_ENV) };
        Ok(())
    }

    #[tokio::test]
    async fn cache_roundtrip() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("acp-registry.json");

        let cached = CachedRegistry {
            fetched_at: 1_700_000_000,
            index: make_index(),
        };
        write_cache(&path, &cached).await;

        let loaded = read_cache(&path).await.ok_or("cache should be present")?;
        assert_eq!(loaded.fetched_at, 1_700_000_000);
        assert_eq!(loaded.index.agents.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn merge_preserves_existing_and_adds_new() -> Result<(), String> {
        use bitrouter_config::{AgentConfig, AgentProtocol};

        let mut agents: HashMap<String, AgentConfig> = HashMap::new();
        agents.insert(
            "alpha".to_owned(),
            AgentConfig {
                protocol: AgentProtocol::Acp,
                binary: "user-custom-path".to_owned(),
                args: Vec::new(),
                enabled: true,
                distribution: Vec::new(),
                session: None,
                a2a: None,
            },
        );

        let index = make_index(); // contains only "alpha" → registry entry
        let added = merge_registry_into_agents(&index, &mut agents);

        // "alpha" already existed — user config wins, nothing added.
        assert_eq!(added, 0);
        assert_eq!(agents["alpha"].binary, "user-custom-path");

        // Extend the registry with a brand-new agent.
        let mut extended = index;
        extended.agents.push(RegistryAgent {
            id: "beta".to_owned(),
            name: "Beta".to_owned(),
            version: "0.2.0".to_owned(),
            description: None,
            repository: None,
            website: None,
            authors: Vec::new(),
            license: None,
            icon: None,
            distribution: RegistryDistribution {
                npx: Some(RegistryNpx {
                    package: "beta@0.2.0".to_owned(),
                    args: Vec::new(),
                    env: Default::default(),
                }),
                uvx: None,
                binary: Default::default(),
            },
        });
        let added = merge_registry_into_agents(&extended, &mut agents);
        assert_eq!(added, 1);
        assert!(agents.contains_key("beta"));
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_cache_is_ignored() -> Result<(), String> {
        let dir = TempDir::new().map_err(|e| e.to_string())?;
        let path = dir.path().join("cache.json");
        tokio::fs::write(&path, b"not json")
            .await
            .map_err(|e| e.to_string())?;
        assert!(read_cache(&path).await.is_none());
        Ok(())
    }
}
