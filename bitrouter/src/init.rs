use std::collections::HashMap;

use crate::runtime::RuntimePaths;
use bitrouter_config::{
    CustomProviderInit, InitOptions, builtin_provider_defs, detect_providers_from_env,
};
use dialoguer::{Confirm, Input, theme::ColorfulTheme};

/// Outcome of the init wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// Config was written successfully.
    Configured,
    /// User cancelled or selected no providers.
    Cancelled,
}

/// Display name and config key for each builtin provider.
const PROVIDERS: &[(&str, &str)] = &[
    ("openai", "OpenAI"),
    ("anthropic", "Anthropic"),
    ("google", "Google"),
];

pub fn run_init(paths: &RuntimePaths) -> Result<InitOutcome, Box<dyn std::error::Error>> {
    let theme = ColorfulTheme::default();

    // Check if running in a terminal
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprintln!("error: `bitrouter init` requires an interactive terminal.");
        eprintln!("Edit ~/.bitrouter/bitrouter.yaml and ~/.bitrouter/.env manually.");
        std::process::exit(1);
    }

    println!();
    println!("  BitRouter Setup");
    println!("  ───────────────");
    println!();

    // Check existing config
    let config_exists = paths.config_file.exists()
        && std::fs::read_to_string(&paths.config_file)
            .map(|s| !s.trim_start().starts_with('#'))
            .unwrap_or(false);

    let overwrite = if config_exists {
        Confirm::with_theme(&theme)
            .with_prompt("Existing configuration found. Overwrite?")
            .default(false)
            .interact()?
    } else {
        true
    };

    if config_exists && !overwrite {
        println!("Setup cancelled. Existing configuration preserved.");
        return Ok(InitOutcome::Cancelled);
    }
    let detected = detect_providers_from_env();
    let detected_names: Vec<&str> = detected.iter().map(|d| d.name.as_str()).collect();

    if !detected.is_empty() {
        println!("  Detected API keys in environment:");
        for d in &detected {
            println!(
                "    ✓ {} ({})",
                provider_display_name(&d.name),
                d.api_key_var
            );
        }
        println!();
    }

    // ── Builtin provider selection ──────────────────────────────────
    println!("  Built-in providers");
    println!();
    let mut selected_providers: Vec<&str> = Vec::new();
    for &(key, display) in PROVIDERS {
        let is_detected = detected_names.contains(&key);
        let enable = Confirm::with_theme(&theme)
            .with_prompt(format!("Configure {display}?"))
            .default(is_detected)
            .interact()?;
        if enable {
            selected_providers.push(key);
        }
    }

    // ── Custom provider setup ───────────────────────────────────────
    println!();
    println!("  Custom providers (OpenAI-compatible or Anthropic-compatible)");
    println!();

    let mut custom_providers: Vec<CustomProviderInit> = Vec::new();
    loop {
        let add_custom = Confirm::with_theme(&theme)
            .with_prompt("Add a custom provider?")
            .default(false)
            .interact()?;

        if !add_custom {
            break;
        }

        if let Some(cp) = prompt_custom_provider(&theme)? {
            custom_providers.push(cp);
        }
        println!();
    }

    if selected_providers.is_empty() && custom_providers.is_empty() {
        println!();
        println!("No providers selected. Run `bitrouter init` again anytime.");
        return Ok(InitOutcome::Cancelled);
    }

    // ── Collect API keys for builtin providers ──────────────────────
    let mut api_keys = HashMap::new();
    let defs = builtin_provider_defs();

    if !selected_providers.is_empty() {
        println!();
    }

    for &name in &selected_providers {
        let fallback = name.to_uppercase();
        let prefix = defs
            .get(name)
            .and_then(|bp| bp.config.env_prefix.as_deref())
            .unwrap_or(&fallback);
        let key_var = format!("{prefix}_API_KEY");

        // Check if key exists in environment
        let env_key = std::env::var(&key_var).ok().filter(|v| !v.is_empty());

        let key = if let Some(existing) = &env_key {
            let masked = mask_key(existing);
            let use_existing = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "{} API key detected ({masked}). Use this?",
                    provider_display_name(name)
                ))
                .default(true)
                .interact()?;

            if use_existing {
                existing.clone()
            } else {
                prompt_api_key(&theme, name)?
            }
        } else {
            prompt_api_key(&theme, name)?
        };

        api_keys.insert(name.to_owned(), key);
    }

    // ── Collect API keys for custom providers ───────────────────────
    for cp in &custom_providers {
        let env_key = std::env::var(&cp.env_key_var)
            .ok()
            .filter(|v| !v.is_empty());

        let key = if let Some(existing) = &env_key {
            let masked = mask_key(existing);
            let use_existing = Confirm::with_theme(&theme)
                .with_prompt(format!(
                    "{} API key detected ({masked}). Use this?",
                    cp.name
                ))
                .default(true)
                .interact()?;

            if use_existing {
                existing.clone()
            } else {
                prompt_api_key(&theme, &cp.name)?
            }
        } else {
            prompt_api_key(&theme, &cp.name)?
        };

        api_keys.insert(cp.name.clone(), key);
    }

    // ── Listen address ──────────────────────────────────────────────
    println!();
    let listen_str: String = Input::with_theme(&theme)
        .with_prompt("Listen address")
        .default("127.0.0.1:8787".into())
        .interact_text()?;
    let listen_addr = listen_str.parse().ok();

    // ── Summary ─────────────────────────────────────────────────────
    let all_provider_names: Vec<String> = selected_providers
        .iter()
        .map(|n| provider_display_name(n).to_owned())
        .chain(
            custom_providers
                .iter()
                .map(|cp| format!("{} (derives: {})", cp.name, cp.derives)),
        )
        .collect();

    println!();
    println!("  Configuration summary");
    println!("  ─────────────────────");
    println!("  Home:      {}", paths.home_dir.display());
    println!("  Listen:    {listen_str}");
    println!("  Providers: {}", all_provider_names.join(", "));
    println!("  Config:    {}", paths.config_file.display());
    println!("  Env file:  {}", paths.env_file.display());
    println!();

    let confirm = Confirm::with_theme(&theme)
        .with_prompt("Write configuration?")
        .default(true)
        .interact()?;

    if !confirm {
        println!("Setup cancelled.");
        return Ok(InitOutcome::Cancelled);
    }

    // Write config
    let options = InitOptions {
        providers: selected_providers.iter().map(|s| s.to_string()).collect(),
        api_keys,
        custom_providers,
        listen_addr,
        home_dir: paths.home_dir.clone(),
    };

    let result = bitrouter_config::write_init_config(&options, overwrite)?;

    println!();
    println!("  ✓ Configuration written!");
    println!();
    println!(
        "  {} provider{} configured: {}",
        result.providers_configured.len(),
        if result.providers_configured.len() == 1 {
            ""
        } else {
            "s"
        },
        all_provider_names.join(", ")
    );
    println!();
    println!("  Start the server:");
    println!("    bitrouter start       # foreground");
    println!("    bitrouter start -d    # background daemon");
    println!();

    // Show example curl for the first provider
    let (example_provider, example_model) = if let Some(&name) = selected_providers.first() {
        let model = defs
            .get(name)
            .and_then(|bp| bp.models.first().map(|s| s.as_str()))
            .unwrap_or("model-id");
        (name.to_owned(), model.to_owned())
    } else if let Some(cp) = result.providers_configured.first() {
        (cp.clone(), "model-id".to_owned())
    } else {
        return Ok(InitOutcome::Configured);
    };

    println!("  Test with:");
    println!("    curl http://{listen_str}/v1/chat/completions \\");
    println!("      -H \"Content-Type: application/json\" \\");
    println!(
        "      -d '{{\"model\": \"{example_provider}:{example_model}\", \"messages\": [{{\"role\": \"user\", \"content\": \"Hello!\"}}]}}'"
    );
    println!();

    Ok(InitOutcome::Configured)
}

