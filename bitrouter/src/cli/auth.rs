//! `bitrouter auth` subcommand — interactive provider authentication.
//!
//! Supports API-key-based providers (prompted inline) and OAuth providers
//! (device-code flow).  Both `login` and `refresh` share the same per-provider
//! logic; `refresh` simply filters to already-configured providers.

use crate::auth::oauth::params_from_oauth_config;
use crate::auth::token_store::TokenStore;
use crate::runtime::RuntimePaths;
use bitrouter_config::{AuthConfig, BitrouterConfig, builtin_provider_defs};
use dialoguer::{MultiSelect, Select, theme::ColorfulTheme};

/// Display name and config key for the well-known providers presented during
/// interactive selection.  Matches the order used in `bitrouter init`.
const KNOWN_PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("google", "Google"),
    ("github-copilot", "GitHub Copilot"),
];

// ── login ──────────────────────────────────────────────────────────────

/// Run `bitrouter auth login [provider]`.
///
/// * With a provider argument → authenticate that single provider.
/// * Without → interactive multi-provider selection and onboarding.
pub fn run_login(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    provider: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(name) = provider {
        return auth_single_provider(config, paths, name);
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err(
            "interactive auth requires a terminal. Use `bitrouter auth login <provider>` instead."
                .into(),
        );
    }

    let theme = ColorfulTheme::default();
    let defs = builtin_provider_defs();
    let store = TokenStore::load(&paths.token_store_file);

    // Build the list of selectable providers (known builtins + any extra
    // already configured in the user's config).
    let mut items: Vec<(&str, &str)> = Vec::new();
    for &(key, display) in KNOWN_PROVIDERS {
        items.push((key, display));
    }
    // Add any config-defined providers not already in KNOWN_PROVIDERS
    for name in config.providers.keys() {
        if !items.iter().any(|(k, _)| k == name) {
            items.push((name.as_str(), name.as_str()));
        }
    }

    // Build labels with current auth status
    let labels: Vec<String> = items
        .iter()
        .map(|(key, display)| {
            let status = provider_auth_status(key, config, &defs, &store, &paths.env_file);
            format!("{display} ({status})")
        })
        .collect();

    println!();
    println!("  Provider Authentication");
    println!("  ───────────────────────");
    println!();

    let defaults: Vec<bool> = items.iter().map(|_| false).collect();
    let selections = MultiSelect::with_theme(&theme)
        .with_prompt("Select providers to enable")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;

    if selections.is_empty() {
        println!("  No providers selected.");
        return Ok(());
    }

    let selected: Vec<(&str, &str)> = selections.iter().map(|&i| items[i]).collect();
    let mut configured_count: usize = 0;

    for (key, display) in &selected {
        println!();
        println!("  ─── {display} ───");

        match auth_provider_flow(config, paths, key, &defs) {
            Ok(()) => configured_count += 1,
            Err(e) => eprintln!("  ✗ {key}: {e}"),
        }
    }

    println!();
    if configured_count > 0 {
        println!(
            "  Done! {configured_count} provider{} configured.",
            if configured_count == 1 { "" } else { "s" }
        );
    } else {
        println!("  No providers were configured.");
    }
    println!();

    Ok(())
}

// ── refresh ────────────────────────────────────────────────────────────

/// Run `bitrouter auth refresh [provider]`.
///
/// Shows only currently enabled/configured providers and re-runs auth.
pub fn run_refresh(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    provider: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(name) = provider {
        return auth_single_provider(config, paths, name);
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return Err("interactive auth requires a terminal. \
             Use `bitrouter auth refresh <provider>` instead."
            .into());
    }

    let theme = ColorfulTheme::default();
    let defs = builtin_provider_defs();
    let store = TokenStore::load(&paths.token_store_file);

    // Filter to configured providers
    let configured: Vec<(&str, String)> = config
        .providers
        .keys()
        .map(|name| {
            let display = provider_display_name(name);
            let status = provider_auth_status(name, config, &defs, &store, &paths.env_file);
            (name.as_str(), format!("{display} ({status})"))
        })
        .collect();

    if configured.is_empty() {
        println!("  No configured providers to refresh.");
        println!("  Run `bitrouter auth login` to set up providers.");
        return Ok(());
    }

    let labels: Vec<&str> = configured.iter().map(|(_, l)| l.as_str()).collect();

    println!();
    println!("  Re-authenticate Provider");
    println!("  ────────────────────────");
    println!();

    let selection = Select::with_theme(&theme)
        .with_prompt("Select provider to re-authenticate")
        .items(&labels)
        .default(0)
        .interact()?;

    let (name, _) = &configured[selection];

    println!();
    println!("  ─── {} ───", provider_display_name(name));

    auth_provider_flow(config, paths, name, &defs)?;

    println!();
    Ok(())
}

// ── status ─────────────────────────────────────────────────────────────

