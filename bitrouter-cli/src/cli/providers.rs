//! `bitrouter providers` subcommand — list and switch between provider modes.
//!
//! Add/remove of individual providers is handled by `bitrouter init`
//! (re-runnable) rather than duplicating the interactive wizard here.

use std::io::{self, Write};

use bitrouter_config::BitrouterConfig;
use serde::Serialize;

use super::OutputFormat;

#[derive(Debug, Serialize)]
pub struct ProviderEntry {
    pub name: String,
    pub api_base: Option<String>,
    pub auth_kind: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderListData {
    pub providers: Vec<ProviderEntry>,
}

pub fn query_list(config: &BitrouterConfig) -> ProviderListData {
    let mut names: Vec<&String> = config.providers.keys().collect();
    names.sort();
    let providers = names
        .into_iter()
        .map(|name| {
            let p = &config.providers[name];
            let auth_kind = if p.api_key.is_some() {
                "api_key".to_owned()
            } else if p.auth.is_some() {
                "oauth".to_owned()
            } else {
                "none".to_owned()
            };
            ProviderEntry {
                name: name.clone(),
                api_base: p.api_base.clone(),
                auth_kind,
            }
        })
        .collect();
    ProviderListData { providers }
}

pub fn render_list_text(data: &ProviderListData, w: &mut impl Write) -> io::Result<()> {
    if data.providers.is_empty() {
        writeln!(w, "  (no providers configured)")?;
        return Ok(());
    }
    eprintln!();
    eprintln!("  Providers");
    eprintln!("  ─────────");
    eprintln!();
    for entry in &data.providers {
        let api_base = entry.api_base.as_deref().unwrap_or("(derives from base)");
        let key_status = match entry.auth_kind.as_str() {
            "api_key" => "\u{2713} key set",
            "oauth" => "\u{2713} OAuth",
            _ => "\u{2717} no credentials",
        };
        writeln!(w, "  {:<20}  {:<40}  {key_status}", entry.name, api_base)?;
    }
    writeln!(w)?;
    Ok(())
}

/// Print every provider in the merged config.
pub fn run_list(
    config: &BitrouterConfig,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = query_list(config);
    match output {
        OutputFormat::Text => render_list_text(&data, &mut io::stdout())?,
        OutputFormat::Json => serde_json::to_writer(io::stdout(), &data)?,
    }
    Ok(())
}

/// Switch between `default` (BitRouter Cloud) and `byok` (Bring Your Own Keys).
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
