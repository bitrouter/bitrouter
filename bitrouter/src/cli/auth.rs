//! `bitrouter auth` subcommand — manage OAuth authentication for providers.

use crate::auth::oauth::params_from_oauth_config;
use crate::auth::token_store::TokenStore;
use crate::runtime::RuntimePaths;
use bitrouter_config::{AuthConfig, BitrouterConfig};

/// Run `bitrouter auth login <provider>` — perform the OAuth device code flow
/// for the given provider and store the resulting token.
pub fn run_login(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    provider_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = config.providers.get(provider_name).ok_or_else(|| {
        format!(
            "unknown provider '{provider_name}'. Check your bitrouter.yaml or built-in providers."
        )
    })?;

    let (client_id, scope, device_auth_url, token_url) = match &provider.auth {
        Some(AuthConfig::OAuth {
            client_id,
            scope,
            device_auth_url,
            token_url,
            ..
        }) => (
            client_id.as_str(),
            scope.as_deref(),
            device_auth_url.as_deref(),
            token_url.as_deref(),
        ),
        _ => {
            return Err(
                format!("provider '{provider_name}' does not use OAuth authentication").into(),
            );
        }
    };

    let params = params_from_oauth_config(client_id, scope, device_auth_url, token_url);
    let mut store = TokenStore::load(&paths.token_store_file);

    crate::auth::oauth::run_device_flow(provider_name, &params, &mut store)?;

    println!("  Token stored for '{provider_name}'.");
    Ok(())
}

/// Run `bitrouter auth status` — show which OAuth providers have stored tokens.
pub fn run_status(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = TokenStore::load(&paths.token_store_file);
    let mut found = false;

    for (name, provider) in &config.providers {
        if matches!(provider.auth, Some(AuthConfig::OAuth { .. })) {
            found = true;
            let status = if store.get(name).is_some() {
                "✓ authenticated"
            } else {
                "✗ not authenticated"
            };
            println!("  {name}: {status}");
        }
    }

    if !found {
        println!("  No OAuth providers configured.");
    }

    Ok(())
}