/// Run `bitrouter auth status` — show auth state for all configured providers.
pub fn run_status(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
) -> Result<(), Box<dyn std::error::Error>> {
    let defs = builtin_provider_defs();
    let store = TokenStore::load(&paths.token_store_file);

    println!();
    println!("  Provider Auth Status");
    println!("  ────────────────────");

    let mut found = false;
    for name in config.providers.keys() {
        found = true;
        let display = provider_display_name(name);
        let status = provider_auth_status(name, config, &defs, &store, &paths.env_file);
        println!("  {display}: {status}");
    }

    if !found {
        println!("  No providers configured.");
        println!("  Run `bitrouter auth login` to set up providers.");
    }

    println!();
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────

/// Authenticate a single named provider (used by both `login <name>` and
/// `refresh <name>`).
fn auth_single_provider(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let defs = builtin_provider_defs();

    // Accept both configured and builtin providers
    if !config.providers.contains_key(name) && !defs.contains_key(name) {
        return Err(format!(
            "unknown provider '{name}'. Check your bitrouter.yaml or built-in providers."
        )
        .into());
    }

    println!();
    println!("  ─── {} ───", provider_display_name(name));
    auth_provider_flow(config, paths, name, &defs)?;
    println!();
    Ok(())
}

/// Run the appropriate auth flow for a provider (API key prompt or OAuth
/// device code flow) and persist the credential.
fn auth_provider_flow(
    config: &BitrouterConfig,
    paths: &RuntimePaths,
    name: &str,
    defs: &std::collections::HashMap<String, bitrouter_config::BuiltinProvider>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Determine auth type: check config first, then builtins
    let auth = config
        .providers
        .get(name)
        .and_then(|p| p.auth.as_ref())
        .or_else(|| defs.get(name).and_then(|bp| bp.config.auth.as_ref()));

    if let Some(AuthConfig::OAuth {
        client_id,
        scope,
        device_auth_url,
        token_url,
        domain,
        ..
    }) = auth
    {
        // Warn when using the borrowed OpenCode client ID so users understand
        // the GitHub authorization page will show "OpenCode" as the app name.
        const OPENCODE_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
        if name == "github-copilot" && client_id == OPENCODE_CLIENT_ID {
            eprintln!();
            eprintln!(
                "  \x1b[33m⚠ The default GitHub Copilot integration borrows OpenCode's OAuth\x1b[0m"
            );
            eprintln!(
                "  \x1b[33m  client ID. The GitHub authorization page will show \"OpenCode\".\x1b[0m"
            );
            eprintln!(
                "  \x1b[33m  To use your own, set `auth.client_id` in the github-copilot\x1b[0m"
            );
            eprintln!("  \x1b[33m  provider block of your bitrouter.yaml.\x1b[0m");
        }

        // OAuth device code flow
        let params = params_from_oauth_config(
            client_id.as_str(),
            scope.as_deref(),
            device_auth_url.as_deref(),
            token_url.as_deref(),
            domain.as_deref(),
        );
        let mut store = TokenStore::load(&paths.token_store_file);
        crate::auth::oauth::run_device_flow(name, &params, &mut store)?;
        println!("  ✓ Authenticated");
        return Ok(());
    }

    // API key flow
    let theme = ColorfulTheme::default();

    let env_prefix = config
        .providers
        .get(name)
        .and_then(|p| p.env_prefix.as_deref())
        .or_else(|| {
            defs.get(name)
                .and_then(|bp| bp.config.env_prefix.as_deref())
        });
    let fallback_prefix = env_prefix
        .map(str::to_owned)
        .unwrap_or_else(|| name.to_uppercase().replace('-', "_"));
    let key_var = format!("{fallback_prefix}_API_KEY");

    let key: String = dialoguer::Password::with_theme(&theme)
        .with_prompt(format!("Enter API key ({key_var})"))
        .interact()?;

    if key.is_empty() {
        eprintln!(
            "  Warning: empty API key for {}",
            provider_display_name(name)
        );
    }

    bitrouter_config::update_env_key(&paths.env_file, &key_var, &key)?;
    println!("  ✓ Saved");

    Ok(())
}

/// Compute a human-readable auth status string for a provider.
fn provider_auth_status(
    name: &str,
    config: &BitrouterConfig,
    defs: &std::collections::HashMap<String, bitrouter_config::BuiltinProvider>,
    store: &TokenStore,
    env_file: &std::path::Path,
) -> String {
    // Check if this is an OAuth provider
    let auth = config
        .providers
        .get(name)
        .and_then(|p| p.auth.as_ref())
        .or_else(|| defs.get(name).and_then(|bp| bp.config.auth.as_ref()));

    if matches!(auth, Some(AuthConfig::OAuth { .. })) {
        if let Some(token) = store.get(name) {
            if token.expires_at != 0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if now >= token.expires_at {
                    return "✗ token expired".to_owned();
                }
            }
            return "✓ authenticated".to_owned();
        }
        return "✗ not authenticated".to_owned();
    }

    // API key provider — check .env file and environment
    let env_prefix = config
        .providers
        .get(name)
        .and_then(|p| p.env_prefix.as_deref())
        .or_else(|| {
            defs.get(name)
                .and_then(|bp| bp.config.env_prefix.as_deref())
        });
    let resolved_prefix = env_prefix
        .map(str::to_owned)
        .unwrap_or_else(|| name.to_uppercase().replace('-', "_"));
    let key_var = format!("{resolved_prefix}_API_KEY");

    // Check process env first
    if std::env::var(&key_var).ok().is_some_and(|v| !v.is_empty()) {
        return "✓ API key configured".to_owned();
    }

    // Check .env file
    if env_file_has_key(env_file, &key_var) {
        return "✓ API key configured".to_owned();
    }

    // Check if the config has an api_key set directly
    if config
        .providers
        .get(name)
        .and_then(|p| p.api_key.as_ref())
        .is_some_and(|k| !k.is_empty())
    {
        return "✓ API key configured".to_owned();
    }

    "✗ no API key".to_owned()
}

/// Check whether a `.env` file contains a non-empty value for the given var.
fn env_file_has_key(env_file: &std::path::Path, var_name: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(env_file) else {
        return false;
    };
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == var_name
            && !value.trim().is_empty()
        {
            return true;
        }
    }
    false
}

/// Map a provider config key to its display name.
fn provider_display_name(key: &str) -> &str {
    KNOWN_PROVIDERS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, d)| *d)
        .unwrap_or(key)
}
