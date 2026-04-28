//! `bitrouter providers` subcommand — list and switch between provider modes.
//!
//! Add/remove of individual providers is handled by `bitrouter init`
//! (re-runnable) rather than duplicating the interactive wizard here.

use bitrouter_config::BitrouterConfig;

/// Print every provider in the merged config: name, api_base, and whether
/// an API key is configured.
pub fn run_list(config: &BitrouterConfig) -> Result<(), Box<dyn std::error::Error>> {
    if config.providers.is_empty() {
        println!("  (no providers configured)");
        return Ok(());
    }

    let mut names: Vec<&String> = config.providers.keys().collect();
    names.sort();

    println!();
    println!("  Providers");
    println!("  ─────────");
    println!();

    for name in names {
        let provider = &config.providers[name];
        let api_base = provider
            .api_base
            .as_deref()
            .unwrap_or("(derives from base)");
        let key_status = if provider.api_key.is_some() {
            "\u{2713} key set"
        } else if provider.auth.is_some() {
            "\u{2713} OAuth"
        } else {
            "\u{2717} no credentials"
        };
        println!("  {name:20}  {api_base:40}  {key_status}");
    }
    println!();

    Ok(())
}

/// Switch between `default` (BitRouter Cloud) and `byok` (Bring Your Own Keys).
///
/// This is a *soft* switch — it prints guidance.  The actual change is
/// durable only via `bitrouter init`.  We detect the current mode by
/// looking at whether any non-bitrouter provider has an API key
/// configured.
pub fn run_use(mode: &str, config: &BitrouterConfig) -> Result<(), Box<dyn std::error::Error>> {
    let mode = mode.trim().to_lowercase();

    let byok_configured = config
        .providers
        .iter()
        .any(|(name, p)| name != "bitrouter" && (p.api_key.is_some() || p.auth.is_some()));

    match mode.as_str() {
        "default" => {
            println!();
            println!("  BitRouter Cloud is the default provider — models in");
            println!("  bitrouter-config/providers/models/bitrouter.yaml are billed");
            println!("  through your wallet.");
            if byok_configured {
                println!();
                println!("  You also have BYOK providers configured.  To make BitRouter");
                println!("  Cloud the *sole* provider, run:");
                println!("    bitrouter init");
                println!("  and choose 'BitRouter Cloud' at the models step.");
            }
        }
        "byok" => {
            println!();
            if byok_configured {
                println!("  BYOK providers already configured:");
                for (name, p) in &config.providers {
                    if name == "bitrouter" {
                        continue;
                    }
                    if p.api_key.is_some() || p.auth.is_some() {
                        println!("    \u{2713} {name}");
                    }
                }
            } else {
                println!("  No BYOK providers configured yet.  Run:");
                println!("    bitrouter init");
                println!("  and choose 'Bring Your Own Keys' at the models step.");
            }
        }
        other => {
            return Err(format!("unknown mode '{other}' (expected 'default' or 'byok')").into());
        }
    }
    println!();
    Ok(())
}