/// Prompt the user to define a custom provider.
fn prompt_custom_provider(
    theme: &ColorfulTheme,
) -> Result<Option<CustomProviderInit>, Box<dyn std::error::Error>> {
    let name: String = Input::with_theme(theme)
        .with_prompt("Provider name (e.g. openrouter, together, ollama)")
        .interact_text()?;

    let name = name.trim().to_lowercase().replace(' ', "-");
    if name.is_empty() {
        return Ok(None);
    }

    // Check for conflicts with builtins
    let defs = builtin_provider_defs();
    if defs.contains_key(&name) {
        eprintln!(
            "  '{name}' is a built-in provider. Configure it in the built-in section instead."
        );
        return Ok(None);
    }

    let derives: String = Input::with_theme(theme)
        .with_prompt("Compatible API protocol")
        .default("openai".into())
        .validate_with(|input: &String| -> Result<(), String> {
            match input.as_str() {
                "openai" | "anthropic" => Ok(()),
                _ => Err("Must be 'openai' or 'anthropic'".into()),
            }
        })
        .interact_text()?;

    let default_base = match derives.as_str() {
        "anthropic" => "https://api.example.com".to_owned(),
        _ => "https://api.example.com/v1".to_owned(),
    };

    let api_base: String = Input::with_theme(theme)
        .with_prompt("API base URL")
        .default(default_base)
        .interact_text()?;

    let env_prefix = name.to_uppercase().replace('-', "_");
    let env_key_var = format!("{env_prefix}_API_KEY");

    println!("  → {name} (derives: {derives}, base: {api_base}, env: {env_key_var})");

    Ok(Some(CustomProviderInit {
        name,
        derives,
        api_base,
        env_key_var,
    }))
}

fn prompt_api_key(
    theme: &ColorfulTheme,
    provider_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let key: String = dialoguer::Password::with_theme(theme)
        .with_prompt(format!("{} API key", provider_display_name(provider_name)))
        .interact()?;

    if key.is_empty() {
        eprintln!(
            "  Warning: empty API key for {}",
            provider_display_name(provider_name)
        );
    }
    Ok(key)
}

fn provider_display_name(key: &str) -> &str {
    PROVIDERS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, d)| *d)
        .unwrap_or(key)
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_owned();
    }
    let prefix = &key[..4];
    let suffix = &key[key.len() - 4..];
    format!("{prefix}...{suffix}")
}
